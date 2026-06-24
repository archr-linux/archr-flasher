//! Overlay management: read/write panel overlays on BOOT partition.
//! Detects mounted Arch R SD cards and reports the current overlay file.

use md5::{Md5, Digest};
use serde::Serialize;
use std::fs;
use std::path::Path;

#[derive(Clone, Serialize)]
pub struct OverlayStatus {
    pub boot_path: String,
    pub has_archr: bool,
    /// Short identifier of the active overlay (hash prefix or "default").
    /// We no longer try to match it against a built-in panel catalogue
    /// since the flasher dropped the in-app overlay generator; users
    /// pick their own mipi-panel.dtbo from the web generator and we
    /// have no canonical list to compare against.
    pub current_overlay: Option<String>,
    pub current_panel_name: Option<String>,
    pub variant: Option<String>,
}

/// Marker files that identify an Arch R BOOT partition.
const ARCHR_MARKERS: &[&str] = &["KERNEL"];
const ARCHR_DIRS: &[&str] = &["dtbs", "overlays"];

/// Find mounted Arch R BOOT partitions.
pub fn find_archr_partitions() -> Vec<String> {
    let mut results = Vec::new();

    #[cfg(not(target_os = "windows"))]
    {
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
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        use std::process::Command;
        const CREATE_NO_WINDOW: u32 = 0x08000000;

        // Use PowerShell to list all mounted volumes with drive letters and mount points
        let output = Command::new("powershell")
            .args([
                "-NoProfile", "-Command",
                r#"Get-Volume | Where-Object { $_.DriveType -eq 'Removable' -or $_.DriveType -eq 'Fixed' } | ForEach-Object { $dl = $_.DriveLetter; if ($dl) { Write-Output "${dl}:\" } }; Get-CimInstance Win32_Volume | Where-Object { $_.DriveLetter -or $_.Name } | ForEach-Object { if ($_.DriveLetter) { Write-Output "$($_.DriveLetter)\" } elseif ($_.Name) { Write-Output $_.Name } }"#,
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output();

        let mut seen = std::collections::HashSet::new();

        // Parse PowerShell output (one path per line)
        if let Ok(o) = output {
            let stdout = String::from_utf8_lossy(&o.stdout);
            for line in stdout.lines() {
                let p = line.trim();
                if p.is_empty() || seen.contains(p) {
                    continue;
                }
                seen.insert(p.to_string());
                if is_archr_boot(Path::new(p)) {
                    results.push(p.to_string());
                }
            }
        }

        // Fallback: scan all drive letters in case PowerShell failed
        if results.is_empty() {
            for letter in b'A'..=b'Z' {
                let drive = format!("{}:\\", letter as char);
                if seen.contains(&drive) {
                    continue;
                }
                let path = Path::new(&drive);
                if path.exists() && is_archr_boot(path) {
                    results.push(drive);
                }
            }
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
        };
    }

    let mipi_path = boot.join("overlays/mipi-panel.dtbo");
    let variant_path = boot.join("variant");

    let variant = fs::read_to_string(variant_path)
        .ok()
        .map(|s| s.trim().to_string());

    let (current_overlay, current_panel_name) = if mipi_path.exists() {
        // The hash prefix gives the user a sanity-check that the file
        // we see is the one they generated, without us pretending to
        // know which panel it represents.
        match fs::read(&mipi_path) {
            Ok(data) => {
                let hash = format!("{:x}", Md5::digest(&data));
                (
                    Some(format!("custom ({})", &hash[..8])),
                    Some("Custom overlay".to_string()),
                )
            }
            Err(_) => (None, None),
        }
    } else {
        (None, None)
    };

    OverlayStatus {
        boot_path: boot_path.to_string(),
        has_archr: true,
        current_overlay,
        current_panel_name,
        variant,
    }
}

