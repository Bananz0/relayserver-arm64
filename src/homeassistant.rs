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

/// One numeric or text entity derived from the relay-sensors snapshot.
struct Reading<'a> {
    suffix: &'a str,
    value: &'a serde_json::Value,
    name: &'a str,
    unit: Option<&'a str>,
    device_class: Option<&'a str>,
    state_class: Option<&'a str>,
    icon: &'a str,
}

/// One binary_sensor derived from a boolean in the relay-sensors snapshot.
struct Flag<'a> {
    suffix: &'a str,
    value: Option<bool>,
    name: &'a str,
    device_class: Option<&'a str>,
    icon_on: &'a str,
    icon_off: &'a str,
}

fn thermal_pressure_name(level: &serde_json::Value) -> serde_json::Value {
    match level.as_u64() {
        Some(0) => serde_json::json!("nominal"),
        Some(1) => serde_json::json!("moderate"),
        Some(2) => serde_json::json!("heavy"),
        Some(3) => serde_json::json!("trapping"),
        Some(4) => serde_json::json!("sleeping"),
        Some(n) => serde_json::json!(n.to_string()),
        None => serde_json::Value::Null,
    }
}

/// Pushes one HA entity per hardware reading from the relay-sensors snapshot.
/// Readings the helper could not obtain on this device (JSON null) are skipped rather than sent as unknown.
async fn push_hardware_sensors(client: &reqwest::Client, base_url: &str, token: &str, entity_id: &str, metrics: &RelayMetrics) {
    let Some(hw) = metrics.get_sensors() else { return };
    let battery = &hw["battery"];
    let thermal = thermal_pressure_name(&hw["thermal_pressure"]);

    let m = Some("measurement");
    let total = Some("total_increasing");
    let readings = [
        Reading { suffix: "battery_temperature", value: &battery["temperature_c"], name: "Battery Temperature", unit: Some("°C"), device_class: Some("temperature"), state_class: m, icon: "mdi:thermometer" },
        Reading { suffix: "battery_voltage", value: &battery["voltage_mv"], name: "Battery Voltage", unit: Some("mV"), device_class: Some("voltage"), state_class: m, icon: "mdi:flash" },
        Reading { suffix: "battery_current", value: &battery["current_ma"], name: "Battery Current", unit: Some("mA"), device_class: Some("current"), state_class: m, icon: "mdi:current-dc" },
        Reading { suffix: "battery_health", value: &battery["health_pct"], name: "Battery Health", unit: Some("%"), device_class: None, state_class: m, icon: "mdi:battery-heart-variant" },
        Reading { suffix: "battery_max_capacity", value: &battery["max_capacity_mah"], name: "Battery Full Charge Capacity", unit: Some("mAh"), device_class: None, state_class: m, icon: "mdi:battery-high" },
        Reading { suffix: "battery_cycles", value: &battery["cycle_count"], name: "Battery Cycles", unit: Some("cycles"), device_class: None, state_class: total, icon: "mdi:battery-sync" },
        Reading { suffix: "charger_rating", value: &battery["adapter_watts"], name: "Charger Rating", unit: Some("W"), device_class: Some("power"), state_class: None, icon: "mdi:power-plug" },
        Reading { suffix: "charger_input_power", value: &hw["charger_input_w"], name: "Charger Input Power", unit: Some("W"), device_class: Some("power"), state_class: m, icon: "mdi:transmission-tower-import" },
        Reading { suffix: "soc_temperature", value: &hw["soc_temp_c"], name: "SoC Temperature", unit: Some("°C"), device_class: Some("temperature"), state_class: m, icon: "mdi:chip" },
        Reading { suffix: "cpu_temperature", value: &hw["cpu_temp_c"], name: "CPU Temperature", unit: Some("°C"), device_class: Some("temperature"), state_class: m, icon: "mdi:cpu-64-bit" },
        Reading { suffix: "storage_temperature", value: &hw["nand_temp_c"], name: "Storage Temperature", unit: Some("°C"), device_class: Some("temperature"), state_class: m, icon: "mdi:harddisk" },
        Reading { suffix: "camera_temperature", value: &hw["camera_temp_c"], name: "Camera Temperature", unit: Some("°C"), device_class: Some("temperature"), state_class: m, icon: "mdi:camera" },
        Reading { suffix: "thermal_state", value: &thermal, name: "Thermal State", unit: None, device_class: None, state_class: None, icon: "mdi:thermometer-alert" },
        Reading { suffix: "illuminance", value: &hw["illuminance_lx"], name: "Ambient Light", unit: Some("lx"), device_class: Some("illuminance"), state_class: m, icon: "mdi:brightness-5" },
        Reading { suffix: "screen_brightness", value: &hw["brightness_pct"], name: "Screen Brightness", unit: Some("%"), device_class: None, state_class: m, icon: "mdi:brightness-6" },
        Reading { suffix: "media_volume", value: &hw["volume_media_pct"], name: "Media Volume", unit: Some("%"), device_class: None, state_class: m, icon: "mdi:volume-high" },
        Reading { suffix: "ringer_volume", value: &hw["volume_ringer_pct"], name: "Ringer Volume", unit: Some("%"), device_class: None, state_class: m, icon: "mdi:bell-ring" },
        Reading { suffix: "wifi_network", value: &hw["wifi_ssid"], name: "Wi-Fi Network", unit: None, device_class: None, state_class: None, icon: "mdi:wifi" },
        Reading { suffix: "wifi_signal", value: &hw["wifi_rssi_dbm"], name: "Wi-Fi Signal", unit: Some("dBm"), device_class: Some("signal_strength"), state_class: m, icon: "mdi:wifi-strength-3" },
        Reading { suffix: "wifi_received", value: &hw["wifi_rx_bytes"], name: "Wi-Fi Received", unit: Some("B"), device_class: Some("data_size"), state_class: total, icon: "mdi:download-network" },
        Reading { suffix: "wifi_sent", value: &hw["wifi_tx_bytes"], name: "Wi-Fi Sent", unit: Some("B"), device_class: Some("data_size"), state_class: total, icon: "mdi:upload-network" },
        Reading { suffix: "storage_free", value: &hw["data_free_mb"], name: "Storage Free", unit: Some("MB"), device_class: Some("data_size"), state_class: m, icon: "mdi:harddisk" },
        Reading { suffix: "system_storage_free", value: &hw["system_free_mb"], name: "System Partition Free", unit: Some("MB"), device_class: Some("data_size"), state_class: m, icon: "mdi:harddisk-remove" },
        Reading { suffix: "memory_free", value: &hw["mem_free_mb"], name: "Free Memory", unit: Some("MB"), device_class: Some("data_size"), state_class: m, icon: "mdi:memory" },
        Reading { suffix: "cpu_load", value: &hw["load_1m"], name: "CPU Load", unit: None, device_class: None, state_class: m, icon: "mdi:gauge" },
        Reading { suffix: "system_uptime", value: &hw["system_uptime_s"], name: "System Uptime", unit: Some("s"), device_class: Some("duration"), state_class: m, icon: "mdi:timer-outline" },
    ];

    for r in readings {
        if r.value.is_null() {
            continue;
        }
        let mut attributes = serde_json::json!({
            "friendly_name": format!("OpenBubbles Relay {}", r.name),
            "icon": r.icon,
        });
        if let Some(unit) = r.unit {
            attributes["unit_of_measurement"] = serde_json::json!(unit);
        }
        if let Some(dc) = r.device_class {
            attributes["device_class"] = serde_json::json!(dc);
        }
        if let Some(sc) = r.state_class {
            attributes["state_class"] = serde_json::json!(sc);
        }
        match r.suffix {
            "wifi_network" => {
                attributes["bssid"] = hw["wifi_bssid"].clone();
                attributes["channel"] = hw["wifi_channel"].clone();
            }
            "storage_free" => attributes["total_mb"] = hw["data_total_mb"].clone(),
            "cpu_load" => {
                attributes["load_5m"] = hw["load_5m"].clone();
                attributes["load_15m"] = hw["load_15m"].clone();
            }
            "battery_max_capacity" => attributes["design_capacity_mah"] = battery["design_capacity_mah"].clone(),
            "charger_rating" => attributes["adapter"] = battery["adapter_name"].clone(),
            _ => {}
        }
        let payload = serde_json::json!({ "state": r.value, "attributes": attributes });
        post_state(client, base_url, token, &format!("{}_{}", entity_id, r.suffix), &payload).await;
    }

    // On/off states as real binary_sensors so they can drive automations directly.
    let charging = battery["is_charging"].as_bool().or_else(|| Some(crate::c::is_charging_rs()));
    let screen_on = hw["screen_on"].as_bool();
    let flags = [
        Flag { suffix: "charging", value: charging, name: "Charging", device_class: Some("battery_charging"), icon_on: "mdi:battery-charging", icon_off: "mdi:battery" },
        Flag { suffix: "plugged_in", value: battery["external_connected"].as_bool(), name: "Plugged In", device_class: Some("plug"), icon_on: "mdi:power-plug", icon_off: "mdi:power-plug-off" },
        Flag { suffix: "screen", value: screen_on, name: "Screen", device_class: None, icon_on: "mdi:cellphone-screenshot", icon_off: "mdi:cellphone-off" },
        // HA's lock device_class means on = unlocked.
        Flag { suffix: "unlocked", value: hw["locked"].as_bool().map(|l| !l), name: "Unlocked", device_class: Some("lock"), icon_on: "mdi:lock-open-variant", icon_off: "mdi:lock" },
        Flag { suffix: "ringer", value: hw["ringer_on"].as_bool(), name: "Ringer", device_class: None, icon_on: "mdi:bell-ring", icon_off: "mdi:bell-off" },
        Flag { suffix: "low_power_mode", value: hw["low_power_mode"].as_bool(), name: "Low Power Mode", device_class: None, icon_on: "mdi:battery-heart-outline", icon_off: "mdi:battery-heart-variant" },
    ];

    let object_id = entity_id.strip_prefix("sensor.").unwrap_or(entity_id);
    for f in flags {
        let Some(on) = f.value else { continue };
        let mut attributes = serde_json::json!({
            "friendly_name": format!("OpenBubbles Relay {}", f.name),
            "icon": if on { f.icon_on } else { f.icon_off },
        });
        if let Some(dc) = f.device_class {
            attributes["device_class"] = serde_json::json!(dc);
        }
        if f.suffix == "charging" {
            attributes["external_connected"] = battery["external_connected"].clone();
            attributes["fully_charged"] = battery["fully_charged"].clone();
        }
        let payload = serde_json::json!({ "state": if on { "on" } else { "off" }, "attributes": attributes });
        post_state(client, base_url, token, &format!("binary_sensor.{}_{}", object_id, f.suffix), &payload).await;
    }
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
