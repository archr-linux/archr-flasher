use std::io::{BufReader, Read, Write};
use std::fs::{self, File};
use std::path::Path;
use tauri::{AppHandle, Emitter};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
struct FlashProgress {
    percent: f64,
    stage: String,
}

fn emit_progress(app: &AppHandle, percent: f64, stage: &str) {
    let _ = app.emit("flash-progress", FlashProgress {
        percent,
        stage: stage.to_string(),
    });
}

// ---------------------------------------------------------------------------
// Linux: pkexec + helper script
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
pub fn flash_image_privileged(
    app: &AppHandle,
    image_path: &str,
    device: &str,
    panel_dtb: &str,
    panel_id: &str,
    variant: &str,
) -> Result<(), String> {
    use std::process::Command;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    let is_xz = image_path.ends_with(".xz");

    // Step 1: Decompress .xz in user space (with progress)
    let img_to_flash = if is_xz {
        emit_progress(app, 0.0, "decompressing");
        let temp_img = std::env::temp_dir().join("archr-flash-temp.img");
        decompress_xz(app, image_path, &temp_img)?;
        temp_img
    } else {
        PathBuf::from(image_path)
    };

    // Step 2: Write helper script to temp file
    let script_path = std::env::temp_dir().join("archr-flash.sh");
    fs::write(&script_path, FLASH_SCRIPT)
        .map_err(|e| format!("Cannot write helper script: {}", e))?;
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("Cannot set script permissions: {}", e))?;

    // Step 3: Run via pkexec (writes image + configures panel)
    emit_progress(app, 60.0, "writing");

    let output = Command::new("pkexec")
        .arg("bash")
        .arg(&script_path)
        .arg(img_to_flash.to_str().unwrap_or(""))
        .arg(device)
        .arg(panel_dtb)
        .arg(panel_id)
        .arg(variant)
        .output()
        .map_err(|e| format!("Failed to run pkexec: {}", e))?;

    // Cleanup temp files
    let _ = fs::remove_file(&script_path);
    if is_xz {
        let _ = fs::remove_file(&img_to_flash);
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("dismissed") || stderr.contains("Not authorized") {
            return Err("cancelled".into());
        }
        return Err(format!("Flash failed: {}", stderr));
    }

    emit_progress(app, 95.0, "configuring");
    emit_progress(app, 100.0, "done");

    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn decompress_xz(app: &AppHandle, src: &str, dst: &Path) -> Result<(), String> {
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

#[cfg(target_os = "linux")]
const FLASH_SCRIPT: &str = r#"#!/bin/bash
set -e

IMAGE="$1"
DEVICE="$2"
PANEL_DTB="$3"
PANEL_ID="$4"
VARIANT="$5"

# Write raw image to device
dd if="$IMAGE" of="$DEVICE" bs=4M conv=fsync status=none
sync

# Re-read partition table
partprobe "$DEVICE" 2>/dev/null || true
sleep 1

# Determine boot partition device name
if [[ "$DEVICE" == *mmcblk* ]] || [[ "$DEVICE" == *nvme* ]]; then
    BOOT_PART="${DEVICE}p1"
else
    BOOT_PART="${DEVICE}1"
fi

# Mount boot partition
MOUNT_DIR=$(mktemp -d)
mount "$BOOT_PART" "$MOUNT_DIR"

# Copy selected panel DTB as kernel.dtb
if [ -f "$MOUNT_DIR/$PANEL_DTB" ]; then
    cp "$MOUNT_DIR/$PANEL_DTB" "$MOUNT_DIR/kernel.dtb"
fi

# Write panel configuration
printf 'PanelNum=%s\nPanelDTB=%s\n' "$PANEL_ID" "$PANEL_DTB" > "$MOUNT_DIR/panel.txt"
echo "confirmed" > "$MOUNT_DIR/panel-confirmed"
echo "$VARIANT" > "$MOUNT_DIR/variant"

sync
umount "$MOUNT_DIR"
rmdir "$MOUNT_DIR"
"#;

// ---------------------------------------------------------------------------
// macOS: privilege escalation via osascript
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
pub fn flash_image_privileged(
    app: &AppHandle,
    image_path: &str,
    device: &str,
    panel_dtb: &str,
    panel_id: &str,
    variant: &str,
) -> Result<(), String> {
    use std::process::Command;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    let is_xz = image_path.ends_with(".xz");

    // Step 1: Decompress .xz in user space (with progress)
    let img_to_flash = if is_xz {
        emit_progress(app, 0.0, "decompressing");
        let temp_img = std::env::temp_dir().join("archr-flash-temp.img");
        decompress_xz(app, image_path, &temp_img)?;
        temp_img
    } else {
        PathBuf::from(image_path)
    };

    // Step 2: Unmount disk (macOS auto-mounts SD cards)
    let _ = Command::new("diskutil")
        .args(["unmountDisk", device])
        .status();

    // Step 3: Write helper script
    let script_path = std::env::temp_dir().join("archr-flash.sh");
    fs::write(&script_path, FLASH_SCRIPT_MACOS)
        .map_err(|e| format!("Cannot write helper script: {}", e))?;
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("Cannot set script permissions: {}", e))?;

    // Step 4: Run via osascript with administrator privileges
    emit_progress(app, 60.0, "writing");

    let cmd = format!(
        "do shell script \"bash '{}' '{}' '{}' '{}' '{}' '{}'\" with administrator privileges",
        script_path.display(),
        img_to_flash.display(), device, panel_dtb, panel_id, variant
    );

    let output = Command::new("osascript")
        .args(["-e", &cmd])
        .output()
        .map_err(|e| format!("osascript error: {}", e))?;

    // Cleanup
    let _ = fs::remove_file(&script_path);
    if is_xz {
        let _ = fs::remove_file(&img_to_flash);
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("User canceled") || stderr.contains("-128") {
            return Err("cancelled".into());
        }
        return Err(format!("Flash failed: {}", stderr));
    }

    emit_progress(app, 95.0, "configuring");
    emit_progress(app, 100.0, "done");

    Ok(())
}

#[cfg(target_os = "macos")]
const FLASH_SCRIPT_MACOS: &str = r#"#!/bin/bash
set -e

IMAGE="$1"
DEVICE="$2"
PANEL_DTB="$3"
PANEL_ID="$4"
VARIANT="$5"

# Unmount all partitions (in case auto-mounted again)
diskutil unmountDisk "$DEVICE" 2>/dev/null || true

# Write raw image to device (macOS uses rdisk for raw access = faster)
RDISK=$(echo "$DEVICE" | sed 's|/dev/disk|/dev/rdisk|')
dd if="$IMAGE" of="$RDISK" bs=4m
sync

# Re-mount disk so we can access boot partition
sleep 2
diskutil mountDisk "$DEVICE" 2>/dev/null || true
sleep 1

# Find the mounted boot partition (FAT32, typically partition 1)
BOOT_VOL=$(diskutil info "${DEVICE}s1" 2>/dev/null | grep "Mount Point:" | awk -F: '{print $2}' | xargs)

if [ -n "$BOOT_VOL" ] && [ -d "$BOOT_VOL" ]; then
    # Copy selected panel DTB as kernel.dtb
    if [ -f "$BOOT_VOL/$PANEL_DTB" ]; then
        cp "$BOOT_VOL/$PANEL_DTB" "$BOOT_VOL/kernel.dtb"
    fi

    # Write panel configuration
    printf 'PanelNum=%s\nPanelDTB=%s\n' "$PANEL_ID" "$PANEL_DTB" > "$BOOT_VOL/panel.txt"
    echo "confirmed" > "$BOOT_VOL/panel-confirmed"
    echo "$VARIANT" > "$BOOT_VOL/variant"

    sync
fi

# Eject disk safely
diskutil eject "$DEVICE" 2>/dev/null || true
"#;

// ---------------------------------------------------------------------------
// Windows: privilege escalation via UAC
// ---------------------------------------------------------------------------
#[cfg(target_os = "windows")]
pub fn flash_image_privileged(
    _app: &AppHandle,
    _image_path: &str,
    _device: &str,
    _panel_dtb: &str,
    _panel_id: &str,
    _variant: &str,
) -> Result<(), String> {
    Err("Windows flash not yet implemented".into())
}
