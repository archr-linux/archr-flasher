use serde::{Deserialize, Serialize};
use sha2::{Sha256, Digest};
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter};

const REPO_API: &str = "https://api.github.com/repos/archr-linux/Arch-R/releases/latest";

#[derive(Debug, Clone, Serialize)]
pub struct ReleaseInfo {
    pub version: String,
    pub image_name: String,
    pub download_url: String,
    pub size_bytes: u64,
    pub expected_sha256: Option<String>,
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

pub async fn get_latest_release(variant: &str) -> Result<ReleaseInfo, String> {
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

    // Match image by variant. Asset names may use either format:
    //   v1: ArchR-R36S-YYYYMMDD.img.gz / ArchR-R36S-clone-YYYYMMDD.img.gz
    //   v2: ArchR-R36S.aarch64-YYYYMMDD-original.img.gz / ...-clone.img.gz
    let asset = release
        .assets
        .iter()
        .find(|a| {
            let is_image = a.name.ends_with(".img.xz") || a.name.ends_with(".img.gz");
            if !is_image {
                return false;
            }
            let name = &a.name;
            match variant {
                "clone" => name.contains("-clone"),
                _ => name.starts_with("ArchR-R36S") && !name.contains("-clone"),
            }
        })
        .ok_or_else(|| format!("No image found for '{}' in latest release", variant))?;

    // Look for a matching .sha256 asset (e.g. "image.img.xz.sha256")
    let sha256_asset_name = format!("{}.sha256", asset.name);
    let expected_sha256 = if let Some(hash_asset) = release.assets.iter()
        .find(|a| a.name == sha256_asset_name)
    {
        // Download the small .sha256 file
        match client.get(&hash_asset.browser_download_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                resp.text().await.ok()
                    .map(|t| t.split_whitespace().next().unwrap_or("").trim().to_string())
                    .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
            }
            _ => None,
        }
    } else {
        None
    };

    Ok(ReleaseInfo {
        version: release.tag_name,
        image_name: asset.name.clone(),
        download_url: asset.browser_download_url.clone(),
        size_bytes: asset.size,
        expected_sha256,
    })
}

/// Verify SHA256 of a file against an expected hash.
fn verify_sha256(path: &Path, expected: &str) -> bool {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 4 * 1024 * 1024];

    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return false,
        }
    }

    let hash = format!("{:x}", hasher.finalize());
    hash == expected
}

/// Compute SHA256 of a file with progress reporting.
/// Returns the hex-encoded hash string.
pub fn verify_sha256_with_progress(app: &AppHandle, path: &Path) -> Result<String, String> {
    let file = File::open(path)
        .map_err(|e| format!("Cannot open file: {}", e))?;
    let total = file.metadata()
        .map_err(|e| format!("Metadata error: {}", e))?.len();
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut read_bytes: u64 = 0;

    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
                read_bytes += n as u64;
                let percent = if total > 0 {
                    (read_bytes as f64 / total as f64 * 100.0).min(100.0)
                } else { 0.0 };
                let _ = app.emit("verification-progress", serde_json::json!({
                    "percent": percent,
                    "stage": "verifying_download"
                }));
            }
            Err(e) => return Err(format!("Read error: {}", e)),
        }
    }

    Ok(format!("{:x}", hasher.finalize()))
}

/// Download the image to cache_dir, emitting progress events.
/// Computes SHA256 incrementally during download.
/// Returns (file_path, was_cached).
pub async fn download_image(
    app: &AppHandle,
    release: &ReleaseInfo,
    cache_dir: &Path,
) -> Result<(PathBuf, bool), String> {
    fs::create_dir_all(cache_dir)
        .map_err(|e| format!("Cannot create cache dir: {}", e))?;

    let dest = cache_dir.join(&release.image_name);
    let hash_path = cache_dir.join(format!("{}.sha256", release.image_name));

    // Check if already cached (same name + matching size)
    if dest.exists() {
        if let Ok(meta) = fs::metadata(&dest) {
            if meta.len() == release.size_bytes {
                // If we have a stored hash, verify integrity
                if let Ok(stored_hash) = fs::read_to_string(&hash_path) {
                    let stored_hash = stored_hash.trim().to_string();
                    if !stored_hash.is_empty() {
                        let _ = app.emit("download-progress", DownloadProgress {
                            percent: 0.0,
                            downloaded_bytes: 0,
                            total_bytes: meta.len(),
                        });

                        if verify_sha256(&dest, &stored_hash) {
                            return Ok((dest, true));
                        }
                        // Hash mismatch: delete and re-download
                        let _ = fs::remove_file(&dest);
                        let _ = fs::remove_file(&hash_path);
                    }
                } else {
                    // No hash file (backward compat) — trust size match
                    return Ok((dest, true));
                }
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

    let mut hasher = Sha256::new();
    let mut downloaded: u64 = 0;
    let mut response = response;

    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("Download error: {}", e))?
    {
        file.write_all(&chunk)
            .map_err(|e| format!("Write error: {}", e))?;
        hasher.update(&chunk);
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

    // Compute final hash and save
    let hash = format!("{:x}", hasher.finalize());
    let _ = fs::write(&hash_path, &hash);

    // Verify against expected hash from release (if available)
    if let Some(ref expected) = release.expected_sha256 {
        if hash != *expected {
            let _ = fs::remove_file(&temp);
            let _ = fs::remove_file(&hash_path);
            return Err(format!(
                "Download corrupted: SHA256 mismatch\nExpected: {}\nGot: {}",
                expected, hash
            ));
        }
    }

    // Rename .part → final name
    fs::rename(&temp, &dest)
        .map_err(|e| format!("Cannot finalize download: {}", e))?;

    Ok((dest, false))
}
