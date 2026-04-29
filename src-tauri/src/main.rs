// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod disk;
mod dtb_to_overlay;
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
fn get_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
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

/// Verify integrity of a local image file by computing its SHA256.
/// If expected_hash is provided, compares against it.
#[tauri::command]
async fn verify_image(
    app: tauri::AppHandle,
    image_path: String,
    expected_hash: Option<String>,
) -> Result<String, String> {
    let path = std::path::Path::new(&image_path);
    if !path.exists() {
        return Err("Image file not found".into());
    }

    let app_clone = app.clone();
    let path_clone = image_path.clone();
    let hash = tokio::task::spawn_blocking(move || {
        github::verify_sha256_with_progress(&app_clone, std::path::Path::new(&path_clone))
    }).await
        .map_err(|e| format!("Task error: {}", e))??;

    if let Some(expected) = expected_hash {
        if hash != expected {
            return Err(format!(
                "SHA256 mismatch\nExpected: {}\nGot: {}",
                expected, hash
            ));
        }
    }

    Ok(hash)
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
    joypad_variant: panel_config::JoypadVariant,
    force_simple_audio: bool,
    skip_vendor_mode: bool,
) -> Result<String, String> {
    let app_clone = app.clone();

    // Use app cache dir (user's data disk) instead of /tmp (OS disk, often small)
    let cache_dir = app.path().app_cache_dir()
        .map_err(|e| format!("Cache dir error: {}", e))?;

    tokio::task::spawn_blocking(move || {
        // 1. Determine decompressed image path (for reading DTBO from FAT32)
        let img_path = std::path::Path::new(&image_path);

        // 2a. Build the config first so we can pick the right pre-built variant.
        let config = PanelConfig {
            rotation,
            invert_left_stick,
            invert_right_stick,
            hp_invert,
            joypad_variant,
            force_simple_audio,
            skip_vendor_mode,
        };

        // 2b. Resolve the variant DTBO path (joypad/audio overrides live in
        //     pre-built files; rotation/sticks/HPi/Dno are runtime patches).
        let variant_path = panel_config::variant_dtbo_path(&panel_dtbo, &config);

        // 2c. Custom overlay flow: when the user picked "Custom" the frontend
        //     sends an absolute filesystem path to a generated DTBO instead of
        //     a panel name relative to overlays/. Read it directly from disk
        //     and skip the image lookup entirely.
        let custom_dtbo_path = std::path::Path::new(&panel_dtbo);
        if custom_dtbo_path.is_absolute() && custom_dtbo_path.is_file() {
            let dtbo_bytes = std::fs::read(custom_dtbo_path)
                .map_err(|e| format!("Cannot read custom overlay: {}", e))?;
            let final_dtbo = if config.is_default() {
                dtbo_bytes
            } else {
                panel_config::build_custom_dtbo(&dtbo_bytes, &config)?
            };
            let dtbo_path = panel_config::write_temp_dtbo(&final_dtbo)?;

            let is_compressed = image_path.ends_with(".xz") || image_path.ends_with(".gz");
            let flash_image_path = if is_compressed {
                let _ = std::fs::create_dir_all(&cache_dir);
                let temp_img = cache_dir.join("archr-flash-temp.img");
                if temp_img.exists() {
                    let stale = match (std::fs::metadata(&image_path), std::fs::metadata(&temp_img)) {
                        (Ok(src), Ok(cached)) => match (src.modified(), cached.modified()) {
                            (Ok(s), Ok(c)) => s > c,
                            _ => false,
                        },
                        _ => false,
                    };
                    if stale { let _ = std::fs::remove_file(&temp_img); }
                }
                if !temp_img.exists() {
                    use std::io::{BufReader, Read, Write};
                    let src_file = std::fs::File::open(&image_path)
                        .map_err(|e| format!("Cannot open image: {}", e))?;
                    let mut dst_file = std::fs::File::create(&temp_img)
                        .map_err(|e| format!("Cannot create temp: {}", e))?;
                    let mut buf = vec![0u8; 4 * 1024 * 1024];
                    if image_path.ends_with(".xz") {
                        let mut decoder = xz2::read::XzDecoder::new(BufReader::new(src_file));
                        loop {
                            let n = decoder.read(&mut buf).map_err(|e| format!("Decompress error: {}", e))?;
                            if n == 0 { break; }
                            dst_file.write_all(&buf[..n]).map_err(|e| format!("Write error: {}", e))?;
                        }
                    } else {
                        let mut decoder = flate2::read::GzDecoder::new(BufReader::new(src_file));
                        loop {
                            let n = decoder.read(&mut buf).map_err(|e| format!("Decompress error: {}", e))?;
                            if n == 0 { break; }
                            dst_file.write_all(&buf[..n]).map_err(|e| format!("Write error: {}", e))?;
                        }
                    }
                    dst_file.flush().map_err(|e| format!("Flush: {}", e))?;
                }
                temp_img.to_string_lossy().to_string()
            } else {
                image_path.clone()
            };

            flash::flash_image_privileged(
                &app_clone,
                &flash_image_path,
                &device,
                dtbo_path.to_str().unwrap_or(""),
                &variant,
            )?;

            let _ = std::fs::remove_file(&dtbo_path);
            if is_compressed {
                let _ = std::fs::remove_file(cache_dir.join("archr-flash-temp.img"));
            }
            return Ok("Flash complete".into());
        }

        // 2d. Read source DTBO from image. If the requested variant doesn't
        //     exist on the image (older builds), fall back to the base panel
        //     name so the user still gets a flash.
        let is_compressed = image_path.ends_with(".xz") || image_path.ends_with(".gz");
        let read_with_fallback = |img: &std::path::Path| -> Result<Vec<u8>, String> {
            match panel_config::read_dtbo_from_image(img, &variant_path) {
                Ok(b) => Ok(b),
                Err(_) if variant_path != panel_dtbo => {
                    panel_config::read_dtbo_from_image(img, &panel_dtbo)
                }
                Err(e) => Err(e),
            }
        };
        let dtbo_bytes = if is_compressed {
            let _ = std::fs::create_dir_all(&cache_dir);
            let temp_img = cache_dir.join("archr-flash-temp.img");

            // Invalidate the cached decompression when the source archive is
            // newer (rebuilt with new overlay names) or when the user picked
            // a different image entirely. Without this guard the flasher
            // happily reads stale FAT contents — e.g. asking for
            // "R36S-V22_2024-12-18.dtbo" on a temp.img still holding
            // "panel0.dtbo".
            if temp_img.exists() {
                let stale = match (std::fs::metadata(&image_path), std::fs::metadata(&temp_img)) {
                    (Ok(src), Ok(cached)) => match (src.modified(), cached.modified()) {
                        (Ok(s), Ok(c)) => s > c,
                        _ => false,
                    },
                    _ => false,
                };
                if stale {
                    let _ = std::fs::remove_file(&temp_img);
                }
            }

            if temp_img.exists() {
                read_with_fallback(&temp_img)?
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
                read_with_fallback(&temp_img)?
            }
        } else {
            read_with_fallback(img_path)?
        };

        // 3. Build DTBO: use original when no customizations, custom when needed
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
    variant: &str,
    rotation: u32,
    invert_left_stick: bool,
    invert_right_stick: bool,
    hp_invert: bool,
    joypad_variant: panel_config::JoypadVariant,
    force_simple_audio: bool,
    skip_vendor_mode: bool,
) -> Result<String, String> {
    let config = PanelConfig {
        rotation,
        invert_left_stick,
        invert_right_stick,
        hp_invert,
        joypad_variant,
        force_simple_audio,
        skip_vendor_mode,
    };
    let result = overlay::apply_overlay_with_config(boot_path, panel_dtbo, &config)?;

    // Switch extlinux config for soysauce variant
    overlay::switch_extlinux_for_variant(boot_path, variant);

    Ok(result)
}

/// Generate a panel overlay (DTBO) from a user-provided stock DTB file.
/// Uses the bundled archr-dtbo.py script (requires Python 3 + fdt package).
/// Generate a panel overlay (DTBO) from a user-provided stock DTB file.
/// Pure Rust implementation — no Python, no external dependencies.
#[tauri::command]
async fn generate_overlay_from_dtb(
    app: tauri::AppHandle,
    dtb_path: String,
    _flags: Option<String>,
) -> Result<String, String> {
    let dtb_data = std::fs::read(&dtb_path)
        .map_err(|e| format!("Cannot read DTB file: {}", e))?;

    let dtbo_data = dtb_to_overlay::generate_overlay(&dtb_data)?;

    let cache_dir = app.path().app_cache_dir()
        .map_err(|e| format!("Cache dir error: {}", e))?;
    let _ = std::fs::create_dir_all(&cache_dir);
    let output_path = cache_dir.join("custom-overlay.dtbo");

    std::fs::write(&output_path, &dtbo_data)
        .map_err(|e| format!("Failed to write overlay: {}", e))?;

    Ok(output_path.to_string_lossy().to_string())
}

/// Apply a custom-generated overlay (from DTB) to an existing SD card.
#[tauri::command]
fn apply_custom_overlay(
    boot_path: &str,
    dtbo_path: &str,
) -> Result<String, String> {
    let boot = std::path::Path::new(boot_path);
    let source = std::path::Path::new(dtbo_path);
    let target = boot.join("overlays/mipi-panel.dtbo");

    if !source.exists() {
        return Err("Custom overlay file not found.".into());
    }

    std::fs::copy(source, &target)
        .map_err(|e| format!("Failed to copy overlay: {}", e))?;

    // fsync the file and parent directory
    if let Ok(f) = std::fs::File::open(&target) {
        let _ = f.sync_all();
    }
    if let Ok(d) = std::fs::File::open(target.parent().unwrap()) {
        let _ = d.sync_all();
    }

    Ok("Custom overlay applied.".into())
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
    // WebKitGTK EGL workaround for AppImage on Wayland.
    // The AppImage bundles an older libwayland-client.so that conflicts with
    // the system Mesa driver, causing "Could not create default EGL display:
    // EGL_BAD_PARAMETER". Fix: preload the system's libwayland-client.so.
    // LD_PRELOAD must be set before process start, so we re-exec ourselves.
    #[cfg(target_os = "linux")]
    {
        if std::env::var("ARCHR_FLASHER_REEXEC").is_err()
            && std::env::var("APPIMAGE").is_ok()
        {
            let wayland_libs = [
                "/usr/lib/libwayland-client.so",
                "/usr/lib64/libwayland-client.so",
                "/usr/lib/x86_64-linux-gnu/libwayland-client.so",
            ];
            for lib in &wayland_libs {
                if std::path::Path::new(lib).exists() {
                    let current_preload = std::env::var("LD_PRELOAD").unwrap_or_default();
                    if !current_preload.contains("libwayland-client") {
                        let new_preload = if current_preload.is_empty() {
                            lib.to_string()
                        } else {
                            format!("{}:{}", lib, current_preload)
                        };
                        // SAFETY: this runs before any threads are spawned,
                        // and we immediately re-exec the process.
                        unsafe {
                            std::env::set_var("LD_PRELOAD", &new_preload);
                            std::env::set_var("ARCHR_FLASHER_REEXEC", "1");
                        }
                        use std::os::unix::process::CommandExt;
                        let exe = std::env::current_exe().expect("Cannot get exe path");
                        let args: Vec<String> = std::env::args().skip(1).collect();
                        let err = std::process::Command::new(&exe)
                            .args(&args)
                            .exec();
                        eprintln!("Re-exec failed: {}", err);
                    }
                    break;
                }
            }
        }
    }

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
            get_version,
            get_panels,
            list_disks,
            check_latest_release,
            download_image,
            verify_image,
            flash_image,
            find_archr_sd,
            read_overlay,
            apply_panel_with_config,
            generate_overlay_from_dtb,
            apply_custom_overlay,
            check_app_update,
            install_app_update,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}