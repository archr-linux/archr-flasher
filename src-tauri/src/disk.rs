use serde::Serialize;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct DiskInfo {
    pub device: String,
    pub name: String,
    pub size_bytes: u64,
    pub size_human: String,
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.0} MB", bytes as f64 / 1_000_000.0)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(target_os = "linux")]
pub fn list_removable_disks() -> Vec<DiskInfo> {
    let mut disks = Vec::new();
    let system_disks = get_system_disk_names();

    let block_dir = Path::new("/sys/block");
    let entries = match fs::read_dir(block_dir) {
        Ok(e) => e,
        Err(_) => return disks,
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();

        // Only sd* and mmcblk* devices
        if !name.starts_with("sd") && !name.starts_with("mmcblk") {
            continue;
        }

        // Never list system disk (where / is mounted)
        if system_disks.iter().any(|sd| name == *sd) {
            continue;
        }

        let sys_path = entry.path();

        // Must be removable
        let removable = fs::read_to_string(sys_path.join("removable"))
            .unwrap_or_default()
            .trim()
            .to_string();
        if removable != "1" {
            continue;
        }

        // Read size (in 512-byte sectors)
        let size_sectors: u64 = fs::read_to_string(sys_path.join("size"))
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0);
        let size_bytes = size_sectors * 512;

        // Skip empty or too large (>128GB, probably not an R36S SD card)
        if size_bytes == 0 || size_bytes > 128 * 1_000_000_000 {
            continue;
        }

        // Read model name
        let model = fs::read_to_string(sys_path.join("device/model"))
            .unwrap_or_else(|_| "SD Card".into())
            .trim()
            .to_string();

        let device = format!("/dev/{}", name);
        let size_human = format_size(size_bytes);
        let display_name = format!("{} ({})", model, size_human);

        disks.push(DiskInfo {
            device,
            name: display_name,
            size_bytes,
            size_human,
        });
    }

    disks
}

/// Read /proc/mounts to find which block devices back /, /home, /boot.
/// Returns the parent disk names (e.g. "sda" from "/dev/sda1", "nvme0n1" from "/dev/nvme0n1p2").
#[cfg(target_os = "linux")]
fn get_system_disk_names() -> Vec<String> {
    let mut names = Vec::new();
    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();

    for line in mounts.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 { continue; }

        let mount_point = parts[1];
        // Only protect critical mount points
        if mount_point != "/" && mount_point != "/home" && mount_point != "/boot" {
            continue;
        }

        let dev = parts[0];
        if !dev.starts_with("/dev/") { continue; }
        let dev_name = &dev[5..]; // strip "/dev/"

        // Strip partition number to get parent disk name
        // "sda1" → "sda", "nvme0n1p2" → "nvme0n1", "mmcblk0p1" → "mmcblk0"
        let parent = strip_partition_suffix(dev_name);
        if !parent.is_empty() && !names.contains(&parent) {
            names.push(parent);
        }
    }

    names
}

/// Strip partition suffix: "sda1" → "sda", "nvme0n1p2" → "nvme0n1", "mmcblk0p1" → "mmcblk0"
#[cfg(target_os = "linux")]
fn strip_partition_suffix(dev: &str) -> String {
    if dev.contains("nvme") || dev.contains("mmcblk") {
        // These use "p" + number suffix: nvme0n1p2 → nvme0n1, mmcblk0p1 → mmcblk0
        if let Some(idx) = dev.rfind('p') {
            if dev[idx + 1..].chars().all(|c| c.is_ascii_digit()) && !dev[idx + 1..].is_empty() {
                return dev[..idx].to_string();
            }
        }
        dev.to_string()
    } else {
        // sd* devices: sda1 → sda (strip trailing digits)
        dev.trim_end_matches(|c: char| c.is_ascii_digit()).to_string()
    }
}

#[cfg(target_os = "macos")]
pub fn list_removable_disks() -> Vec<DiskInfo> {
    use std::process::Command;

    let mut disks = Vec::new();

    // Use plain text output (not -plist XML) to list external disks.
    // Output format: "/dev/disk2 (external, physical):" as header lines.
    let output = Command::new("diskutil")
        .args(["list", "external"])
        .output();

    let output = match output {
        Ok(o) => o,
        Err(_) => return disks,
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut device_names: Vec<String> = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        // Match lines like: "/dev/disk2 (external, physical):"
        if line.starts_with("/dev/disk") && line.contains("external") {
            if let Some(dev) = line.split_whitespace().next() {
                device_names.push(dev.to_string());
            }
        }
    }

    // Exclude boot disk (safety: never flash the system disk)
    if let Ok(boot_info) = Command::new("diskutil").args(["info", "/"]).output() {
        let info_str = String::from_utf8_lossy(&boot_info.stdout);
        for line in info_str.lines() {
            if line.contains("Part of Whole:") {
                if let Some(disk_name) = line.split(':').nth(1) {
                    let boot_disk = format!("/dev/{}", disk_name.trim());
                    device_names.retain(|d| d != &boot_disk);
                }
                break;
            }
        }
    }

    for device in device_names {
        // Get detailed info for each disk
        let info_output = Command::new("diskutil")
            .args(["info", &device])
            .output();

        if let Ok(info) = info_output {
            let info_str = String::from_utf8_lossy(&info.stdout);
            let mut size_bytes: u64 = 0;
            let mut name = "SD Card".to_string();

            for info_line in info_str.lines() {
                if info_line.contains("Disk Size:") {
                    // Extract byte count: "Disk Size:   31.9 GB (31914983424 Bytes)"
                    if let Some(start) = info_line.find('(') {
                        if let Some(end) = info_line.find(" Bytes") {
                            let num_str = &info_line[start + 1..end];
                            let num_str = num_str.replace(',', "").replace(' ', "");
                            size_bytes = num_str.parse().unwrap_or(0);
                        }
                    }
                }
                if info_line.contains("Device / Media Name:") {
                    name = info_line.split(':').nth(1).unwrap_or("SD Card").trim().to_string();
                }
            }

            if size_bytes > 0 && size_bytes <= 128 * 1_000_000_000 {
                disks.push(DiskInfo {
                    device,
                    name: format!("{} ({})", name, format_size(size_bytes)),
                    size_bytes,
                    size_human: format_size(size_bytes),
                });
            }
        }
    }

    disks
}

#[cfg(target_os = "windows")]
pub fn list_removable_disks() -> Vec<DiskInfo> {
    use std::process::Command;

    let mut disks = Vec::new();

    // Use PowerShell to list removable disks
    let output = Command::new("powershell")
        .args([
            "-NoProfile", "-Command",
            "Get-Disk | Where-Object { ($_.BusType -eq 'USB' -or $_.BusType -eq 'SD') -and $_.IsSystem -eq $false -and $_.IsBoot -eq $false } | Select-Object Number, FriendlyName, Size | ConvertTo-Json"
        ])
        .output();

    let output = match output {
        Ok(o) => o,
        Err(_) => return disks,
    };

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse JSON output
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
        let items = match &json {
            serde_json::Value::Array(arr) => arr.clone(),
            obj @ serde_json::Value::Object(_) => vec![obj.clone()],
            _ => vec![],
        };

        for item in items {
            let number = item.get("Number").and_then(|v| v.as_u64()).unwrap_or(0);
            let name = item.get("FriendlyName").and_then(|v| v.as_str()).unwrap_or("SD Card");
            let size_bytes = item.get("Size").and_then(|v| v.as_u64()).unwrap_or(0);

            if size_bytes > 0 && size_bytes <= 128 * 1_000_000_000 {
                disks.push(DiskInfo {
                    device: format!("\\\\.\\PhysicalDrive{}", number),
                    name: format!("{} ({})", name, format_size(size_bytes)),
                    size_bytes,
                    size_human: format_size(size_bytes),
                });
            }
        }
    }

    disks
}
