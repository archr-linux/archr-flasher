// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod disk;
mod flash;
mod github;
mod panels;

use disk::DiskInfo;
use github::{DownloadResult, ReleaseInfo};
use panels::Panel;
use tauri::Manager;

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
async fn check_latest_release() -> Result<ReleaseInfo, String> {
    github::get_latest_release().await
}

/// Download the latest image to local cache (or return cached path).
#[tauri::command]
async fn download_image(app: tauri::AppHandle) -> Result<DownloadResult, String> {
    let release = github::get_latest_release().await?;

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
#[tauri::command]
async fn flash_image(
    app: tauri::AppHandle,
    image_path: String,
    device: String,
    panel_dtb: String,
    panel_id: String,
    variant: String,
) -> Result<String, String> {
    let app_clone = app.clone();

    tokio::task::spawn_blocking(move || {
        flash::flash_image_privileged(
            &app_clone,
            &image_path,
            &device,
            &panel_dtb,
            &panel_id,
            &variant,
        )
    })
    .await
    .map_err(|e| format!("Task error: {}", e))??;

    Ok("Flash complete".into())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            get_locale,
            get_panels,
            list_disks,
            check_latest_release,
            download_image,
            flash_image,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
