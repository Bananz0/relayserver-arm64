use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HomeAssistantConfig {
    pub url: String,
    pub token: String,
    #[serde(default = "default_entity_id")]
    pub entity_id: String,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
}

fn default_entity_id() -> String {
    "sensor.openbubbles_relay".to_string()
}

fn default_interval() -> u64 {
    60
}

pub struct RelayMetrics {
    pub start_time: Instant,
    pub validations_completed: AtomicU64,
    pub last_validation_time: RwLock<Option<String>>,
    pub last_error: RwLock<Option<String>>,
    pub ha_last_push_status: RwLock<Option<String>>,
    pub relay_code: RwLock<String>,
    pub connected: AtomicBool,
    pub ha_trigger: tokio::sync::mpsc::UnboundedSender<()>,
    /// Last JSON snapshot from the relay-sensors helper (None until the first successful read).
    pub sensors: RwLock<Option<serde_json::Value>>,
}

impl RelayMetrics {
    pub fn new(ha_trigger: tokio::sync::mpsc::UnboundedSender<()>) -> Arc<Self> {
        Arc::new(Self {
            start_time: Instant::now(),
            validations_completed: AtomicU64::new(0),
            last_validation_time: RwLock::new(None),
            last_error: RwLock::new(None),
            ha_last_push_status: RwLock::new(None),
            relay_code: RwLock::new("(Connecting...)".to_string()),
            connected: AtomicBool::new(false),
            ha_trigger,
            sensors: RwLock::new(None),
        })
    }

    pub fn get_sensors(&self) -> Option<serde_json::Value> {
        self.sensors.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn set_code(&self, code: &str) {
        *self.relay_code.write().unwrap_or_else(|p| p.into_inner()) = code.to_string();
        let _ = self.ha_trigger.send(());
    }

    pub fn get_code(&self) -> String {
        self.relay_code.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn set_connected(&self, conn: bool) {
        self.connected.store(conn, Ordering::SeqCst);
        let _ = self.ha_trigger.send(());
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn record_validation(&self) {
        let count = self.validations_completed.fetch_add(1, Ordering::SeqCst) + 1;
        let now_str = format!("Validation #{} at +{:.0}s", count, self.start_time.elapsed().as_secs());
        *self.last_validation_time.write().unwrap_or_else(|p| p.into_inner()) = Some(now_str);
        let _ = self.ha_trigger.send(());
    }

    pub fn record_error(&self, err: &str) {
        *self.last_error.write().unwrap_or_else(|p| p.into_inner()) = Some(err.to_string());
        let _ = self.ha_trigger.send(());
    }

    pub fn set_push_status(&self, status: &str) {
        *self.ha_last_push_status.write().unwrap_or_else(|p| p.into_inner()) = Some(status.to_string());
    }

    pub fn get_push_status(&self) -> Option<String> {
        self.ha_last_push_status.read().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

pub fn get_local_ip() -> String {
    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = socket.local_addr() {
                return addr.ip().to_string();
            }
        }
    }
    "127.0.0.1".to_string()
}

const SENSORS_HELPER: &str = "/var/mobile/relay-sensors";

/// Runs the relay-sensors helper and caches its JSON output. The helper is a separate
/// process so a crash in the private IOKit/IOHID calls can never take the relay down.
pub async fn refresh_sensors(metrics: &RelayMetrics) {
    if !std::path::Path::new(SENSORS_HELPER).exists() {
        return;
    }
    let run = tokio::process::Command::new(SENSORS_HELPER).kill_on_drop(true).output();
    let parsed = match tokio::time::timeout(std::time::Duration::from_secs(5), run).await {
        Ok(Ok(out)) if out.status.success() => serde_json::from_slice::<serde_json::Value>(&out.stdout).ok(),
        Ok(Ok(out)) => {
            eprintln!("[Sensors] helper exited with {}", out.status);
            None
        }
        Ok(Err(e)) => {
            eprintln!("[Sensors] failed to spawn helper: {e}");
            None
        }
        Err(_) => {
            eprintln!("[Sensors] helper timed out");
            None
        }
    };
    if let Some(v) = parsed {
        *metrics.sensors.write().unwrap_or_else(|p| p.into_inner()) = Some(v);
    }
}

pub fn generate_ha_payload(metrics: &RelayMetrics) -> serde_json::Value {
    let local_ip = get_local_ip();
    let battery = crate::c::get_battery_level_rs();
    let charging = crate::c::is_charging_rs();
    let uptime = metrics.start_time.elapsed().as_secs();
    let code = metrics.get_code();
    let is_online = metrics.is_connected();
    let validations = metrics.validations_completed.load(Ordering::Relaxed);
    let last_val = metrics.last_validation_time.read().unwrap_or_else(|p| p.into_inner()).clone();
    let last_err = metrics.last_error.read().unwrap_or_else(|p| p.into_inner()).clone();

    let device_model = {
        let prod_type = crate::c::mg_copy_answer_rs("ProductType");
        if !prod_type.is_empty() {
            prod_type
        } else if let Ok(uts) = nix::sys::utsname::uname() {
            uts.machine().to_str().unwrap_or("iPhone").to_string()
        } else {
            "iPhone".to_string()
        }
    };
    let ios_version = {
        let ver = crate::c::mg_copy_answer_rs("ProductVersion");
        if !ver.is_empty() { ver } else { "iOS".to_string() }
    };

    serde_json::json!({
        "state": if is_online { "Online" } else { "Connecting" },
        "attributes": {
            "friendly_name": "OpenBubbles Relay",
            "icon": "mdi:cellphone-wireless",
            "relay_code": code,
            "status": if is_online { "Connected" } else { "Connecting" },
            "nac_health": if validations > 0 { "Healthy (Validations active)" } else { "Ready" },
            "validations_completed": validations,
            "last_validation": last_val.unwrap_or_else(|| "None yet".to_string()),
            "battery_level": battery,
            "battery_charging": charging,
            "device_model": device_model,
            "ios_version": ios_version,
            "ip_address": local_ip,
            "web_ui": format!("http://{}:8080", local_ip),
            "uptime_seconds": uptime,
            "uptime_human": format!("{:.1}h", uptime as f64 / 3600.0),
            "last_error": last_err,
            "hardware": metrics.get_sensors(),
        }
    })
}

async fn post_state(client: &reqwest::Client, base_url: &str, token: &str, entity_id: &str, payload: &serde_json::Value) {
    let Ok(body) = serde_json::to_string(payload) else { return };
    let _ = client
        .post(format!("{}/api/states/{}", base_url, entity_id))
        .header("Authorization", format!("Bearer {}", token))
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await;
}

/// Pushes one HA entity per hardware reading from the relay-sensors snapshot.
/// Readings the helper could not obtain (JSON null) are skipped rather than sent as unknown.
async fn push_hardware_sensors(client: &reqwest::Client, base_url: &str, token: &str, entity_id: &str, metrics: &RelayMetrics) {
    let Some(hw) = metrics.get_sensors() else { return };
    let battery = &hw["battery"];

    // (suffix, value, friendly name, unit, device_class, icon)
    let readings: [(&str, &serde_json::Value, &str, &str, Option<&str>, &str); 8] = [
        ("battery_temperature", &battery["temperature_c"], "Battery Temperature", "°C", Some("temperature"), "mdi:thermometer"),
        ("battery_voltage", &battery["voltage_mv"], "Battery Voltage", "mV", Some("voltage"), "mdi:flash"),
        ("battery_current", &battery["current_ma"], "Battery Current", "mA", Some("current"), "mdi:current-dc"),
        ("battery_health", &battery["health_pct"], "Battery Health", "%", None, "mdi:battery-heart-variant"),
        ("battery_cycles", &battery["cycle_count"], "Battery Cycles", "cycles", None, "mdi:battery-sync"),
        ("illuminance", &hw["illuminance_lx"], "Ambient Light", "lx", Some("illuminance"), "mdi:brightness-5"),
        ("memory_free", &hw["mem_free_mb"], "Free Memory", "MB", Some("data_size"), "mdi:memory"),
        ("system_uptime", &hw["system_uptime_s"], "System Uptime", "s", Some("duration"), "mdi:timer-outline"),
    ];

    for (suffix, value, name, unit, device_class, icon) in readings {
        if value.is_null() {
            continue;
        }
        let mut attributes = serde_json::json!({
            "friendly_name": format!("OpenBubbles Relay {}", name),
            "unit_of_measurement": unit,
            "state_class": "measurement",
            "icon": icon,
        });
        if let Some(dc) = device_class {
            attributes["device_class"] = serde_json::json!(dc);
        }
        let payload = serde_json::json!({ "state": value, "attributes": attributes });
        post_state(client, base_url, token, &format!("{}_{}", entity_id, suffix), &payload).await;
    }

    // Charging as a real binary_sensor so it can drive automations directly.
    let charging = battery["is_charging"].as_bool().unwrap_or_else(crate::c::is_charging_rs);
    let object_id = entity_id.strip_prefix("sensor.").unwrap_or(entity_id);
    let payload = serde_json::json!({
        "state": if charging { "on" } else { "off" },
        "attributes": {
            "friendly_name": "OpenBubbles Relay Charging",
            "device_class": "battery_charging",
            "external_connected": battery["external_connected"],
            "fully_charged": battery["fully_charged"],
        }
    });
    post_state(client, base_url, token, &format!("binary_sensor.{}_charging", object_id), &payload).await;
}

pub async fn push_to_homeassistant(
    client: &reqwest::Client,
    config: &HomeAssistantConfig,
    metrics: &RelayMetrics,
) -> Result<String, String> {
    let base_url = config.url.trim_end_matches('/');
    let clean_entity: String = config.entity_id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.')
        .collect();
    let entity_id = if clean_entity.is_empty() { "sensor.openbubbles_relay" } else { &clean_entity };
    let endpoint = format!("{}/api/states/{}", base_url, entity_id);
    let payload = generate_ha_payload(metrics);
    let body_str = serde_json::to_string(&payload).map_err(|e| format!("JSON serialize error: {e}"))?;
    let req = client
        .post(&endpoint)
        .header("Authorization", format!("Bearer {}", config.token.trim()))
        .header("Content-Type", "application/json")
        .body(body_str);

    let resp = req.send().await.map_err(|e| format!("HTTP request error: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        let code = metrics.get_code();
        let local_ip = get_local_ip();

        // 1. Dedicated Code Sensor (so you can view/copy the pairing code directly from HA dashboard or notifications)
        let code_endpoint = format!("{}/api/states/{}_code", base_url, config.entity_id);
        let code_payload = serde_json::json!({
            "state": code,
            "attributes": {
                "friendly_name": "OpenBubbles Relay Code",
                "icon": "mdi:numeric",
                "phone_ip": local_ip,
                "web_ui": format!("http://{}:8080", local_ip),
                "is_connected": metrics.is_connected(),
            }
        });
        if let Ok(code_str) = serde_json::to_string(&code_payload) {
            let _ = client
                .post(&code_endpoint)
                .header("Authorization", format!("Bearer {}", config.token.trim()))
                .header("Content-Type", "application/json")
                .body(code_str)
                .send()
                .await;
        }

        // 2. Battery Sensor
        if let Some(level) = crate::c::get_battery_level_rs() {
            let bat_endpoint = format!("{}/api/states/{}_battery", base_url, config.entity_id);
            let charging = crate::c::is_charging_rs();
            let bat_payload = serde_json::json!({
                "state": level,
                "attributes": {
                    "friendly_name": "OpenBubbles Relay Battery",
                    "device_class": "battery",
                    "unit_of_measurement": "%",
                    "battery_charging": charging,
                    "icon": if charging { "mdi:battery-charging" } else { "mdi:battery" }
                }
            });
            if let Ok(bat_str) = serde_json::to_string(&bat_payload) {
                let _ = client
                    .post(&bat_endpoint)
                    .header("Authorization", format!("Bearer {}", config.token.trim()))
                    .header("Content-Type", "application/json")
                    .body(bat_str)
                    .send()
                    .await;
            }
        }

        // 3. Hardware sensors from the relay-sensors helper
        push_hardware_sensors(client, base_url, config.token.trim(), entity_id, metrics).await;

        let msg = format!("Success (HTTP {status})");
        metrics.set_push_status(&msg);
        Ok(msg)
    } else {
        let text = resp.text().await.unwrap_or_default();
        let msg = format!("HA Error HTTP {status}: {text}");
        metrics.set_push_status(&msg);
        Err(msg)
    }
}
