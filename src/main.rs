use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command as StdCommand, Stdio};
use std::sync::Arc;
use std::time::Duration;
use std::fs;

use chrono::Local;
use configparser::ini::Ini;
use futures_util::{SinkExt, StreamExt};
use log::{error, info, warn, LevelFilter};
use regex::Regex;
use reqwest::Client;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, AsyncSeekExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command as TokioCommand};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};


const DISCORD_WS_BASE: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
const GUILD_ID: &str = "1186570213077041233";
const CHANNEL_IDS: &[u64] = &[
    1282542323590496277,
    1282543762425516083,
    1282543762425516083,
    1282542323590496277,
    1282542323590496277,
    1282542323590496277,
];
const WEBHOOK_COLORS: &[u32] = &[11141375, 11141375, 16749056, 11206400, 16744703, 3093247];
const ROBLOX_GAME_ID: &str = "15532962292";

struct LdProcess {
    stdin: ChildStdin,
    stdout: Option<BufReader<ChildStdout>>,
}

struct Sniper {
    config: Ini,
    output_list: Arc<Mutex<Vec<LdProcess>>>,
    share_link_found: Arc<Mutex<bool>>,
    adb_path: PathBuf,
    is_running: bool,
    words: Vec<String>,
    link_pattern: Regex,
    link_pattern_share: Regex,
    emu_pattern: Regex,
    word_patterns: Vec<Regex>,
    activated_index: Vec<usize>,
    http_client: Client,
}

impl Sniper {
    pub fn new() -> Self {
        let mut config = Ini::new();
        let exe_path = env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
		let fallback = PathBuf::from(".");
        let config_path = exe_path
            .parent()
			.unwrap_or(&fallback)
            .join("config.ini");
        
        if !config_path.exists() {
            config.load("config.ini").unwrap_or_default();
        } else {
            config.load(config_path.to_str().unwrap_or("config.ini")).unwrap_or_default();
        }

        let adb_path = PathBuf::from(config.get("Technical", "LDPlayer Path").unwrap_or_default()).join("adb.exe");

        let words = vec![
            "Corruption".to_string(),
            "Jester".to_string(),
            "Rin".to_string(),
            "Glitched".to_string(),
            "Dreamspace".to_string(),
            "Cyberspace".to_string(),
        ];

        let link_pattern = Regex::new(&format!(
            r"https://.*roblox\.com/games/{}/.*privateServerLinkCode=[0-9]{{32}}",
            ROBLOX_GAME_ID
        )).unwrap();
        
        let link_pattern_share = Regex::new(r"https://.*roblox\.com/share\?code=.*type=Server").unwrap();
		
        let emu_pattern = Regex::new(r"emulator-[0-9]{4}").unwrap();

        let word_patterns = vec![
            Regex::new(r"c[orr]+.+up").unwrap(),
            Regex::new(r"jes.+[ter]+").unwrap(),
            Regex::new(r"rin").unwrap(),
            Regex::new(r"g[lit]+.+ch").unwrap(),
            Regex::new(r"ds|dr[ea]+.+ms").unwrap(),
            Regex::new(r"cy[b]?.+[er]+s").unwrap(),
        ];

        Self {
            config,
            output_list: Arc::new(Mutex::new(Vec::new())),
            share_link_found: Arc::new(Mutex::new(false)),
            adb_path,
            is_running: true,
            words,
            link_pattern,
            link_pattern_share,
            emu_pattern,
            word_patterns,
            activated_index: Vec::new(),
            http_client: Client::new(),
        }
    }

    fn _setup_logging(&self) {
        env_logger::Builder::new()
            .format(|buf, record| {
                writeln!(
                    buf,
                    "[{}][{}] - {}",
                    Local::now().format("%H:%M:%S"),
                    record.line().unwrap_or(0),
                    record.args()
                )
            })
            .filter(None, LevelFilter::Info)
            .init();
    }

    async fn _identify(&self, ws: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin)) {
        let token = self.config.get("Authentication", "Discord Token").unwrap_or_default();
        let identify_payload = json!({
            "op": 2,
            "d": {
                "token": token,
                "properties": {
                    "$os": "windows",
                    "$browser": "chrome",
                    "$device": "pc",
                },
            },
        });
        if let Err(e) = ws.send(Message::Text(identify_payload.to_string().into())).await {
            error!("{}", e);
        }
    }

    async fn _subscribe(&self, ws: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin)) {
        let subscription_payload = json!({
            "op": 14,
            "d": {
                "guild_id": GUILD_ID,
                "channels_ranges": {},
                "typing": true,
                "threads": false,
                "activities": false,
                "members": [],
                "thread_member_lists": [],
            },
        });
        if let Err(e) = ws.send(Message::Text(subscription_payload.to_string().into())).await {
            error!("{}", e);
        }
    }

    fn _should_process_message(&self, message: &str, choice_id: usize) -> bool {
        let msg_lower = message.to_lowercase();
        if !self.word_patterns[choice_id].is_match(&msg_lower) {
            return false;
        }

        return true;
    }

    async fn _extract_server_code(&self, message: &str) -> Option<String> {
        if let Some(link_match) = self.link_pattern.find(message) {
            let mut share_link_found = self.share_link_found.lock().await;
            *share_link_found = false;
            return link_match.as_str().split("LinkCode=").last().map(|s| s.to_string());
        }

        if let Some(link_match_2) = self.link_pattern_share.find(message) {
            let mut share_link_found = self.share_link_found.lock().await;
            *share_link_found = true;
            return link_match_2.as_str().split("code=").last()
                .and_then(|s| s.split('&').next())
                .map(|s| s.to_string());
        }

        None
    }

    async fn _handle_server_join(&self, choice_id: usize, server_code: &str) {
        let use_ldplayer = self.config.get("Technical", "Use LDPlayer").unwrap_or_default().to_lowercase() == "true";
        if use_ldplayer {
            self._join_ldplayer(server_code, choice_id).await;
        } else {
            self._join_windows(server_code, choice_id).await;
        }
        info!("{} link found!", self.words[choice_id]);
		
		self._check_logs(use_ldplayer, choice_id).await;
    }

    async fn _send_webhook_notification(&self, choice_id: usize, server_code: &str) {
        let share_link_found = *self.share_link_found.lock().await;
        let final_link = if !share_link_found {
            format!("https://www.roblox.com/games/{}/Sols-RNG?privateServerLinkCode={}", ROBLOX_GAME_ID, server_code)
        } else {
            format!("https://www.roblox.com/share?code={}&type=Server", server_code)
        };

        let webhook_url = self.config.get("Webhook", "Webhook Link").unwrap_or_default();
        if webhook_url.is_empty() {
            return;
        }

        let user_id = self.config.get("Webhook", "Discord User ID").unwrap_or_default();
        let payload = json!({
            "content": format!("<@{}>", user_id),
            "embeds": [
                {
                    "title": format!("[{}] {} Link Sniped!", Local::now().format("%H:%M:%S"), self.words[choice_id]),
                    "color": WEBHOOK_COLORS[choice_id],
                    "fields": [
                        {"name": format!("{} Link:", self.words[choice_id]), "value": final_link}
                    ],
                    "footer": {"text": "weii joins"},
                }
            ],
        });

        match self.http_client.post(&webhook_url).json(&payload).send().await {
            Ok(response) => {
                if response.status().as_u16() >= 400 {
                    let text = response.text().await.unwrap_or_default();
                    error!("Failed to send webhook: {}", text);
                }
            }
            Err(e) => error!("Failed to send webhook: {}", e),
        }
    }
	
	async fn _check_logs(&self, use_ldplayer: bool, choice_id: usize) {
		if use_ldplayer {
			let mut output_list = self.output_list.lock().await;
			let mut handles = Vec::new();

			for proc in output_list.iter_mut() {
				let shell_command = "logcat -T 1 | grep  --line-buffered -F 'BloxstrapRPC'\n";
				if let Err(e) = proc.stdin.write_all(shell_command.as_bytes()).await {
					error!("Failed to write to ADB stdin: {}", e);
					continue;
				}

				if let Some(mut stdout) = proc.stdout.take() {
					let target_word = self.words[choice_id].to_lowercase();
					let adb_path = self.adb_path.clone();

					let handle = tokio::spawn(async move {
						loop {
							let mut line = String::new();
							match stdout.read_line(&mut line).await {
								Ok(0) => {
									info!("ADB stdout stream closed.");
									break;
								}
								Ok(_) => {
									let trimmed = line.trim();
									if let Some(json_start) = trimmed.find('{') {
										let json_str = &trimmed[json_start..];
										if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
											if let Some(hover_text) = parsed["data"]["largeImage"]["hoverText"].as_str() {
												info!("Current Biome: {}", hover_text);
												if hover_text.to_lowercase() != target_word {
													info!("Closing Roblox...");
													let _ = StdCommand::new(&adb_path)
														.args(["shell", "am", "force-stop", "com.roblox.client"])
														.spawn();
												}
												return;
											}
										}
									}
								}
								Err(e) => {
									error!("Failed to read from ADB stdout: {}", e);
									break;
								}
							}
						}
					});
					handles.push(handle);
				}
			}
			
		drop(output_list);
			for handle in handles {
				let _ = handle.await;
			}
		} else {
			let target_word = self.words[choice_id].to_lowercase();

			tokio::spawn(async move {
				let log_dir = match std::env::var("LOCALAPPDATA") {
					Ok(appdata) => PathBuf::from(appdata).join("Roblox").join("logs"),
					Err(_) => {
						error!("Could not read %LOCALAPPDATA%");
						return;
					}
				};

				let latest_log = std::fs::read_dir(&log_dir)
					.ok()
					.and_then(|entries| {
						entries
							.filter_map(|e| e.ok())
							.filter(|e| {
								e.path().extension().and_then(|x| x.to_str()) == Some("log")
							})
							.max_by_key(|e| {
								e.metadata().and_then(|m| m.modified()).ok()
							})
							.map(|e| e.path())
					});

				let log_path = match latest_log {
					Some(p) => p,
					None => {
						error!("No Roblox log files found in {:?}", log_dir);
						return;
					}
				};

				let file = match tokio::fs::File::open(&log_path).await {
					Ok(f) => f,
					Err(e) => {
						error!("Failed to open log file: {}", e);
						return;
					}
				};

				let mut reader = BufReader::new(file);

				use tokio::io::AsyncSeekExt;
				let _ = reader.seek(std::io::SeekFrom::End(0)).await;

				loop {
					let mut line = String::new();
					match reader.read_line(&mut line).await {
						Ok(0) => {
							tokio::time::sleep(Duration::from_millis(200)).await;
						}
						Ok(_) => {
							let trimmed = line.trim();
							if trimmed.contains("BloxstrapRPC") {
								if let Some(json_start) = trimmed.find('{') {
									let json_str = &trimmed[json_start..];
									if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
										if let Some(hover_text) = parsed["data"]["largeImage"]["hoverText"].as_str() {
											info!("Current Biome: {}", hover_text);
											if hover_text.to_lowercase() != target_word {
												info!("Closing Roblox...");
												let _ = StdCommand::new("cmd")
													.args(["/C", "taskkill", "/IM", "RobloxPlayerBeta.exe", "/F"])
													.spawn();
											}
											return;
										}
									}
								}
							}
						}
						Err(e) => {
							error!("Failed to read log file: {}", e);
							return;
						}
					}
				}
			});
		}
	}
		
async fn _join_ldplayer(&self, server_code: &str, choice_id: usize) {
		let share_link_found = *self.share_link_found.lock().await;
		let final_link = if !share_link_found {
			format!(r"roblox://placeID={}\&linkCode={}", ROBLOX_GAME_ID, server_code)
		} else {
			format!(r"roblox://navigation/share_links?code={}\&type=Server", server_code)
		};

		let shell_command = format!("am start -a android.intent.action.VIEW {}\n", final_link);
		let data = shell_command.as_bytes();

		let mut output_list = self.output_list.lock().await;
		for proc in output_list.iter_mut() {
			if let Err(e) = proc.stdin.write_all(data).await {
				error!("Failed to write to ADB stdin: {}", e);
				continue;
			}

			if let Some(stdout) = proc.stdout.as_mut() {
				let mut line = String::new();
				if let Ok(n) = stdout.read_line(&mut line).await {
					if n > 0 {
						info!("{}", line.trim());
					}
				}
			}
		}

		self._send_webhook_notification(choice_id, server_code).await;
	}

    async fn _join_windows(&self, server_code: &str, choice_id: usize) {
        let share_link_found = *self.share_link_found.lock().await;
        let final_link = if !share_link_found {
            format!("roblox://placeID={}^&linkCode={}", ROBLOX_GAME_ID, server_code)
        } else {
            format!("roblox://navigation/share_links?code={}^&type=Server", server_code)
        };

        // Equivalent of Popen(["start", final_link], shell=True)
        let _ = StdCommand::new("cmd")
            .args(["/C", "start", "", &final_link])
            .spawn();
            
        self._send_webhook_notification(choice_id, server_code).await;
    }

    async fn process_message(&self, content: &str, choice_id: usize) {
        if !self._should_process_message(content, choice_id) {
            return;
        }

        let server_code = match self._extract_server_code(content).await {
            Some(code) => code,
            None => return,
        };

        info!("Found message! content: {}", content);
        self._handle_server_join(choice_id, &server_code).await;
    }

    pub async fn run(mut self) {
        let mut active_indices = Vec::new();
        let config_keys = ["Corruption", "Jester", "Rin", "Glitched", "Dreamspace", "Cyberspace"];

        for (i, config_key) in config_keys.iter().enumerate() {
            if self.config.get("Toggles", config_key).unwrap_or_else(|| "false".to_string()).to_lowercase() == "true" {
                active_indices.append(&mut vec![i]);
            }
        }

        if active_indices.is_empty() {
            eprintln!("At least one option has to be True.");
            return;
        }

        self.activated_index = active_indices;
        self._setup_logging();

        let _ = StdCommand::new("cmd").args(["/C", "title weii joins"]).status();
        let _ = StdCommand::new("cmd").args(["/C", "CLS"]).status();

        if self.config.get("Technical", "Use LDPlayer").unwrap_or_else(|| "false".to_string()).to_lowercase() == "true" {
            info!("Using LDPlayer Mode...");

            let proc = TokioCommand::new(&self.adb_path)
                .arg("devices")
                .stdout(Stdio::piped())
                .spawn();

            if let Ok(child) = proc {
                let output = child.wait_with_output().await.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
                let devices: Vec<_> = self.emu_pattern.find_iter(&output).map(|m| m.as_str().to_string()).collect();

                if !devices.is_empty() {
                    info!("Found devices: {:?}", devices);
                    let mut output_list = self.output_list.lock().await;
                    for device in devices {
                        let shell_proc = TokioCommand::new(&self.adb_path)
                            .args(["-s", &device, "shell"])
                            .stdin(Stdio::piped())
                            .stdout(Stdio::piped())
                            .spawn();
                        
                        if let Ok(mut child) = shell_proc {
                            let stdin = child.stdin.take().unwrap();
                            let stdout = BufReader::new(child.stdout.take().unwrap());
							output_list.push(LdProcess {
								stdin,
								stdout: Some(stdout), // <-- wrap in Some
							});
                        }
                    }
                } else {
                    warn!("No emulator devices found.");
                }
            }
        }

        info!("SNIPER STARTED");

        let sniper_arc = Arc::new(self);

        loop {
            info!("Connecting to discord websockets...");
            let result = connect_async(DISCORD_WS_BASE).await;
            
            match result {
                Ok((mut ws_stream, _)) => {
                    sniper_arc._identify(&mut ws_stream).await;
                    sniper_arc._subscribe(&mut ws_stream).await;

                    let mut heartbeat_interval = Duration::from_secs(41); // Default
                    if let Some(Ok(Message::Text(text))) = ws_stream.next().await {
                        if let Ok(v) = serde_json::from_str::<Value>(&text) {
                            if let Some(interval) = v["d"]["heartbeat_interval"].as_u64() {
                                heartbeat_interval = Duration::from_millis(interval);
                            }
                        }
                    }

                    let mut heartbeat_timer = tokio::time::interval(heartbeat_interval);
                    
                    loop {
                        tokio::select! {
                            _ = heartbeat_timer.tick() => {
                                let heartbeat_json = json!({"op": 1, "d": null});
                                if ws_stream.send(Message::Text(heartbeat_json.to_string().into())).await.is_err() {
                                    info!("Connection closed while sending heartbeat, retrying...");
                                    break;
                                }
                            }
                            msg = ws_stream.next() => {
                                match msg {
                                    Some(Ok(Message::Text(text))) => {
                                        if let Ok(event) = serde_json::from_str::<Value>(&text) {
                                            if event["t"] == "MESSAGE_CREATE" {
                                                let channel_id_str = event["d"]["channel_id"].as_str().unwrap_or("0");
                                                let channel_id: u64 = channel_id_str.parse().unwrap_or(0);
                                                let content = event["d"]["content"].as_str().unwrap_or("");

                                                for (choice_id_idx, &target_channel) in CHANNEL_IDS.iter().enumerate() {
                                                    if !sniper_arc.activated_index.contains(&choice_id_idx) {
                                                        continue;
                                                    }
                                                    if channel_id == target_channel {
                                                        let sniper_clone = Arc::clone(&sniper_arc);
                                                        let content_owned = content.to_string();
                                                        tokio::spawn(async move {
                                                            sniper_clone.process_message(&content_owned, choice_id_idx).await;
                                                        });
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    Some(Err(_)) | None => {
                                        info!("Connection closed while receiving messages (or token is invalid), retrying...");
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("WebSocket error: {}", e);
                    sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let sniper = Sniper::new();
    sniper.run().await;
}
