use std::io::{BufReader, Read, Write};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
struct FlashProgress {
    percent: f64,
    stage: String,
    /// Bytes already written/processed in the current stage. Zero when
    /// the byte counter is not available (e.g. while decompressing).
    bytes_done: u64,
    /// Total bytes for the current stage (image size during writing
    /// and verifying). Zero when unknown.
    bytes_total: u64,
}

pub(crate) fn emit_progress(app: &AppHandle, percent: f64, stage: &str) {
    emit_progress_bytes(app, percent, stage, 0, 0);
}

fn emit_progress_bytes(
    app: &AppHandle,
    percent: f64,
    stage: &str,
    bytes_done: u64,
    bytes_total: u64,
) {
    let _ = app.emit("flash-progress", FlashProgress {
        percent,
        stage: stage.to_string(),
        bytes_done,
        bytes_total,
    });
}

/// Re-validate the device is a valid removable disk before flashing.
/// Prevents writing to a device that was removed or swapped since selection.
pub(crate) fn validate_device(device: &str) -> Result<(), String> {
    if device.is_empty() {
        return Err("No device specified".into());
    }

    // Basic path validation: must be a block device path
    if !device.starts_with("/dev/") && !device.starts_with("\\\\.\\") {
        return Err(format!("Invalid device path: {}", device));
    }

    // On Linux/macOS: verify the device still exists
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if !Path::new(device).exists() {
            return Err("Device not found. Was the SD card removed?".into());
        }
    }

    // On Linux: verify it's still a removable disk (not system disk)
    #[cfg(target_os = "linux")]
    {
        let dev_name = device.trim_start_matches("/dev/");
        let sys_path = format!("/sys/block/{}/removable", dev_name);
        let removable = fs::read_to_string(&sys_path).unwrap_or_default();
        if removable.trim() != "1" {
            return Err(format!("{} is not a removable device", device));
        }
    }

    // On Windows: validate PhysicalDrive path format
    #[cfg(target_os = "windows")]
    {
        if !device.starts_with("\\\\.\\PhysicalDrive") {
            return Err(format!("Invalid device path: {}", device));
        }
        let disk_num = device.trim_start_matches("\\\\.\\PhysicalDrive");
        if disk_num.is_empty() || !disk_num.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("Invalid disk number in: {}", device));
        }
    }

    Ok(())
}

/// Check available temp space before XZ decompression.
/// XZ images decompress to ~3-4x their compressed size.
pub(crate) fn check_temp_space(image_path: &str) -> Result<(), String> {
    let src_size = fs::metadata(image_path)
        .map_err(|e| format!("Cannot read image: {}", e))?.len();
    let needed = src_size * 4;
    let temp_dir = std::env::temp_dir();
    let available = fs2::available_space(&temp_dir)
        .map_err(|e| format!("Cannot check disk space: {}", e))?;

    if available < needed {
        let need_gb = needed as f64 / 1_000_000_000.0;
        let have_gb = available as f64 / 1_000_000_000.0;
        return Err(format!(
            "Not enough temp space: need {:.1} GB, have {:.1} GB",
            need_gb, have_gb
        ));
    }
    Ok(())
}

/// Poll a progress file written by the flash script.
/// Supports two formats:
///   - Raw number: bytes written → maps to writing progress (55%..90%)
///   - "STAGE:verifying": switches to verification stage (90%..98%)
///   - "STAGE:done": emits 100%
pub(crate) fn poll_flash_progress(
    app: AppHandle,
    progress_file: PathBuf,
    image_size: u64,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(500));
            if let Ok(content) = fs::read_to_string(&progress_file) {
                let content = content.trim();
                if content.starts_with("STAGE:verifying:") {
                    let pct_str = content.trim_start_matches("STAGE:verifying:");
                    let verify_pct = pct_str.parse::<f64>().unwrap_or(0.0);
                    let pct = 90.0 + (verify_pct / 100.0 * 8.0); // 90% to 98%
                    emit_progress(&app, pct, "verifying");
                } else if content.starts_with("STAGE:testing") {
                    emit_progress(&app, 55.0, "testing_sd");
                } else if content.starts_with("STAGE:speed_slow:") {
                    let speed = content.trim_start_matches("STAGE:speed_slow:");
                    let _ = app.emit("sd-speed-result", serde_json::json!({
                        "quality": "slow",
                        "speed_mbs": speed.parse::<u32>().unwrap_or(0)
                    }));
                    emit_progress(&app, 56.0, "writing_safe");
                } else if content.starts_with("STAGE:speed_medium:") {
                    let speed = content.trim_start_matches("STAGE:speed_medium:");
                    let _ = app.emit("sd-speed-result", serde_json::json!({
                        "quality": "medium",
                        "speed_mbs": speed.parse::<u32>().unwrap_or(0)
                    }));
                    emit_progress(&app, 56.0, "writing");
                } else if content.starts_with("STAGE:speed_ok:") {
                    let speed = content.trim_start_matches("STAGE:speed_ok:");
                    let _ = app.emit("sd-speed-result", serde_json::json!({
                        "quality": "fast",
                        "speed_mbs": speed.parse::<u32>().unwrap_or(0)
                    }));
                    emit_progress(&app, 56.0, "writing");
                } else if content.starts_with("STAGE:done") {
                    emit_progress(&app, 98.0, "finalizing");
                } else if content.starts_with("VERIFY_FAILED") {
                    // Surface as a dedicated event so the UI shows a
                    // clear "verification failed" panel instead of the
                    // generic "Flash failed" path. The bash script
                    // also exits 1 right after writing this marker, so
                    // the Rust side will pick up the failure from the
                    // child process exit code too.
                    let detail = content.trim_start_matches("VERIFY_FAILED:");
                    let _ = app.emit("flash-verify-failed", serde_json::json!({
                        "detail": detail
                    }));
                    emit_progress(&app, 0.0, "verify_failed");
                } else if let Ok(bytes) = content.parse::<u64>() {
                    if image_size > 0 {
                        let pct = 55.0 + (bytes as f64 / image_size as f64 * 35.0).min(35.0);
                        emit_progress_bytes(&app, pct, "writing", bytes, image_size);
                    }
                }
            }
        }
    })
}


pub(crate) fn decompress_xz(app: &AppHandle, src: &str, dst: &Path) -> Result<(), String> {
    let src_file = File::open(src)
        .map_err(|e| format!("Cannot open image: {}", e))?;
    let src_size = src_file.metadata()
        .map_err(|e| format!("Metadata error: {}", e))?.len();
    let estimated_total = src_size * 3; // rough estimate for decompressed size

    let mut decoder = xz2::read::XzDecoder::new(BufReader::new(src_file));
    let mut dst_file = File::create(dst)
        .map_err(|e| format!("Cannot create temp file: {}", e))?;

    let mut buf = vec![0u8; 4 * 1024 * 1024]; // 4MB buffer
    let mut written: u64 = 0;

    loop {
        let n = decoder.read(&mut buf)
            .map_err(|e| format!("Decompress error: {}", e))?;
        if n == 0 { break; }

        dst_file.write_all(&buf[..n])
            .map_err(|e| format!("Write error: {}", e))?;
        written += n as u64;

        // 0-55% for decompression phase
        let pct = (written as f64 / estimated_total as f64 * 55.0).min(55.0);
        emit_progress(app, pct, "decompressing");
    }

    dst_file.flush().map_err(|e| format!("Flush error: {}", e))?;
    Ok(())
}

pub(crate) fn decompress_gz(app: &AppHandle, src: &str, dst: &Path) -> Result<(), String> {
    let src_file = File::open(src)
        .map_err(|e| format!("Cannot open image: {}", e))?;
    let src_size = src_file.metadata()
        .map_err(|e| format!("Metadata error: {}", e))?.len();
    let estimated_total = src_size * 3;

    let mut decoder = flate2::read::GzDecoder::new(BufReader::new(src_file));
    let mut dst_file = File::create(dst)
        .map_err(|e| format!("Cannot create temp file: {}", e))?;

    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut written: u64 = 0;

    loop {
        let n = decoder.read(&mut buf)
            .map_err(|e| format!("Decompress error: {}", e))?;
        if n == 0 { break; }

        dst_file.write_all(&buf[..n])
            .map_err(|e| format!("Write error: {}", e))?;
        written += n as u64;

        let pct = (written as f64 / estimated_total as f64 * 55.0).min(55.0);
        emit_progress(app, pct, "decompressing");
    }

    dst_file.flush().map_err(|e| format!("Flush error: {}", e))?;
    Ok(())
}


// ---------------------------------------------------------------------------
// Per-OS implementations live in flash_linux / flash_macos / flash_windows.
// Re-export the target platform's flash_image_privileged so callers keep
// using `flash::flash_image_privileged` unchanged.
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
pub use crate::flash_linux::flash_image_privileged;
#[cfg(target_os = "macos")]
pub use crate::flash_macos::flash_image_privileged;
#[cfg(target_os = "windows")]
pub use crate::flash_windows::flash_image_privileged;
