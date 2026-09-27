use std::{sync::Arc, time::Duration};

use backon::ExponentialBuilder;
use futures::{SinkExt, StreamExt};
use nix::sys::utsname::uname;
use serde::{Deserialize, Serialize};
use tokio::{net::TcpStream, select, sync::Mutex, task::JoinHandle, time::{self, Instant}};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

use crate::{base64_encode, c::mg_copy_answer_rs, error::RelayError, homeassistant::RelayMetrics, nac::generate_validation_data, util::{Resource, ResourceManager}};


#[derive(Deserialize, Serialize, Clone)]
pub struct RelayState {
    pub code: String,
    pub secret: String,
}

pub struct RelayResource {
    pub url: Mutex<String>,
    pub state: Mutex<Option<RelayState>>,
    pub metrics: Arc<RelayMetrics>,
}

#[derive(Serialize, Deserialize)]
pub struct RelayVersions {
    hardware_version: String,
    software_name: String,
    software_version: String,
    software_build_id: String,
    unique_device_id: String,
    serial_number: String,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum CommandData {
    Code {
        #[serde(flatten)]
        code: RelayState
    },
    Versions {
        versions: RelayVersions
    },
    ValidationData {
        data: String,
    },
    Empty {},
}

#[derive(Deserialize, Serialize)]
struct RelayCommand {
    command: String,
    id: Option<u64>,
    data: Option<CommandData>,
}

impl RelayCommand {
    fn to_message(self) -> Message {
        Message::Text(serde_json::to_string(&self).unwrap_or_default())
    }

    fn respond(&self, data: CommandData) -> Message {
        RelayCommand {
            command: "response".to_string(),
            id: self.id,
            data: Some(data)
        }.to_message()
    }
}

pub type Relay = Arc<ResourceManager<RelayResource>>;

impl Resource for RelayResource {
    async fn generate(self: &Arc<Self>) -> Result<JoinHandle<()>, RelayError> {
        let (mut ws_stream, _) = connect_async(&*self.url.lock().await).await?;

        let mut state = self.state.lock().await;
        
        let mapped = state.clone().map(|i| CommandData::Code { code: i }).unwrap_or(CommandData::Empty {  });

        ws_stream.send(RelayCommand { id: None, command: "register".to_string(), data: Some(mapped)}.to_message()).await?;

        let next_msg = ws_stream.next().await.ok_or_else(|| RelayError::ResourcePanic("WebSocket stream closed prematurely".to_string()))??;
        let item: RelayCommand = serde_json::from_str(&next_msg.into_text()?)?;
        let Some(CommandData::Code { code }) = item.data else {
            return Err(RelayError::ResourcePanic("Invalid registration response from server".to_string()));
        };

        println!("Connected with code {}", code.code);

        self.metrics.set_code(&code.code);
        self.metrics.set_connected(true);

        *state = Some(code);

        let metrics = self.metrics.clone();
        Ok(tokio::spawn(async move {
            match RelayResource::poll(ws_stream, metrics.clone()).await {
                Ok(_) => {
                    println!("[Relay] WebSocket connection closed normally. Waiting 3s before reconnecting...");
                },
                Err(err) => {
                    println!("[Relay] WebSocket error: {err}. Waiting 3s before reconnecting...");
                }
            }
            metrics.set_connected(false);
            tokio::time::sleep(Duration::from_secs(3)).await;
        }))
    }
}

impl RelayResource {
    async fn poll(mut ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>, metrics: Arc<RelayMetrics>) -> Result<(), RelayError> {
        let ping_interval = Duration::from_secs(60);
        let max_idle = Duration::from_secs(180);
        let mut last_ping = Instant::now();
        let mut last_activity = Instant::now();
        loop {
            select! {
                msg = ws_stream.next() => {
                    let Some(msg) = msg else {
                        println!("[Relay] WebSocket stream ended by server. Reconnecting...");
                        break;
                    };
                    let msg = match msg {
                        Ok(Message::Text(msg)) => {
                            last_activity = Instant::now();
                            msg
                        },
                        Ok(Message::Ping(_) | Message::Pong(_)) => {
                            last_activity = Instant::now();
                            continue;
                        },
                        Ok(Message::Close(_)) => {
                            println!("[Relay] Server sent WebSocket Close frame.");
                            break;
                        },
                        Ok(_) => continue,
                        Err(e) => {
                            println!("[Relay] WebSocket stream error: {e}");
                            break;
                        }
                    };
                    
                    let command: RelayCommand = match serde_json::from_str(&msg) {
                        Ok(c) => c,
                        Err(e) => {
                            eprintln!("Failed to parse WebSocket JSON '{msg}': {e}");
                            continue;
                        }
                    };
                    match command.command.as_str() {
                        "get-version-info" => {
                            let machine_str = match uname() {
                                Ok(uts) => uts.machine().to_str().unwrap_or("iPhone").to_string(),
                                Err(_) => "iPhone".to_string(),
                            };
                            ws_stream.send(command.respond(CommandData::Versions { versions: RelayVersions {
                                hardware_version: machine_str,
                                software_name: "iPhone OS".to_string(),
                                software_version: mg_copy_answer_rs("ProductVersion"),
                                software_build_id: mg_copy_answer_rs("BuildVersion"),
                                unique_device_id: mg_copy_answer_rs("UniqueDeviceID"),
                                serial_number: mg_copy_answer_rs("SerialNumber"),
                            } })).await?;
                        },
                        "get-validation-data" => {
                            println!("Received get-validation-data request from client!");
                            match generate_validation_data().await {
                                Ok(val_bytes) => {
                                    metrics.record_validation();
                                    println!("Validation data generated successfully ({} bytes)!", val_bytes.len());
                                    let resp = command.respond(CommandData::ValidationData {
                                        data: base64_encode(&val_bytes),
                                    });
                                    if let Err(e) = ws_stream.send(resp).await {
                                        eprintln!("Failed to send validation data response over WebSocket: {e}");
                                    } else {
                                        println!("Sent validation data response to Beeper!");
                                    }
                                }
                                Err(e) => {
                                    metrics.record_error(&format!("{e:?}"));
                                    eprintln!("ERROR generating validation data: {e:?}");
                                    let err_resp = command.respond(CommandData::Empty {});
                                    let _ = ws_stream.send(err_resp).await;
                                }
                            }
                        },
                        "pong" => {
                            last_activity = Instant::now();
                        },
                        _raw => {
                            println!("Unhandled command from relay: {_raw}");
                        },
                    }
                },
                _ = time::sleep_until(last_ping + ping_interval) => {
                    if last_activity.elapsed() > max_idle {
                        println!("[Relay] No response/activity from server in {:?}. Forcing reconnect...", max_idle);
                        break;
                    }
                    if let Err(e) = ws_stream.send(RelayCommand {
                        command: "ping".to_string(),
                        id: None,
                        data: None,
                    }.to_message()).await {
                        eprintln!("[Relay] Failed to send WebSocket ping: {e}");
                        break;
                    }
                    last_ping = Instant::now();
                }
            }
        }
        Ok(())
    }

    pub fn new(url: String, state: Option<RelayState>, metrics: Arc<RelayMetrics>) -> Relay {
        let resource = RelayResource {
            url: Mutex::new(url),
            state: Mutex::new(state),
            metrics,
        };

        ResourceManager::new(
            Arc::new(resource),
            ExponentialBuilder::default()
                .with_max_delay(Duration::from_secs(30))
                .with_max_times(usize::MAX), None)
    }
}
