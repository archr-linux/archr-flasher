// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) 2026 ArchR
//
// macOS flash path: DiskArbitration-style unmount via diskutil + dd to the
// raw /dev/rdisk, driven under osascript admin privileges. Split out of
// flash.rs so each OS owns its own flash logic.
#![allow(unused_imports)]

use std::io::{BufReader, Read, Write};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use serde::Serialize;
use crate::flash::{emit_progress, validate_device, check_temp_space, poll_flash_progress, decompress_xz, decompress_gz};

// ---------------------------------------------------------------------------
// macOS: privilege escalation via osascript
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
pub fn flash_image_privileged(
    app: &AppHandle,
    image_path: &str,
    device: &str,
    custom_dtbo_path: &str,
    variant: &str,
    verify: bool,
) -> Result<(), String> {
    let _ = verify;  // TODO: thread through into osascript invocation
    use std::process::{Command, Stdio};
    use std::os::unix::fs::PermissionsExt;

    validate_device(device)?;

    let is_xz = image_path.ends_with(".xz");
    let is_gz = image_path.ends_with(".gz") && !is_xz;
    let needs_decompress = is_xz || is_gz;

    // Check temp space before decompression
    if needs_decompress {
        check_temp_space(image_path)?;
    }

    // Step 1: Decompress in user space (with progress)
    let img_to_flash = if needs_decompress {
        emit_progress(app, 0.0, "decompressing");
        let temp_img = std::env::temp_dir().join("archr-flash-temp.img");
        if is_xz {
            decompress_xz(app, image_path, &temp_img)?;
        } else {
            decompress_gz(app, image_path, &temp_img)?;
        }
        temp_img
    } else {
        PathBuf::from(image_path)
    };

    // Get decompressed image size for progress tracking
    let image_size = fs::metadata(&img_to_flash)
        .map(|m| m.len()).unwrap_or(0);

    // Step 2: Unmount disk (macOS auto-mounts SD cards)
    // RPi Imager technique: force unmount (kDADiskUnmountOptionForce equivalent)
    let _ = Command::new("diskutil")
        .args(["unmountDisk", "force", device])
        .status();

    // Step 3: Write helper script
    let script_path = std::env::temp_dir().join("archr-flash.sh");
    fs::write(&script_path, FLASH_SCRIPT_MACOS)
        .map_err(|e| format!("Cannot write helper script: {}", e))?;
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("Cannot set script permissions: {}", e))?;

    // Step 4: Set up progress file
    let progress_file = std::env::temp_dir().join("archr-flash-progress");
    fs::write(&progress_file, "0").ok();

    emit_progress(app, 60.0, "writing");

    let stop = Arc::new(AtomicBool::new(false));
    let poll_handle = poll_flash_progress(
        app.clone(), progress_file.clone(), image_size, stop.clone(),
    );

    // Step 5: Run via osascript with administrator privileges
    // Build shell command with proper escaping for AppleScript context:
    // AppleScript do shell script uses double-quoted strings — escape \ and "
    let shell_esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"").replace('\'', "'\\''");
    let shell_cmd = format!(
        "bash '{}' '{}' '{}' '{}' '{}' '{}'",
        shell_esc(&script_path.display().to_string()),
        shell_esc(&img_to_flash.display().to_string()),
        shell_esc(device), shell_esc(custom_dtbo_path), shell_esc(variant),
        shell_esc(&progress_file.display().to_string())
    );
    let applescript = format!(
        "do shell script \"{}\" with administrator privileges",
        shell_cmd.replace('\\', "\\\\").replace('"', "\\\"")
    );

    let child = Command::new("osascript")
        .args(["-e", &applescript])
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("osascript error: {}", e))?;

    let output = child.wait_with_output()
        .map_err(|e| format!("Failed to wait for osascript: {}", e))?;

    // Stop polling thread
    stop.store(true, Ordering::Relaxed);
    let _ = poll_handle.join();

    // Cleanup
    let _ = fs::remove_file(&script_path);
    let _ = fs::remove_file(&progress_file);
    if is_xz {
        let _ = fs::remove_file(&img_to_flash);
    }

    // Keep a diagnostic log regardless of outcome; the UI shows a short
    // translated message, this file carries the real story for reports.
    let log_path = std::env::temp_dir().join("archr-flasher-macos.log");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let _ = fs::write(&log_path, format!(
        "exit: {:?}\ndevice: {}\nimage: {}\n--- script stderr ---\n{}",
        output.status, device, img_to_flash.display(), stderr
    ));

    if !output.status.success() {
        if stderr.contains("User canceled") || stderr.contains("-128") {
            return Err("cancelled".into());
        }
        // Stable tokens from the helper script win over the generic text.
        for token in ["err:dd_write", "err:mount_boot"] {
            if stderr.contains(token) {
                return Err(format!("{} (log: {})", token, log_path.display()));
            }
        }
        if stderr.to_lowercase().contains("busy") {
            return Err("err:device_busy".into());
        }
        return Err(format!("Flash failed: {} (log: {})", stderr, log_path.display()));
    }

    emit_progress(app, 95.0, "syncing");
    emit_progress(app, 100.0, "done");

    Ok(())
}

#[cfg(target_os = "macos")]
const FLASH_SCRIPT_MACOS: &str = r#"#!/bin/bash
set -e

IMAGE="$1"
DEVICE="$2"
CUSTOM_DTBO="$3"
VARIANT="$4"
PROGRESS_FILE="$5"

# RPi Imager technique: force unmount (kDADiskUnmountOptionForce equivalent)
diskutil unmountDisk force "$DEVICE" 2>/dev/null || true

# Write raw image to device (macOS uses rdisk for raw access = faster)
RDISK=$(echo "$DEVICE" | sed 's|/dev/disk|/dev/rdisk|')

# === ZERO FIRST AND LAST MB BEFORE WRITING ===
# Mirror the rpi-imager approach: destroy any leftover MBR/GPT/FS
# signatures so kernel/diskutil can't fall back to stale partition state
# from a previous flash. Critical for ArchR specifically because
# fs-resize bails on first boot if /storage still has .config from the
# previous SD owner, which kept the same SD bricked across reflashes.
echo "STAGE:wiping_signatures" > "$PROGRESS_FILE"
DEVICE_BYTES=$(diskutil info "$DEVICE" | awk '/Disk Size/ {for(i=1;i<=NF;i++) if($i ~ /^\([0-9]+/) {gsub(/[(),]/, "", $i); print $i; exit}}')
dd if=/dev/zero of="$RDISK" bs=1m count=1 conv=sync 2>/dev/null || true
if [ -n "$DEVICE_BYTES" ] && [ "$DEVICE_BYTES" -gt $((2 * 1024 * 1024)) ] 2>/dev/null; then
    LAST_MB_OFFSET=$((DEVICE_BYTES / (1024 * 1024) - 1))
    dd if=/dev/zero of="$RDISK" bs=1m count=1 oseek="$LAST_MB_OFFSET" conv=sync 2>/dev/null || true
fi
sync

# dd in background, monitor via SIGINFO. One retry after a fresh force
# unmount: macOS occasionally remounts the card between our unmount and
# the raw open, and the first dd then dies with "Resource busy".
run_dd() {
    DD_STDERR=$(mktemp)
    dd if="$IMAGE" of="$RDISK" bs=4m 2>"$DD_STDERR" &
    DD_PID=$!
    while kill -0 $DD_PID 2>/dev/null; do
        sleep 1
        kill -INFO $DD_PID 2>/dev/null || true
        sleep 0.5
        # BSD dd prints "N bytes transferred" to stderr
        BYTES=$(tail -1 "$DD_STDERR" 2>/dev/null | grep -o '^[0-9]*' || true)
        if [ -n "$BYTES" ] && [ "$BYTES" -gt 0 ] 2>/dev/null; then
            echo "$BYTES" > "$PROGRESS_FILE"
        fi
    done
    wait $DD_PID
    DD_RC=$?
    return $DD_RC
}

set +e
run_dd
DD_RC=$?
if [ $DD_RC -ne 0 ] && grep -qi "busy" "$DD_STDERR" 2>/dev/null; then
    echo "dd hit Resource busy, forcing unmount and retrying once" >&2
    diskutil unmountDisk force "$DEVICE" >&2 2>&1 || true
    sleep 2
    run_dd
    DD_RC=$?
fi
if [ $DD_RC -ne 0 ]; then
    echo "err:dd_write rc=$DD_RC" >&2
    tail -3 "$DD_STDERR" >&2 2>/dev/null
    rm -f "$DD_STDERR"
    exit 1
fi
rm -f "$DD_STDERR"
set -e

sync

# Re-mount disk so we can access boot partition (retry up to 3 times)
BOOT_VOL=""
for i in 1 2 3 4 5; do
    sleep 2
    # mountDisk mounts every volume; the explicit s1 mount covers the
    # case where diskutil skips the boot FAT on the first pass.
    diskutil mountDisk "$DEVICE" >&2 2>&1 || true
    diskutil mount "${DEVICE}s1" >&2 2>&1 || true
    sleep 1
    BOOT_VOL=$(diskutil info "${DEVICE}s1" 2>/dev/null | grep "Mount Point:" | awk -F: '{print $2}' | xargs)
    [ -n "$BOOT_VOL" ] && [ -d "$BOOT_VOL" ] && break
    BOOT_VOL=""
done

if [ -z "$BOOT_VOL" ] || [ ! -d "$BOOT_VOL" ]; then
    echo "err:mount_boot could not mount ${DEVICE}s1 after flashing (image was written)" >&2
    diskutil info "${DEVICE}s1" >&2 2>&1 || true
    diskutil eject "$DEVICE" 2>/dev/null || true
    exit 1
fi

# Copy custom DTBO as mipi-panel.dtbo
if [ ! -f "$CUSTOM_DTBO" ]; then
    echo "ERROR: Custom DTBO not found at $CUSTOM_DTBO" >&2
    diskutil eject "$DEVICE" 2>/dev/null || true
    exit 1
fi
mkdir -p "$BOOT_VOL/overlays"
cp "$CUSTOM_DTBO" "$BOOT_VOL/overlays/mipi-panel.dtbo"

# Write variant file
echo -n "$VARIANT" > "$BOOT_VOL/variant"

# Switch extlinux config for soysauce variant (uses explicit FDT),
# matching the Linux flash path. Without it the board keeps the default
# extlinux.conf and the console never boots.
if [ "$VARIANT" = "soysauce" ] && [ -f "$BOOT_VOL/extlinux/extlinux.conf.soysauce" ]; then
    cp "$BOOT_VOL/extlinux/extlinux.conf" "$BOOT_VOL/extlinux/extlinux.conf.bak"
    cp "$BOOT_VOL/extlinux/extlinux.conf.soysauce" "$BOOT_VOL/extlinux/extlinux.conf"
fi

sync

# Eject disk safely
diskutil eject "$DEVICE" 2>/dev/null || true
"#;
