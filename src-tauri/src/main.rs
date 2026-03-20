// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod disk;
mod dtbo_builder;
mod flash;
mod github;
mod overlay;
mod panel_config;
mod panels;

use disk::DiskInfo;
use github::{DownloadResult, ReleaseInfo};
use panel_config::PanelConfig;
use panels::Panel;
use tauri::Manager;
use tauri_plugin_updater::UpdaterExt;

/// Returns the OS locale (e.g. "pt-BR", "en-US") for i18n.
#[tauri::command]
fn get_locale() -> String {
    sys_locale::get_locale().unwrap_or_else(|| "en".to_string())
}

#[tauri::command]
fn get_panels(console: &str) -> Vec<Panel> {
    panels::get_panels(console)
}

#[tauri::command]
fn list_disks() -> Vec<DiskInfo> {
    disk::list_removable_disks()
}

#[tauri::command]
async fn check_latest_release(variant: String) -> Result<ReleaseInfo, String> {
    github::get_latest_release(&variant).await
}

/// Download the latest image to local cache (or return cached path).
#[tauri::command]
async fn download_image(app: tauri::AppHandle, variant: String) -> Result<DownloadResult, String> {
    let release = github::get_latest_release(&variant).await?;

    let cache_dir = app.path().app_cache_dir()
        .map_err(|e| format!("Cache dir error: {}", e))?;

    let (path, cached) = github::download_image(&app, &release, &cache_dir).await?;

    Ok(DownloadResult {
        path: path.to_string_lossy().to_string(),
        version: release.version,
        image_name: release.image_name,
        cached,
    })
}

/// Flash image to SD card with privilege escalation.
/// Builds custom DTBO from panel + config before calling privileged script.
#[tauri::command]
async fn flash_image(
    app: tauri::AppHandle,
    image_path: String,
    device: String,
    panel_dtbo: String,
    variant: String,
    rotation: u32,
    invert_left_stick: bool,
    invert_right_stick: bool,
    hp_invert: bool,
) -> Result<String, String> {
    let app_clone = app.clone();

    // Use app cache dir (user's data disk) instead of /tmp (OS disk, often small)
    let cache_dir = app.path().app_cache_dir()
        .map_err(|e| format!("Cache dir error: {}", e))?;

    tokio::task::spawn_blocking(move || {
        // 1. Determine decompressed image path (for reading DTBO from FAT32)
        let img_path = std::path::Path::new(&image_path);

        // 2. Read source DTBO from image
        //    Compressed images (.xz, .gz) must be decompressed first to read FAT32.
        let is_compressed = image_path.ends_with(".xz") || image_path.ends_with(".gz");
        let dtbo_bytes = if is_compressed {
            let _ = std::fs::create_dir_all(&cache_dir);
            let temp_img = cache_dir.join("archr-flash-temp.img");
            if temp_img.exists() {
                panel_config::read_dtbo_from_image(&temp_img, &panel_dtbo)?
            } else {
                use std::io::{BufReader, Read, Write};
                let src_file = std::fs::File::open(&image_path)
                    .map_err(|e| format!("Cannot open image: {}", e))?;
                let mut dst_file = std::fs::File::create(&temp_img)
                    .map_err(|e| format!("Cannot create temp: {}", e))?;
                let mut buf = vec![0u8; 4 * 1024 * 1024];

                if image_path.ends_with(".xz") {
                    let mut decoder = xz2::read::XzDecoder::new(BufReader::new(src_file));
                    loop {
                        let n = decoder.read(&mut buf)
                            .map_err(|e| format!("Decompress error: {}", e))?;
                        if n == 0 { break; }
                        dst_file.write_all(&buf[..n])
                            .map_err(|e| format!("Write error: {}", e))?;
                    }
                } else {
                    let mut decoder = flate2::read::GzDecoder::new(BufReader::new(src_file));
                    loop {
                        let n = decoder.read(&mut buf)
                            .map_err(|e| format!("Decompress error: {}", e))?;
                        if n == 0 { break; }
                        dst_file.write_all(&buf[..n])
                            .map_err(|e| format!("Write error: {}", e))?;
                    }
                }

                dst_file.flush().map_err(|e| format!("Flush: {}", e))?;
                panel_config::read_dtbo_from_image(&temp_img, &panel_dtbo)?
            }
        } else {
            panel_config::read_dtbo_from_image(img_path, &panel_dtbo)?
        };

        // 3. Build DTBO: use original when no customizations, custom when needed
        let config = PanelConfig {
            rotation,
            invert_left_stick,
            invert_right_stick,
            hp_invert,
        };

        let final_dtbo = if config.is_default() {
            // No customizations — use the original DTBO as-is (preserves all
            // hardware nodes: reset-gpios, pinctrl, power supply, __fixups__)
            dtbo_bytes
        } else {
            // Clone original DTBO and inject customization properties
            panel_config::build_custom_dtbo(&dtbo_bytes, &config)?
        };

        // 4. Write to temp file
        let dtbo_path = panel_config::write_temp_dtbo(&final_dtbo)?;

        // 5. Use decompressed image if we already created it (avoids double decompress)
        let flash_image_path = if is_compressed {
            let temp_img = cache_dir.join("archr-flash-temp.img");
            if temp_img.exists() {
                temp_img.to_string_lossy().to_string()
            } else {
                image_path.clone()
            }
        } else {
            image_path.clone()
        };

        // 6. Flash with privileged script
        flash::flash_image_privileged(
            &app_clone,
            &flash_image_path,
            &device,
            dtbo_path.to_str().unwrap_or(""),
            &variant,
        )?;

        // 7. Cleanup temp files
        let _ = std::fs::remove_file(&dtbo_path);
        if is_compressed {
            let _ = std::fs::remove_file(cache_dir.join("archr-flash-temp.img"));
        }

        Ok("Flash complete".into())
    })
    .await
    .map_err(|e| format!("Task error: {}", e))?
}

// ---- Overlay Tab Commands ----

#[tauri::command]
fn find_archr_sd() -> Vec<String> {
    overlay::find_archr_partitions()
}

#[tauri::command]
fn read_overlay(boot_path: &str) -> overlay::OverlayStatus {
    overlay::read_overlay_status(boot_path)
}

#[tauri::command]
fn apply_panel_with_config(
    boot_path: &str,
    panel_dtbo: &str,
    _variant: &str,
    rotation: u32,
    invert_left_stick: bool,
    invert_right_stick: bool,
    hp_invert: bool,
) -> Result<String, String> {
    let config = PanelConfig {
        rotation,
        invert_left_stick,
        invert_right_stick,
        hp_invert,
    };
    let result = overlay::apply_overlay_with_config(boot_path, panel_dtbo, &config)?;
    Ok(result)
}

/// Check if a new version of the Flasher app is available.
#[tauri::command]
async fn check_app_update(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let update = app.updater_builder()
        .build()
        .map_err(|e| format!("{}", e))?
        .check()
        .await
        .map_err(|e| format!("{}", e))?;

    match update {
        Some(u) => Ok(Some(format!("{}|{}", u.version, u.body.unwrap_or_default()))),
        None => Ok(None),
    }
}

/// Download and install the app update, then restart.
#[tauri::command]
async fn install_app_update(app: tauri::AppHandle) -> Result<(), String> {
    let update = app.updater_builder()
        .build()
        .map_err(|e| format!("{}", e))?
        .check()
        .await
        .map_err(|e| format!("{}", e))?;

    if let Some(update) = update {
        update.download_and_install(|_, _| {}, || {})
            .await
            .map_err(|e| format!("{}", e))?;
        app.restart();
    }

    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            #[cfg(desktop)]
            app.handle().plugin(
                tauri_plugin_updater::Builder::new().build()
            )?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_locale,
            get_panels,
            list_disks,
            check_latest_release,
            download_image,
            flash_image,
            find_archr_sd,
            read_overlay,
            apply_panel_with_config,
            check_app_update,
            install_app_update,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}