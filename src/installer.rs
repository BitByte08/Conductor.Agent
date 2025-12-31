use std::path::Path;
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use log::info;

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct ServerMetadata {
    pub server_type: String,
    pub version: String,
}

pub async fn download_file(url: &str, path: &str) -> anyhow::Result<()> {
    info!("Downloading {} to {}", url, path);
    let resp = reqwest::get(url).await?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!("Download failed: {}", resp.status()));
    }
    
    let content = resp.bytes().await?;
    // Ensure parent directory exists
    let p = Path::new(path);
    if let Some(parent) = p.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent).await?;
        }
    }
    let mut file: File = File::create(path).await?;
    file.write_all(&content).await?;
    info!("Download complete: {}", path);
    Ok(())
}

pub async fn accept_eula(directory: &str) -> anyhow::Result<()> {
    let dir = Path::new(directory);
    if !dir.exists() {
        fs::create_dir_all(dir).await?;
    }
    let eula_path = dir.join("eula.txt");
    let content = "eula=true\n";
    let mut file: File = File::create(eula_path).await?;
    file.write_all(content.as_bytes()).await?;
    info!("Accepted EULA in {:?}", dir);
    Ok(())
}

pub async fn create_metadata_file(server_type: &str, version: &str, base_dir: &str) -> anyhow::Result<()> {
    let metadata = ServerMetadata {
        server_type: server_type.to_string(),
        version: version.to_string(),
    };
    let json = serde_json::to_string_pretty(&metadata)?;
    let base = Path::new(base_dir);
    if !base.exists() {
        fs::create_dir_all(base).await?;
    }
    let meta_path = base.join("conductor_metadata.json");
    let mut file: File = File::create(meta_path).await?;
    file.write_all(json.as_bytes()).await?;
    info!("Metadata saved in {:?}: {} {}", base, server_type, version);
    Ok(())
}

pub async fn install_mod(url: &str, filename: &str, base_dir: &str) -> anyhow::Result<()> {
    let base = Path::new(base_dir);
    let mods_dir = base.join("mods");
    if !mods_dir.exists() {
        fs::create_dir_all(&mods_dir).await?;
    }
    
    let path = mods_dir.join(filename);
    info!("Installing mod {} to {:?}", url, path);
    
    // Re-use download logic (use string path)
    let resp = reqwest::get(url).await?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!("Download failed: {}", resp.status()));
    }
    
    let content = resp.bytes().await?;
    let p = path.to_string_lossy().to_string();
    let mut file: File = File::create(&p).await?;
    file.write_all(&content).await?;
    info!("Mod installed successfully: {:?}", path);
    Ok(())
}

pub async fn create_default_server_files(base_dir: &str) -> anyhow::Result<()> {
    let base = Path::new(base_dir);
    if !base.exists() {
        fs::create_dir_all(base).await?;
    }

    // Create world folder
    let world = base.join("world");
    if !world.exists() {
        fs::create_dir_all(&world).await?;
    }

    // Create default JSON files
    let ops = base.join("ops.json");
    if !ops.exists() {
        let mut f: File = File::create(&ops).await?;
        f.write_all(b"[]").await?;
    }

    let banned_players = base.join("banned-players.json");
    if !banned_players.exists() {
        let mut f: File = File::create(&banned_players).await?;
        f.write_all(b"[]").await?;
    }

    let banned_ips = base.join("banned-ips.json");
    if !banned_ips.exists() {
        let mut f: File = File::create(&banned_ips).await?;
        f.write_all(b"[]").await?;
    }

    let whitelist = base.join("whitelist.json");
    if !whitelist.exists() {
        let mut f: File = File::create(&whitelist).await?;
        f.write_all(b"[]").await?;
    }

    // Ensure server.properties exists with defaults
    let props = base.join("server.properties");
    if !props.exists() {
        let default = r#"#Minecraft server properties
motd=Conductor Server
"#;
        let mut f: File = File::create(&props).await?;
        f.write_all(default.as_bytes()).await?;
    }

    Ok(())
}
