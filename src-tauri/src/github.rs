use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter};

const REPO_API: &str = "https://api.github.com/repos/archr-linux/Arch-R/releases/latest";
const IMAGE_PREFIX: &str = "ArchR-R36S-no-panel-";

#[derive(Debug, Clone, Serialize)]
pub struct ReleaseInfo {
    pub version: String,
    pub image_name: String,
    pub download_url: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadResult {
    pub path: String,
    pub version: String,
    pub image_name: String,
    pub cached: bool,
}

#[derive(Debug, Clone, Serialize)]
struct DownloadProgress {
    percent: f64,
    downloaded_bytes: u64,
    total_bytes: u64,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

pub async fn get_latest_release() -> Result<ReleaseInfo, String> {
    let client = reqwest::Client::builder()
        .user_agent("archr-flasher")
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    let release: GithubRelease = client
        .get(REPO_API)
        .send()
        .await
        .map_err(|e| format!("GitHub API error: {}", e))?
        .json()
        .await
        .map_err(|e| format!("JSON parse error: {}", e))?;

    let asset = release
        .assets
        .iter()
        .find(|a| a.name.starts_with(IMAGE_PREFIX) && a.name.ends_with(".img.xz"))
        .ok_or("No no-panel image found in latest release")?;

    Ok(ReleaseInfo {
        version: release.tag_name,
        image_name: asset.name.clone(),
        download_url: asset.browser_download_url.clone(),
        size_bytes: asset.size,
    })
}

/// Download the image to cache_dir, emitting progress events.
/// Returns (file_path, was_cached).
pub async fn download_image(
    app: &AppHandle,
    release: &ReleaseInfo,
    cache_dir: &Path,
) -> Result<(PathBuf, bool), String> {
    fs::create_dir_all(cache_dir)
        .map_err(|e| format!("Cannot create cache dir: {}", e))?;

    let dest = cache_dir.join(&release.image_name);

    // Check if already cached (same name + matching size)
    if dest.exists() {
        if let Ok(meta) = fs::metadata(&dest) {
            if meta.len() == release.size_bytes {
                return Ok((dest, true));
            }
        }
    }

    let client = reqwest::Client::builder()
        .user_agent("archr-flasher")
        .build()
        .map_err(|e| format!("HTTP error: {}", e))?;

    let response = client
        .get(&release.download_url)
        .send()
        .await
        .map_err(|e| format!("Download error: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("Download failed: HTTP {}", response.status()));
    }

    let total = response.content_length().unwrap_or(release.size_bytes);

    // Write to .part file first, then rename (atomic)
    let temp = cache_dir.join(format!("{}.part", release.image_name));
    let mut file = File::create(&temp)
        .map_err(|e| format!("Cannot create file: {}", e))?;

    let mut downloaded: u64 = 0;
    let mut response = response;

    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("Download error: {}", e))?
    {
        file.write_all(&chunk)
            .map_err(|e| format!("Write error: {}", e))?;
        downloaded += chunk.len() as u64;

        let percent = if total > 0 {
            (downloaded as f64 / total as f64 * 100.0).min(100.0)
        } else {
            0.0
        };

        let _ = app.emit(
            "download-progress",
            DownloadProgress {
                percent,
                downloaded_bytes: downloaded,
                total_bytes: total,
            },
        );
    }

    file.flush().map_err(|e| format!("Flush error: {}", e))?;
    drop(file);

    // Rename .part → final name
    fs::rename(&temp, &dest)
        .map_err(|e| format!("Cannot finalize download: {}", e))?;

    Ok((dest, false))
}
