mod c;
mod error;
mod homeassistant;
mod nac;
mod relay;
mod util;

use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose;
use base64::Engine;
use homeassistant::{generate_ha_payload, get_local_ip, push_to_homeassistant, refresh_sensors, HomeAssistantConfig, RelayMetrics};
use nac::generate_validation_data;
use relay::{Relay, RelayResource, RelayState};
use serde::{Deserialize, Serialize};
use tokio::{fs, sync::broadcast};

pub fn base64_encode(data: &[u8]) -> String {
    general_purpose::STANDARD.encode(data)
}

#[derive(Serialize, Deserialize, Clone)]
struct RelayConfig {
    url: String,
    state: Option<RelayState>,
    #[serde(default)]
    homeassistant: Option<HomeAssistantConfig>,
}

impl RelayConfig {
    async fn from_relay(relay: &Relay, ha: Option<HomeAssistantConfig>) -> RelayConfig {
        RelayConfig {
            url: relay.url.lock().await.clone(),
            state: relay.state.lock().await.clone(),
            homeassistant: ha,
        }
    }
}

fn parse_url_encoded(body: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for pair in body.split('&') {
        let mut parts = pair.splitn(2, '=');
        if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
            let key = urlencoding_decode(k);
            let val = urlencoding_decode(v);
            map.insert(key, val);
        }
    }
    map
}

fn urlencoding_decode(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '+' {
            result.push(' ');
        } else if c == '%' {
            let h1 = chars.next().unwrap_or('0');
            let h2 = chars.next().unwrap_or('0');
            if let Ok(b) = u8::from_str_radix(&format!("{}{}", h1, h2), 16) {
                result.push(b as char);
            }
        } else {
            result.push(c);
        }
    }
    result
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
     .replace('<', "&lt;")
     .replace('>', "&gt;")
     .replace('"', "&quot;")
     .replace('\'', "&#39;")
}

fn save_config_atomic(path: &str, content: &str) -> std::io::Result<()> {
    let tmp_path = format!("{}.tmp", path);
    let bak_path = format!("{}.bak", path);
    std::fs::write(&tmp_path, content)?;
    if std::path::Path::new(path).exists() {
        let _ = std::fs::copy(path, &bak_path);
    }
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

#[tokio::main]
async fn main() {
    let config_path = if std::path::Path::new("/var/mobile").exists() {
        "/var/mobile/config.json"
    } else {
        "config.json"
    };

    println!("RelayServer starting. Target config: {}", config_path);

    let mut config: Option<RelayConfig> = None;
    if let Ok(read) = fs::read_to_string(config_path).await {
        if let Ok(item) = serde_json::from_str::<RelayConfig>(&read) {
            println!("Loaded existing configuration.");
            config = Some(item);
        }
    }

    // Initialize metrics and push trigger channel
    let (ha_tx, mut ha_rx) = tokio::sync::mpsc::unbounded_channel();
    let metrics = RelayMetrics::new(ha_tx);
    if let Some(ref c) = config {
        if let Some(ref s) = c.state {
            metrics.set_code(&s.code);
        }
    }

    // Ensure single instance: bind HTTP port 8080 first
    let listener = match tokio::net::TcpListener::bind("0.0.0.0:8080").await {
        Ok(l) => {
            println!("Relay Web UI running at http://0.0.0.0:8080");
            l
        }
        Err(e) => {
            eprintln!("Failed to bind HTTP listener on port 8080 (another instance is running): {}", e);
            std::process::exit(0);
        }
    };

    let url = config.as_ref().map(|i| i.url.clone()).unwrap_or_else(|| "wss://registration-relay.beeper.com/api/v1/provider".to_string());
    let relay = RelayResource::new(url, config.as_ref().and_then(|i| i.state.clone()), metrics.clone());

    // Home Assistant config shared state (configured via /var/mobile/config.json or Web UI)
    let ha_config_shared = Arc::new(tokio::sync::RwLock::new(config.as_ref().and_then(|i| i.homeassistant.clone())));

    // Background Home Assistant reporting loop
    let ha_loop_metrics = metrics.clone();
    let ha_loop_config = ha_config_shared.clone();
    let ha_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .use_rustls_tls()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let loop_client = ha_client.clone();
    tokio::spawn(async move {
        // Initial delay
        tokio::time::sleep(Duration::from_secs(3)).await;
        loop {
            refresh_sensors(&ha_loop_metrics).await;
            let ha_opt = ha_loop_config.read().await.clone();
            if let Some(ref ha_cfg) = ha_opt {
                println!("[HomeAssistant] Reporting status to {}...", ha_cfg.url);
                match push_to_homeassistant(&loop_client, ha_cfg, &ha_loop_metrics).await {
                    Ok(msg) => println!("[HomeAssistant] {}", msg),
                    Err(e) => eprintln!("[HomeAssistant] Push error: {}", e),
                }
            }
            let interval = ha_opt.as_ref().map(|c| c.interval_secs).unwrap_or(60).max(10);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(interval)) => {},
                _ = ha_rx.recv() => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    while ha_rx.try_recv().is_ok() {}
                }
            }
        }
    });

    // Spawn HTTP status server on listener
    let http_relay = relay.clone();
    let http_metrics = metrics.clone();
    let http_ha_config = ha_config_shared.clone();
    let http_config_path = config_path.to_string();
    let http_client = ha_client.clone();

    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => continue,
            };

            let relay = http_relay.clone();
            let metrics = http_metrics.clone();
            let ha_config = http_ha_config.clone();
            let config_path = http_config_path.clone();
            let client = http_client.clone();

            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 8192];
                let n = match tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf)).await {
                    Ok(Ok(n)) if n > 0 => n,
                    _ => return,
                };
                let req_str = String::from_utf8_lossy(&buf[..n]);
                let first_line = req_str.lines().next().unwrap_or("");
                let mut parts = first_line.split_whitespace();
                let method = parts.next().unwrap_or("GET");
                let full_path = parts.next().unwrap_or("/");

                let mut path_and_query = full_path.splitn(2, '?');
                let path = path_and_query.next().unwrap_or("/");
                let query = path_and_query.next().unwrap_or("");

                let ha_current = ha_config.read().await.clone();
                let state = RelayConfig::from_relay(&relay, ha_current.clone()).await;
                let code_val = match &state.state {
                    Some(s) => s.code.clone(),
                    None => "(Connecting...)".to_string(),
                };

                let (status, content_type, body) = if path == "/code" {
                    ("200 OK", "text/plain; charset=utf-8".to_string(), code_val)
                } else if path == "/json" {
                    let mut safe_state = state.clone();
                    if let Some(ref mut ha) = safe_state.homeassistant {
                        ha.token = "********".to_string();
                    }
                    if let Some(ref mut s) = safe_state.state {
                        s.secret = "********".to_string();
                    }
                    ("200 OK", "application/json".to_string(), serde_json::to_string_pretty(&safe_state).unwrap_or_default())
                } else if path == "/ha" || path == "/metrics" {
                    let ha_payload = generate_ha_payload(&metrics);
                    ("200 OK", "application/json".to_string(), serde_json::to_string_pretty(&ha_payload).unwrap_or_default())
                } else if path == "/ha/test" {
                    let res = if let Some(ref cfg) = ha_current {
                        match push_to_homeassistant(&client, cfg, &metrics).await {
                            Ok(msg) => format!("SUCCESS: {}", msg),
                            Err(e) => format!("FAILED: {}", e),
                        }
                    } else {
                        "Home Assistant is not configured yet. Configure it on the dashboard.".to_string()
                    };
                    ("200 OK", "text/plain; charset=utf-8".to_string(), res)
                } else if path == "/ha/save" {
                    // Extract parameters from query or body
                    let mut params = parse_url_encoded(query);
                    if method == "POST" {
                        if let Some(body_start) = req_str.split("\r\n\r\n").nth(1) {
                            let body_params = parse_url_encoded(body_start);
                            for (k, v) in body_params {
                                params.insert(k, v);
                            }
                        }
                    }

                    let ha_url = params.get("url").cloned().unwrap_or_default().trim().to_string();
                    let ha_token = params.get("token").cloned().unwrap_or_default().trim().to_string();
                    let ha_entity = params.get("entity_id").cloned().unwrap_or_else(|| "sensor.openbubbles_relay".to_string()).trim().to_string();

                    if !ha_url.is_empty() && !ha_token.is_empty() {
                        let new_ha = HomeAssistantConfig {
                            url: ha_url,
                            token: ha_token,
                            entity_id: if ha_entity.is_empty() { "sensor.openbubbles_relay".to_string() } else { ha_entity },
                            interval_secs: 60,
                        };

                        *ha_config.write().await = Some(new_ha.clone());

                        // Persist to config.json atomically
                        let updated_state = RelayConfig::from_relay(&relay, Some(new_ha.clone())).await;
                        if let Ok(json_str) = serde_json::to_string_pretty(&updated_state) {
                            let _ = save_config_atomic(&config_path, &json_str);
                        }

                        // Immediate test push
                        let push_res = push_to_homeassistant(&client, &new_ha, &metrics).await;
                        let push_msg = match push_res {
                            Ok(m) => format!("Saved & tested: {}", m),
                            Err(e) => format!("Saved, but initial push failed: {}", e),
                        };

                        ("200 OK", "text/html; charset=utf-8".to_string(), format!(
                            "<!DOCTYPE html><html><head><meta charset='utf-8'><meta http-equiv='refresh' content='2;url=/'><style>body{{font-family:sans-serif;background:#121212;color:#fff;text-align:center;padding:40px;}}a{{color:#30d158;}}</style></head><body><h2>{}</h2><p>Redirecting back to dashboard in 2s... <a href='/'>Go back</a></p></body></html>",
                            html_escape(&push_msg)
                        ))
                    } else {
                        ("400 Bad Request", "text/plain; charset=utf-8".to_string(), "Missing url or token parameter.".to_string())
                    }
                } else if path == "/log" {
                    let log_data = match tokio::fs::read_to_string("/var/mobile/relayserver.log").await {
                        Ok(s) => s,
                        Err(_) => tokio::fs::read_to_string("relayserver.log").await.unwrap_or_else(|_| "(Log empty or not found)".to_string()),
                    };
                    ("200 OK", "text/plain; charset=utf-8".to_string(), log_data)
                } else if path == "/err" {
                    let err_data = match tokio::fs::read_to_string("/var/mobile/relayserver.err").await {
                        Ok(s) => s,
                        Err(_) => tokio::fs::read_to_string("relayserver.err").await.unwrap_or_else(|_| "(Error log empty or not found)".to_string()),
                    };
                    ("200 OK", "text/plain; charset=utf-8".to_string(), err_data)
                } else if path == "/test-diag" || path == "/test-nac" || path == "/test-validation" {
                    println!("[HTTP] Manual test diagnostics requested via browser!");
                    let result_str = match generate_validation_data().await {
                        Ok(data) => format!("SUCCESS! Validation data generated: {} bytes\n\nBase64:\n{}\n", data.len(), base64_encode(&data)),
                        Err(e) => format!("FAILED: {:?}\n\nCheck /log for full step-by-step trace.\n", e),
                    };
                    ("200 OK", "text/plain; charset=utf-8".to_string(), result_str)
                } else if path == "/restart" {
                    println!("[HTTP] Restart command received. Terminating process so launchd can restart it clean...");
                    tokio::spawn(async {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        std::process::exit(0);
                    });
                    ("200 OK", "text/plain; charset=utf-8".to_string(), "Restarting RelayServer process...".to_string())
                } else {
                    let local_ip = get_local_ip();
                    let bat = c::get_battery_level_rs().map(|l| format!("{}%", l)).unwrap_or_else(|| "N/A".to_string());
                    let charging = if c::is_charging_rs() { " ⚡ Charging" } else { "" };
                    let val_count = metrics.validations_completed.load(std::sync::atomic::Ordering::Relaxed);
                    let ha_status = metrics.get_push_status().unwrap_or_else(|| {
                        if ha_current.is_some() { "Configured (awaiting first push)".to_string() } else { "Not configured".to_string() }
                    });
                    let ha_url_val = ha_current.as_ref().map(|c| c.url.clone()).unwrap_or_default();
                    let ha_entity_val = ha_current.as_ref().map(|c| c.entity_id.clone()).unwrap_or_else(|| "sensor.openbubbles_relay".to_string());
                    let device_name = {
                        let prod = c::mg_copy_answer_rs("ProductType");
                        if !prod.is_empty() { prod } else { "iOS Device".to_string() }
                    };
                    let ios_ver = {
                        let v = c::mg_copy_answer_rs("ProductVersion");
                        if !v.is_empty() { v } else { "10.3.3".to_string() }
                    };
                    let conn_badge = if metrics.is_connected() {
                        "<b style='color:#30d158;'>Online</b>"
                    } else {
                        "<b style='color:#ff9f0a;'>Connecting...</b>"
                    };

                    let html = format!(
                        "<!DOCTYPE html><html><head><meta charset='utf-8'><meta name='viewport' content='width=device-width, initial-scale=1'><title>OpenBubbles Relay</title><style>body {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; background: #121212; color: #fff; text-align: center; padding: 20px; margin: 0; }} .card {{ background: #1e1e1e; border-radius: 16px; padding: 20px; max-width: 520px; margin: 16px auto; box-shadow: 0 8px 24px rgba(0,0,0,0.5); text-align: left; }} .label {{ font-size: 13px; text-transform: uppercase; color: #888; letter-spacing: 1px; font-weight: bold; margin-bottom: 8px; }} .code {{ font-family: monospace; font-size: 28px; font-weight: bold; color: #30d158; background: #000; padding: 12px; border-radius: 10px; margin: 12px 0; text-align: center; word-break: break-all; user-select: all; }} .badge-row {{ display: flex; flex-wrap: wrap; gap: 8px; margin: 12px 0; }} .badge {{ background: #2c2c2e; padding: 6px 12px; border-radius: 8px; font-size: 12px; color: #ccc; }} .badge b {{ color: #fff; }} .btn {{ display: inline-block; padding: 10px 14px; margin: 4px 2px; background: #2c2c2e; color: #0a84ff; text-decoration: none; border-radius: 8px; font-weight: 600; font-size: 13px; text-align: center; border: none; cursor: pointer; }} .btn:hover {{ background: #3a3a3c; }} .btn-green {{ background: #30d158; color: #000; font-weight: bold; }} .btn-green:hover {{ background: #28b84d; }} input[type=text], input[type=password] {{ width: 100%; box-sizing: border-box; background: #000; border: 1px solid #333; color: #fff; padding: 10px; border-radius: 8px; font-size: 14px; margin: 6px 0 12px 0; }}</style></head><body><div class='card'><div class='label'>OpenBubbles Relay Code</div><div class='code'>{}</div><div class='badge-row'><div class='badge'>Device: <b>{} (iOS {})</b></div><div class='badge'>IP: <b>{}</b></div><div class='badge'>Battery: <b>{}{}</b></div><div class='badge'>Validations: <b>{} completed</b></div><div class='badge'>Relay: {}</div></div><p style='font-size:12px;color:#888;margin:6px 0;'>Saved to: <code>/var/mobile/config.json</code></p></div><div class='card'><div class='label'>Home Assistant Integration</div><p style='font-size:13px;color:#aaa;margin-top:0;'>Your phone automatically reports its IP, battery, relay code, and health to Home Assistant every 60s so you can plug it in anywhere and forget about it.</p><div class='badge' style='margin-bottom:12px;'>Status: <b>{}</b></div><form action='/ha/save' method='POST'><label style='font-size:12px;color:#aaa;'>Home Assistant URL</label><input type='text' name='url' placeholder='http://homeassistant.local:8123 or http://192.168.0.x:8123' value='{}' required><label style='font-size:12px;color:#aaa;'>Long-Lived Access Token (from HA Profile &rarr; Long-Lived Access Tokens)</label><input type='password' name='token' placeholder='eyJhbGciOi...' required><label style='font-size:12px;color:#aaa;'>Entity ID</label><input type='text' name='entity_id' value='{}' required><button type='submit' class='btn btn-green' style='width:100%;padding:12px;margin:8px 0;'>Save &amp; Send Test Update to Home Assistant</button></form></div><div class='card'><div class='label'>Tools &amp; Diagnostics</div><div style='margin-top:8px;'><a class='btn' href='/test-diag' target='_blank'>&rarr; Test System Diagnostics</a><a class='btn' href='/ha/test' target='_blank'>Test HA Push</a><a class='btn' href='/ha' target='_blank'>HA JSON API (/ha)</a><a class='btn' href='/log' target='_blank'>Live Log</a><a class='btn' href='/err' target='_blank'>Error Log</a><a class='btn' href='/json' target='_blank'>Config JSON</a><a class='btn' href='/restart'>Restart Daemon</a></div></div></body></html>",
                        html_escape(&code_val),
                        html_escape(&device_name),
                        html_escape(&ios_ver),
                        html_escape(&local_ip),
                        html_escape(&bat),
                        charging,
                        val_count,
                        conn_badge,
                        html_escape(&ha_status),
                        html_escape(&ha_url_val),
                        html_escape(&ha_entity_val)
                    );
                    ("200 OK", "text/html; charset=utf-8".to_string(), html)
                };

                let resp = format!(
                    "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status, content_type, body.len(), body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        }
    });

    let mut to_refresh = relay.generated_signal.subscribe();
    let reconn_conn = Arc::downgrade(&relay);
    let reconn_ha_config = ha_config_shared.clone();

    tokio::spawn(async move {
        loop {
            match to_refresh.recv().await {
                Ok(()) => {
                    let Some(conn) = reconn_conn.upgrade() else { break };
                    let ha_curr = reconn_ha_config.read().await.clone();
                    let state = RelayConfig::from_relay(&conn, ha_curr).await;
                    let json_data = serde_json::to_string_pretty(&state).unwrap_or_default();
                    println!("Relay updated! Writing to {}", config_path);
                    if let Err(e) = save_config_atomic(config_path, &json_data) {
                        eprintln!("Failed to write to {}: {}", config_path, e);
                    }
                },
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    std::future::pending::<()>().await;
}
