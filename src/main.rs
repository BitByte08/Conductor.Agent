mod server;
mod installer;
use server::{ServerProcess, ServerEvent};
use tokio::sync::mpsc;
use sysinfo::{System, RefreshKind, CpuRefreshKind, MemoryRefreshKind};
use tokio::time::{sleep, Duration};
use log::{info, error, warn};
use tokio_tungstenite::{connect_async, connect_async_tls_with_config, tungstenite::protocol::Message, Connector};
use rustls;
use rustls_native_certs;
use std::sync::Arc;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;

#[derive(Serialize, serde::Deserialize, Debug, Clone, Default)]
struct AgentConfig {
    ram_mb: String, // e.g. "4G"
    agent_id: String,
    backend_url: String,
}

#[derive(Serialize, serde::Deserialize, Debug, Clone)]
struct ServerProperties(std::collections::HashMap<String, String>);

#[derive(Serialize)]
struct Heartbeat {
    #[serde(rename = "type")]
    type_: String,
    cpu_usage: f32,
    ram_usage: u64,
    ram_total: u64,
    server_status: String,
    config: AgentConfig,
    metadata: String,
}

#[derive(serde::Deserialize, Debug)]
#[serde(tag = "type", content = "payload")]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum BackendMessage {
    StartServer { jar_path: String },
    StopServer,
    Command { command: String },
    InstallServer { url: String, filename: String, server_type: String, version: String },
    InstallMod { url: String, filename: String },
    UpdateConfig { ram_mb: String },
    ReadProperties,
    WriteProperties { properties: std::collections::HashMap<String, String> },
}

async fn read_server_properties(base_dir: &str) -> anyhow::Result<std::collections::HashMap<String, String>> {
    let path = format!("{}/server.properties", base_dir);
    if !std::path::Path::new(&path).exists() {
        let default_props = r#"#Minecraft server properties
#Thu Jan 01 00:00:00 UTC 2026
spawn-protection=16
max-tick-time=60000
query.port=25565
generator-settings=
force-gamemode=false
allow-nether=true
enforce-whitelist=false
gamemode=survival
broadcast-console-to-ops=true
enable-query=false
player-idle-timeout=0
difficulty=easy
spawn-monsters=true
op-permission-level=4
pvp=true
snooper-enabled=true
level-type=default
hardcore=false
enable-command-block=false
max-players=20
network-compression-threshold=256
resource-pack-sha1=
max-world-size=29999984
server-port=25565
server-ip=
spawn-npcs=true
allow-flight=false
level-name=world
view-distance=10
resource-pack=
spawn-animals=true
white-list=false
generate-structures=true
online-mode=true
max-build-height=256
level-seed=
prevent-proxy-connections=false
use-native-transport=true
motd=A Minecraft Server
enable-rcon=false
"#;
        tokio::fs::write(path.clone(), default_props).await?;
    }

    let content = tokio::fs::read_to_string(path).await?;
    let mut props = std::collections::HashMap::new();
    for line in content.lines() {
        if let Some((key, value)) = line.split_once('=') {
            props.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    Ok(props)
}

async fn write_server_properties(props: std::collections::HashMap<String, String>, base_dir: &str) -> anyhow::Result<()> {
    // We want to preserve comments if possible, but for MVP we might validly overwrite.
    // Let's just overwrite for now to ensure consistency.
    let mut content = String::from("# Minecraft server properties\n# (File overwritten by Conductor)\n");
    // Sort keys for stability
    let mut keys: Vec<&String> = props.keys().collect();
    keys.sort();
    
    for key in keys {
        let value = props.get(key).unwrap();
        content.push_str(&format!("{}={}\n", key, value));
    }
    
    let path = format!("{}/server.properties", base_dir);
    tokio::fs::write(path, content).await?;
    Ok(())
}

fn make_tls_connector() -> anyhow::Result<tokio_tungstenite::Connector> {
    // Load root certificates from the platform trust store
    info!("Loading native root certificates...");
    let certs = rustls_native_certs::load_native_certs()?;
    info!("Loaded {} root certificates", certs.len());
    
    let mut root_store = rustls::RootCertStore::empty();
    for (i, cert) in certs.into_iter().enumerate() {
        match root_store.add(cert) {
            Ok(_) => {},
            Err(e) => {
                warn!("Failed to add certificate {}: {}", i, e);
            }
        }
    }
    info!("Root certificate store ready with {} certs", root_store.len());

    let client_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    info!("TLS connector created successfully");
    Ok(tokio_tungstenite::Connector::Rustls(Arc::new(client_config)))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize rustls with ring crypto provider
    let _ = rustls::crypto::ring::default_provider().install_default();
    
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    info!("Starting Conductor Agent...");

    // Setup signal handler for graceful shutdown
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => info!("Received SIGTERM"),
            _ = sigint.recv() => info!("Received SIGINT"),
        }
        let _ = shutdown_tx.send(()).await;
    });

    // Initialize system monitor settings
    let mut sys = System::new_with_specifics(
        RefreshKind::nothing()
            .with_cpu(CpuRefreshKind::everything())
            .with_memory(MemoryRefreshKind::everything())
    );

    // Load Config and Metadata - Ensure defaults if missing
    let mut config: AgentConfig = match tokio::fs::read_to_string("conductor_config.json").await {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| AgentConfig { 
            ram_mb: "4G".into(), 
            agent_id: "test-agent".into(), 
            backend_url: "ws://127.0.0.1:8000".into() 
        }),
        Err(_) => AgentConfig { 
            ram_mb: "4G".into(), 
            agent_id: "test-agent".into(), 
            backend_url: "ws://127.0.0.1:8000".into() 
        },
    };

    // Ensure minecraft directory exists
    if !std::path::Path::new("minecraft").exists() {
        tokio::fs::create_dir("minecraft").await?;
    }
    // Note: Config is global, Metadata is per-server (usually).
    // Let's move metadata to minecraft/ as well for consistency?
    // Actually, user asked to group "server created files". 
    // server.properties, logs, world, mods -> these are created by server.
    // server.jar is installed by us.
    // So if we run in `minecraft/`, all these go there.
    
    // Metadata we manage, keep in root? Or in minecraft/?
    // Let's keep metadata in root for now to avoid complexity of migration, 
    // unless we change installer to write to minecraft/.
    
    // Load metadata from current directory (systemd sets WorkingDirectory=/var/lib/conductor/<id>)
    let metadata_path = "conductor_metadata.json";
    let mut metadata: installer::ServerMetadata = match tokio::fs::read_to_string(&metadata_path).await {
        Ok(s) => serde_json::from_str(&s).unwrap_or(installer::ServerMetadata { server_type: "Unknown".into(), version: "?".into() }),
        Err(_) => installer::ServerMetadata { server_type: "Unknown".into(), version: "?".into() },
    };

    // Server Manager State
    let mut server = ServerProcess::new();
    let (server_tx, mut server_rx) = mpsc::channel::<ServerEvent>(100);

    // Attempt to recover existing server process
    if let Err(e) = server.try_recover().await {
        warn!("Failed to recover existing server: {}", e);
    }

    // Stdin Handler - ignore EOF/pipe closure
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(100);
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();
        loop {
            match reader.read_line(&mut line).await {
                Ok(0) => {
                    // EOF - keep running, just don't read anymore
                    info!("Stdin closed (running in background)");
                    break;
                }
                Ok(_) => {
                    let trimmed = line.trim().to_string();
                    if !trimmed.is_empty() {
                        let _ = stdin_tx.send(trimmed).await;
                    }
                    line.clear();
                }
                Err(_) => {
                    // Error reading stdin (e.g., pipe closed) - just stop reading
                    break;
                }
            }
        }
    });

    loop {
        // Construct URL dynamically to support config changes
        let backend_url = format!("{}/ws/agent/{}", config.backend_url.trim_end_matches('/'), config.agent_id);
        info!("Connecting to backend: {}", backend_url);
        
        // Connect based on scheme (wss vs ws)
        let connect_result = if backend_url.starts_with("wss://") {
            // Use TLS connector for wss:// with system roots
            let connector = match make_tls_connector() {
                Ok(conn) => conn,
                Err(e) => {
                    error!("Failed to create TLS connector: {}", e);
                    tokio::select! {
                        _ = sleep(Duration::from_secs(5)) => {},
                        Some(line) = stdin_rx.recv() => {
                            let parts: Vec<&str> = line.split_whitespace().collect();
                            if !parts.is_empty() && parts[0] == "set-url" && parts.len() > 1 {
                                config.backend_url = parts[1].to_string();
                                info!("Backend URL updated to: {}", config.backend_url);
                                let _ = tokio::fs::write("conductor_config.json", serde_json::to_string_pretty(&config).unwrap()).await;
                            }
                        }
                    }
                    continue;
                }
            };
            info!("Attempting TLS connection to: {}", backend_url);
            let result = connect_async_tls_with_config(&backend_url, None, false, Some(connector)).await;
            info!("TLS connection result: {:?}", result.as_ref().map(|_| "Ok").map_err(|e| format!("{:?}", e)));
            result
        } else {
            // Use plain ws://
            connect_async(&backend_url).await
        };
        
        let used_url = backend_url.clone();
        let connect_result = match connect_result {
            Ok(pair) => Ok(pair),
            Err(e) => Err(e)
        };

        match connect_result {
            Ok((ws_stream, _)) => {
                info!("Connected to Backend: {}", used_url);
                let (mut write, mut read) = ws_stream.split();

                loop {
                    sys.refresh_cpu_all();
                    sys.refresh_memory();
                    
                    let cpu_usage = sys.global_cpu_usage();
                    let total_mem = sys.total_memory();
                    let used_mem = sys.used_memory();
                    let server_status = if server.is_running() { "ONLINE" } else { "OFFLINE" };

                    // Reload metadata from current directory
                    if let Ok(s) = tokio::fs::read_to_string(&metadata_path).await {
                        if let Ok(m) = serde_json::from_str(&s) {
                            metadata = m;
                        }
                    }

                    // Reload config if changed
                    if let Ok(s) = tokio::fs::read_to_string("conductor_config.json").await {
                         if let Ok(c) = serde_json::from_str(&s) {
                             config = c;
                         }
                    }

                    let heartbeat = Heartbeat {
                        type_: "HEARTBEAT".to_string(),
                        cpu_usage,
                        ram_usage: used_mem,
                        ram_total: total_mem,
                        server_status: server_status.to_string(),
                        config: config.clone(),
                        metadata: metadata.server_type.clone() + " " + &metadata.version,
                    };

                    tokio::select! {
                        _ = sleep(Duration::from_secs(5)) => {
                            let json = serde_json::to_string(&heartbeat)?;
                            if let Err(e) = write.send(Message::Text(json.into())).await {
                                error!("WS Send Error: {}", e);
                                break;
                            }
                        }
                        
                        _ = shutdown_rx.recv() => {
                            info!("Shutdown signal received, stopping server...");
                            if let Err(e) = server.graceful_stop().await {
                                error!("Failed to gracefully stop server: {}", e);
                            }
                            info!("Agent shutdown complete");
                            return Ok(());
                        }
                        
                        Some(event) = server_rx.recv() => {
                            match event {
                                ServerEvent::Output(line) => {
                                    let msg = serde_json::json!({
                                        "type": "LOG",
                                        "payload": { "line": line }
                                    });
                                    info!("Server: {}", line); // Mirror to local terminal
                                    if let Err(_) = write.send(Message::Text(msg.to_string().into())).await { break; }
                                }
                                ServerEvent::Exit(code) => {
                                    // Clean up server state on exit
                                    server.cleanup_on_exit().await;
                                    
                                    let _ = write.send(Message::Text(serde_json::json!({
                                        "type": "SERVER_EXIT",
                                        "payload": { "code": code }
                                    }).to_string().into())).await;
                                    let _ = write.send(Message::Text(serde_json::json!({ "type": "LOG", "payload": { "line": format!("Server exited: {:?}", code) } }).to_string().into())).await;
                                }
                                _ => {}
                            }
                        }

                        // Handle Stdin Commands
                        Some(line) = stdin_rx.recv() => {
                            let parts: Vec<&str> = line.split_whitespace().collect();
                            if parts.is_empty() { continue; }
                            
                            match parts[0] {
                                "set-id" => {
                                    if parts.len() > 1 {
                                        config.agent_id = parts[1].to_string();
                                        info!("Agent ID updated to: {}", config.agent_id);
                                        let json = serde_json::to_string_pretty(&config)?;
                                        tokio::fs::write("conductor_config.json", json).await?;
                                        info!("Config saved. Reconnecting...");
                                        break; 
                                    } else {
                                        error!("Usage: set-id <new_id>");
                                    }
                                },
                                "set-url" => {
                                    if parts.len() > 1 {
                                        config.backend_url = parts[1].to_string();
                                        info!("Backend URL updated to: {}", config.backend_url);
                                        let json = serde_json::to_string_pretty(&config)?;
                                        tokio::fs::write("conductor_config.json", json).await?;
                                        info!("Config saved. Reconnecting...");
                                        break;
                                    }
                                },
                                "status" => {
                                    info!("Status: Backend={}, ID={}, Server={}", config.backend_url, config.agent_id, server_status);
                                }
                                "help" => {
                                    info!("Available commands: set-id <id>, set-url <url>, status, help");
                                }
                                _ => {
                                    warn!("Unknown command: {}", parts[0]);
                                }
                            }
                        }

                        msg = read.next() => {
                            match msg {
                                Some(Ok(Message::Text(text))) => {
                                    if let Ok(cmd) = serde_json::from_str::<BackendMessage>(&text) {
                                        info!("Received command: {:?}", cmd);
                                        match cmd {
                                            BackendMessage::StartServer { jar_path: _ } => {
                                                let mut args = vec!["nogui".into()];
                                                if !config.ram_mb.is_empty() {
                                                    args.insert(0, format!("-Xmx{}", config.ram_mb));
                                                    args.insert(0, format!("-Xms{}", config.ram_mb));
                                                }
                                                // Run server.jar from minecraft/ subdirectory
                                                let jar_full = "minecraft/server.jar";

                                                // Debug: ensure eula exists and log contents
                                                let eula_path = "minecraft/eula.txt";
                                                match tokio::fs::read_to_string(&eula_path).await {
                                                    Ok(content) => {
                                                        let _ = server_tx.clone().send(ServerEvent::Output(format!("EULA file found: {}", eula_path))).await;
                                                        let _ = server_tx.clone().send(ServerEvent::Output(format!("EULA content: {}", content.trim()))).await;
                                                    }
                                                    Err(e) => {
                                                        let _ = server_tx.clone().send(ServerEvent::Output(format!("EULA missing or unreadable ({}) : {}", eula_path, e))).await;
                                                    }
                                                }

                                                // Pre-check port availability from server.properties
                                                match read_server_properties("minecraft").await {
                                                    Ok(props) => {
                                                        let port = props.get("server-port").and_then(|s| s.parse::<u16>().ok()).unwrap_or(25565);
                                                        // Try IPv4 and IPv6 binds to detect if port is already in use
                                                        let mut busy = false;
                                                        match std::net::TcpListener::bind(("0.0.0.0", port)) {
                                                            Ok(listener) => { drop(listener); }
                                                            Err(e) => {
                                                                if e.kind() == std::io::ErrorKind::AddrInUse {
                                                                    busy = true;
                                                                } else {
                                                                    // Not a bind-in-use error, log but don't treat as busy
                                                                    let _ = server_tx.clone().send(ServerEvent::Output(format!("Port check (IPv4) error for {}: {}", port, e))).await;
                                                                }
                                                            }
                                                        }
                                                        if !busy {
                                                            match std::net::TcpListener::bind(("::", port)) {
                                                                Ok(listener) => { drop(listener); }
                                                                Err(e) => {
                                                                    if e.kind() == std::io::ErrorKind::AddrInUse {
                                                                        busy = true;
                                                                    } else {
                                                                        // IPv6 may not be available; ignore non-AddrInUse errors here
                                                                        let _ = server_tx.clone().send(ServerEvent::Output(format!("Port check (IPv6) error for {}: {}", port, e))).await;
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        if busy {
                                                            let _ = server_tx.clone().send(ServerEvent::Output(format!("Failed to start: port {} is in use" , port))).await;
                                                            // don't attempt to start the server if the port is unavailable
                                                            continue;
                                                        }
                                                    }
                                                    Err(_) => {
                                                        let _ = server_tx.clone().send(ServerEvent::Output("No server.properties found; assuming default port 25565".into())).await;
                                                    }
                                                }

                                                if let Err(e) = server.start(&jar_full, args, server_tx.clone()).await {
                                                    error!("Failed to start server: {}", e);
                                                }
                                            },

                                            BackendMessage::StopServer => { 
                                                let _ = server.graceful_stop().await; 
                                            },
                                            BackendMessage::Command { command } => {
                                                if let Err(e) = server.write_command(&command).await {
                                                    error!("Failed to write command: {}", e);
                                                }
                                            },
                                            BackendMessage::UpdateConfig { ram_mb } => {
                                                config.ram_mb = ram_mb;
                                                let json = serde_json::to_string_pretty(&config)?;
                                                tokio::fs::write("conductor_config.json", json).await?;
                                            },
                                            BackendMessage::ReadProperties => {
                                                // Read from minecraft/ directory where server actually runs
                                                if let Ok(props) = read_server_properties("minecraft").await {
                                                    let msg = serde_json::json!({ "type": "PROPERTIES", "payload": props });
                                                    let _ = write.send(Message::Text(msg.to_string().into())).await;
                                                } else {
                                                    let _ = write.send(Message::Text(serde_json::json!({ 
                                                        "type": "LOG", 
                                                        "payload": { "line": "server.properties not found - server may not have been started yet" } 
                                                    }).to_string().into())).await;
                                                }
                                            },
                                            BackendMessage::WriteProperties { properties } => {
                                                // Write to minecraft/ directory where server actually runs
                                                let _ = write_server_properties(properties, "minecraft").await;
                                            },
                                            BackendMessage::InstallServer { url, filename, server_type, version } => {
                                                let url_clone = url.clone();
                                                let filename_clone = filename.clone();
                                                let type_clone = server_type.clone();
                                                let ver_clone = version.clone();
                                                let agent_id_clone = config.agent_id.clone();
                                                let tx = server_tx.clone();
                                                tokio::spawn(async move {
                                                    let _ = tx.send(ServerEvent::Output(format!("Starting download of {}...", url_clone))).await;
                                                    
                                                    // INSTALL TARGET: minecraft/server.jar in current directory
                                                    let base = "minecraft";
                                                    
                                                    // Ensure minecraft directory exists
                                                    if let Err(e) = tokio::fs::create_dir_all(&base).await {
                                                        let _ = tx.send(ServerEvent::Output(format!("Failed to create minecraft directory: {}", e))).await;
                                                        return;
                                                    }
                                                    
                                                    let target_path = format!("{}/server.jar", base);
                                                    
                                                    match installer::download_file(&url_clone, &target_path).await {
                                                        Ok(_) => {
                                                            let _ = tx.send(ServerEvent::Output("Download successful. Accepting EULA...".into())).await;
                                                            if let Err(e) = installer::accept_eula(&base).await {
                                                                let _ = tx.send(ServerEvent::Output(format!("Failed to accept EULA: {}", e))).await;
                                                            } else {
                                                                let _ = tx.send(ServerEvent::Output("EULA accepted.".into())).await;
                                                            }
                                                            if let Err(e) = installer::create_metadata_file(&type_clone, &ver_clone, &base).await {
                                                                let _ = tx.send(ServerEvent::Output(format!("Failed to create metadata: {}", e))).await;
                                                            } else {
                                                                // Notify frontend about metadata so overview can update
                                                                let _ = tx.send(ServerEvent::Output(format!("METADATA: {} {}", type_clone, ver_clone))).await;
                                                            }
                                                            if let Err(e) = installer::create_default_server_files(&base).await {
                                                                let _ = tx.send(ServerEvent::Output(format!("Failed to create default server files: {}", e))).await;
                                                            }
                                                            let _ = tx.send(ServerEvent::Output("Installation complete! You can now start the server.".into())).await;
                                                        },
                                                        Err(e) => {
                                                            let _ = tx.send(ServerEvent::Output(format!("Failed to download server: {}", e))).await;
                                                        }
                                                    }
                                                });
                                            },
                                            BackendMessage::InstallMod { url, filename } => {
                                                let url_clone = url.clone();
                                                let filename_clone = filename.clone();
                                                let agent_id_clone = config.agent_id.clone();
                                                let tx = server_tx.clone();
                                                tokio::spawn(async move {
                                                    let _ = tx.send(ServerEvent::Output(format!("Installing mod from {}...", url_clone))).await;
                                                    // Mods go to minecraft/mods
                                                    let base = "minecraft";
                                                    if let Err(e) = installer::install_mod(&url_clone, &filename_clone, &base).await {
                                                        let _ = tx.send(ServerEvent::Output(format!("Failed to install mod: {}", e))).await;
                                                    } else {
                                                        let _ = tx.send(ServerEvent::Output(format!("Mod installed: {}", filename_clone))).await;
                                                    }
                                                });
                                            }
                                        }
                                    }
                                }
                                Some(Err(e)) => { error!("WS Read Error: {}", e); break; }
                                None => { warn!("Disconnected"); break; }, 
                                _ => {}
                            }
                        }
                    }
                }
            }
            Err(e) => {
                error!("Connection failed: {}", e);
                // Handle stdin while disconnected (simplified)
                tokio::select! {
                    _ = sleep(Duration::from_secs(5)) => {},
                    Some(line) = stdin_rx.recv() => {
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if !parts.is_empty() {
                            if parts[0] == "set-id" && parts.len() > 1 {
                                config.agent_id = parts[1].to_string();
                                info!("Agent ID updated to: {}", config.agent_id);
                                let let_json = serde_json::to_string_pretty(&config).unwrap();
                                let _ = tokio::fs::write("conductor_config.json", let_json).await;
                            } else if parts[0] == "set-url" && parts.len() > 1 {
                                config.backend_url = parts[1].to_string();
                                info!("Backend URL updated to: {}", config.backend_url);
                                let let_json = serde_json::to_string_pretty(&config).unwrap();
                                let _ = tokio::fs::write("conductor_config.json", let_json).await;
                            }
                        }
                    }
                }
            }
        }
    }
}
