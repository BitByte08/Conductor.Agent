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
    let mut file = File::create(path).await?;
    file.write_all(&content).await?;
    info!("Download complete: {}", path);
    Ok(())
}

pub async fn accept_eula(directory: &str) -> anyhow::Result<()> {
    let eula_path = Path::new(directory).join("eula.txt");
    let content = "eula=true\n";
    let mut file = File::create(eula_path).await?;
    file.write_all(content.as_bytes()).await?;
    info!("Accepted EULA in {}", directory);
    Ok(())
}

pub async fn create_metadata_file(server_type: &str, version: &str) -> anyhow::Result<()> {
    let metadata = ServerMetadata {
        server_type: server_type.to_string(),
        version: version.to_string(),
    };
    let json = serde_json::to_string_pretty(&metadata)?;
    let mut file = File::create("conductor_metadata.json").await?;
    file.write_all(json.as_bytes()).await?;
    info!("Metadata saved: {} {}", server_type, version);
    Ok(())
}

pub async fn install_mod(url: &str, filename: &str, mods_dir_path: &str) -> anyhow::Result<()> {
    let mods_dir = Path::new(mods_dir_path);
    if !mods_dir.exists() {
        fs::create_dir(mods_dir).await?;
    }
    
    let path = mods_dir.join(filename);
    info!("Installing mod {} to {:?}", url, path);
    
    // Re-use download logic
    let resp = reqwest::get(url).await?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!("Download failed: {}", resp.status()));
    }
    
    let content = resp.bytes().await?;
    let mut file = File::create(path).await?;
    file.write_all(&content).await?;
    info!("Mod installed successfully");
    Ok(())
}
