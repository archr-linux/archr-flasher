//! Overlay management: read/write panel overlays on BOOT partition.
//! Detects mounted Arch R SD cards and allows changing the active panel overlay.

use crate::panel_config::{self, PanelConfig};
use crate::panels;
use md5::{Md5, Digest};
use serde::Serialize;
use std::fs;
use std::io::Write;
use std::path::Path;

#[derive(Clone, Serialize)]
pub struct OverlayStatus {
    pub boot_path: String,
    pub has_archr: bool,
    pub current_overlay: Option<String>,
    pub current_panel_name: Option<String>,
    pub variant: Option<String>,
    pub rotation: u32,
    pub invert_left_stick: bool,
    pub invert_right_stick: bool,
    pub hp_invert: bool,
}

/// Marker files that identify an Arch R BOOT partition.
const ARCHR_MARKERS: &[&str] = &["KERNEL"];
const ARCHR_DIRS: &[&str] = &["dtbs", "overlays"];

/// Find mounted Arch R BOOT partitions.
pub fn find_archr_partitions() -> Vec<String> {
    let mut results = Vec::new();

    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();

    for line in mounts.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let mount_point = parts[1];
        let fs_type = parts[2];

        if fs_type != "vfat" {
            continue;
        }

        if is_archr_boot(Path::new(mount_point)) {
            results.push(mount_point.to_string());
        }
    }

    results
}

fn is_archr_boot(path: &Path) -> bool {
    for marker in ARCHR_MARKERS {
        if !path.join(marker).exists() {
            return false;
        }
    }
    for dir in ARCHR_DIRS {
        if !path.join(dir).is_dir() {
            return false;
        }
    }
    true
}

/// Read the current overlay status from a BOOT partition.
pub fn read_overlay_status(boot_path: &str) -> OverlayStatus {
    let boot = Path::new(boot_path);

    if !is_archr_boot(boot) {
        return OverlayStatus {
            boot_path: boot_path.to_string(),
            has_archr: false,
            current_overlay: None,
            current_panel_name: None,
            variant: None,
            rotation: 0,
            invert_left_stick: false,
            invert_right_stick: false,
            hp_invert: false,
        };
    }

    let mipi_path = boot.join("overlays/mipi-panel.dtbo");
    let variant_path = boot.join("variant");

    let variant = fs::read_to_string(variant_path)
        .ok()
        .map(|s| s.trim().to_string());

    let (current_overlay, current_panel_name, config) = if mipi_path.exists() {
        let (overlay, name) = identify_overlay(&mipi_path);
        let cfg = fs::read(&mipi_path)
            .map(|data| panel_config::extract_config(&data))
            .unwrap_or_default();
        (overlay, name, cfg)
    } else {
        (None, None, PanelConfig::default())
    };

    OverlayStatus {
        boot_path: boot_path.to_string(),
        has_archr: true,
        current_overlay,
        current_panel_name,
        variant,
        rotation: config.rotation,
        invert_left_stick: config.invert_left_stick,
        invert_right_stick: config.invert_right_stick,
        hp_invert: config.hp_invert,
    }
}

/// Identify current overlay by comparing panel_description MD5 with known panels.
/// Uses panel_description (not full DTBO hash) so customized DTBOs still match.
fn identify_overlay(mipi_path: &Path) -> (Option<String>, Option<String>) {
    let data = match fs::read(mipi_path) {
        Ok(d) => d,
        Err(_) => return (None, None),
    };

    // Extract panel_description from the active mipi-panel.dtbo
    let mipi_desc_hash = match panel_config::extract_panel_description(&data) {
        Ok(desc) => format!("{:x}", Md5::digest(&desc)),
        Err(_) => {
            // Fallback: hash the whole file
            let hash = format!("{:x}", Md5::digest(&data));
            return (Some(format!("custom ({})", &hash[..8])), Some("Custom overlay".to_string()));
        }
    };

    let boot_dir = mipi_path.parent().and_then(|p| p.parent());
    let overlays_dir = match boot_dir {
        Some(b) => b.join("overlays"),
        None => return (Some(mipi_desc_hash), None),
    };

    // Check all known panels from both sets
    let all_panels: Vec<panels::Panel> = panels::get_panels("original")
        .into_iter()
        .chain(panels::get_panels("clone"))
        .collect();

    for panel in &all_panels {
        let dtbo_path = overlays_dir.join(&panel.dtbo);
        if let Ok(panel_data) = fs::read(&dtbo_path) {
            if let Ok(panel_desc) = panel_config::extract_panel_description(&panel_data) {
                let panel_hash = format!("{:x}", Md5::digest(&panel_desc));
                if panel_hash == mipi_desc_hash {
                    return (Some(panel.dtbo.clone()), Some(panel.name.clone()));
                }
            }
        }
    }

    (Some(format!("custom ({})", &mipi_desc_hash[..8])), Some("Custom overlay".to_string()))
}

/// Apply a panel overlay with customizations: read source DTBO, extract
/// panel_description, build custom DTBO with config baked in, write as mipi-panel.dtbo.
pub fn apply_overlay_with_config(
    boot_path: &str,
    panel_dtbo: &str,
    config: &PanelConfig,
) -> Result<String, String> {
    let boot = Path::new(boot_path);

    if !is_archr_boot(boot) {
        return Err("Not an Arch R BOOT partition".to_string());
    }

    let source = boot.join("overlays").join(panel_dtbo);
    let target = boot.join("overlays/mipi-panel.dtbo");

    if !source.exists() {
        return Err(format!("Panel overlay not found: {}", panel_dtbo));
    }

    let source_data = fs::read(&source)
        .map_err(|e| format!("Failed to read {}: {}", panel_dtbo, e))?;

    // Use original DTBO when no customizations (preserves all hardware nodes:
    // reset-gpios, pinctrl, power supply, __fixups__). Only build custom DTBO
    // when rotation, stick inversion, or HP invert are set.
    let final_dtbo = if config.is_default() {
        source_data.clone()
    } else {
        // Clone original DTBO and inject customization properties
        panel_config::build_custom_dtbo(&source_data, config)?
    };

    // Write with explicit fsync (FAT32!)
    let mut file = fs::File::create(&target)
        .map_err(|e| format!("Failed to create mipi-panel.dtbo: {}", e))?;
    file.write_all(&final_dtbo)
        .map_err(|e| format!("Failed to write mipi-panel.dtbo: {}", e))?;
    file.sync_all()
        .map_err(|e| format!("Failed to sync: {}", e))?;

    // Fsync parent directory (FAT32 requirement)
    if let Ok(dir) = fs::File::open(boot.join("overlays")) {
        let _ = dir.sync_all();
    }

    Ok(format!("Applied {} as mipi-panel.dtbo", panel_dtbo))
}

