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
}

fn emit_progress(app: &AppHandle, percent: f64, stage: &str) {
    let _ = app.emit("flash-progress", FlashProgress {
        percent,
        stage: stage.to_string(),
    });
}

/// Re-validate the device is a valid removable disk before flashing.
/// Prevents writing to a device that was removed or swapped since selection.
fn validate_device(device: &str) -> Result<(), String> {
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
fn check_temp_space(image_path: &str) -> Result<(), String> {
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
/// Maps bytes written (0..image_size) to progress (60%..95%).
fn poll_flash_progress(
    app: AppHandle,
    progress_file: PathBuf,
    image_size: u64,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(500));
            if let Ok(content) = fs::read_to_string(&progress_file) {
                if let Ok(bytes) = content.trim().parse::<u64>() {
                    if image_size > 0 {
                        let pct = 60.0 + (bytes as f64 / image_size as f64 * 35.0).min(35.0);
                        emit_progress(&app, pct, "writing");
                    }
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Linux: pkexec + helper script
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
pub fn flash_image_privileged(
    app: &AppHandle,
    image_path: &str,
    device: &str,
    custom_dtbo_path: &str,
    variant: &str,
) -> Result<(), String> {
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

    // Step 2: Write helper script to temp file
    let script_path = std::env::temp_dir().join("archr-flash.sh");
    fs::write(&script_path, FLASH_SCRIPT)
        .map_err(|e| format!("Cannot write helper script: {}", e))?;
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("Cannot set script permissions: {}", e))?;

    // Step 3: Set up progress file and run via pkexec
    let progress_file = std::env::temp_dir().join("archr-flash-progress");
    fs::write(&progress_file, "0").ok();

    emit_progress(app, 60.0, "writing");

    let stop = Arc::new(AtomicBool::new(false));
    let poll_handle = poll_flash_progress(
        app.clone(), progress_file.clone(), image_size, stop.clone(),
    );

    let child = Command::new("pkexec")
        .arg("bash")
        .arg(&script_path)
        .arg(img_to_flash.to_str().unwrap_or(""))
        .arg(device)
        .arg(custom_dtbo_path)
        .arg(variant)
        .arg(&progress_file)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to run pkexec: {}", e))?;

    let output = child.wait_with_output()
        .map_err(|e| format!("Failed to wait for pkexec: {}", e))?;

    // Stop polling thread
    stop.store(true, Ordering::Relaxed);
    let _ = poll_handle.join();

    // Cleanup temp files
    let _ = fs::remove_file(&script_path);
    let _ = fs::remove_file(&progress_file);
    if is_xz {
        let _ = fs::remove_file(&img_to_flash);
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("[flash] script exit code: {:?}", output.status.code());
        eprintln!("[flash] stderr: {}", stderr);
        if stderr.contains("dismissed") || stderr.contains("Not authorized") {
            return Err("cancelled".into());
        }
        // Pass dd error through directly so frontend shows the real cause
        let stderr = stderr.trim();
        if stderr.starts_with("dd failed:") {
            return Err(format!("dd error: {}", &stderr[10..].trim()));
        }
        return Err(format!("Flash failed: {}", stderr));
    }

    emit_progress(app, 95.0, "syncing");
    emit_progress(app, 100.0, "done");

    Ok(())
}

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

fn decompress_gz(app: &AppHandle, src: &str, dst: &Path) -> Result<(), String> {
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

#[cfg(target_os = "linux")]
const FLASH_SCRIPT: &str = r#"#!/bin/bash
set -e

IMAGE="$1"
DEVICE="$2"
CUSTOM_DTBO="$3"
VARIANT="$4"
PROGRESS_FILE="$5"

# Recreate progress file as root (fs.protected_regular=2 on Ubuntu blocks
# root from writing to user-owned files in /tmp via O_CREAT)
rm -f "$PROGRESS_FILE"
echo "0" > "$PROGRESS_FILE"
chmod 666 "$PROGRESS_FILE"

unmount_all() {
    for part in "${DEVICE}"*; do
        [ -b "$part" ] || continue
        udisksctl unmount -b "$part" --no-user-interaction 2>/dev/null || true
        umount "$part" 2>/dev/null \
            || umount -l "$part" 2>/dev/null \
            || umount -f "$part" 2>/dev/null \
            || true
    done
}

# Aggressive unmount: udisksctl (tells desktop to release), then kernel umount
unmount_all
sleep 1

# Flush kernel buffers and wipe FS signatures (prevents desktop auto-remount)
blockdev --flushbufs "$DEVICE" 2>/dev/null || true
wipefs -a "$DEVICE" 2>/dev/null || true

# Write raw image with retry (SD cards can have transient I/O errors)
DD_OK=0
for attempt in 1 2 3; do
    DD_ERR=$(mktemp)
    dd if="$IMAGE" of="$DEVICE" bs=4M conv=fsync status=none 2>"$DD_ERR" &
    DD_PID=$!

    # Monitor dd progress via /proc/fdinfo
    while kill -0 $DD_PID 2>/dev/null; do
        sleep 1
        if [ -f "/proc/$DD_PID/fdinfo/1" ]; then
            POS=$(grep '^pos:' "/proc/$DD_PID/fdinfo/1" 2>/dev/null | awk '{print $2}')
            if [ -n "$POS" ]; then
                echo "$POS" > "$PROGRESS_FILE"
            fi
        fi
    done

    if wait $DD_PID; then
        DD_OK=1
        rm -f "$DD_ERR"
        break
    fi

    ERR=$(cat "$DD_ERR" 2>/dev/null)
    rm -f "$DD_ERR"
    if [ "$attempt" -lt 3 ]; then
        sleep 2
        # Re-unmount in case desktop auto-mounted during write
        unmount_all
        blockdev --flushbufs "$DEVICE" 2>/dev/null || true
        echo "0" > "$PROGRESS_FILE"
    else
        echo "dd failed: $ERR" >&2
        exit 1
    fi
done

sync

# Re-read partition table with retry (kernel may be slow to update)
for i in 1 2 3; do
    partprobe "$DEVICE" 2>/dev/null || true
    sleep 1
    # Determine boot partition device name
    if [[ "$DEVICE" == *mmcblk* ]] || [[ "$DEVICE" == *nvme* ]]; then
        BOOT_PART="${DEVICE}p1"
    else
        BOOT_PART="${DEVICE}1"
    fi
    [ -b "$BOOT_PART" ] && break
    sleep 2
done

if [ ! -b "$BOOT_PART" ]; then
    echo "Boot partition $BOOT_PART not found after writing" >&2
    exit 1
fi

# Mount boot partition with retry (may need time after partprobe)
MOUNT_DIR=$(mktemp -d)
MOUNTED=0
for i in 1 2 3; do
    if mount "$BOOT_PART" "$MOUNT_DIR" 2>/dev/null; then
        MOUNTED=1
        break
    fi
    sleep 2
    partprobe "$DEVICE" 2>/dev/null || true
done

if [ "$MOUNTED" -ne 1 ]; then
    echo "Failed to mount boot partition $BOOT_PART" >&2
    rmdir "$MOUNT_DIR" 2>/dev/null || true
    exit 1
fi

# Copy custom DTBO as mipi-panel.dtbo
if [ ! -f "$CUSTOM_DTBO" ]; then
    echo "ERROR: Custom DTBO not found at $CUSTOM_DTBO" >&2
    umount "$MOUNT_DIR" 2>/dev/null || true
    rmdir "$MOUNT_DIR" 2>/dev/null || true
    exit 1
fi
mkdir -p "$MOUNT_DIR/overlays"
cp "$CUSTOM_DTBO" "$MOUNT_DIR/overlays/mipi-panel.dtbo"

# Write variant file
echo -n "$VARIANT" > "$MOUNT_DIR/variant"

# Switch extlinux config for soysauce variant (uses explicit FDT)
if [ "$VARIANT" = "soysauce" ] && [ -f "$MOUNT_DIR/extlinux/extlinux.conf.soysauce" ]; then
    cp "$MOUNT_DIR/extlinux/extlinux.conf" "$MOUNT_DIR/extlinux/extlinux.conf.bak"
    cp "$MOUNT_DIR/extlinux/extlinux.conf.soysauce" "$MOUNT_DIR/extlinux/extlinux.conf"
fi

sync
umount "$MOUNT_DIR"
rmdir "$MOUNT_DIR"

# Eject the device
eject "$DEVICE" 2>/dev/null || true
"#;

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
) -> Result<(), String> {
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

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("User canceled") || stderr.contains("-128") {
            return Err("cancelled".into());
        }
        return Err(format!("Flash failed: {}", stderr));
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

# dd in background, monitor via SIGINFO
DD_STDERR=$(mktemp)
dd if="$IMAGE" of="$RDISK" bs=4m 2>"$DD_STDERR" &
DD_PID=$!

# Monitor progress: send SIGINFO periodically, parse stderr for bytes
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
rm -f "$DD_STDERR"

sync

# Re-mount disk so we can access boot partition (retry up to 3 times)
BOOT_VOL=""
for i in 1 2 3; do
    sleep 2
    diskutil mountDisk "$DEVICE" 2>/dev/null || true
    sleep 1
    BOOT_VOL=$(diskutil info "${DEVICE}s1" 2>/dev/null | grep "Mount Point:" | awk -F: '{print $2}' | xargs)
    [ -n "$BOOT_VOL" ] && [ -d "$BOOT_VOL" ] && break
    BOOT_VOL=""
done

if [ -z "$BOOT_VOL" ] || [ ! -d "$BOOT_VOL" ]; then
    echo "ERROR: Could not mount boot partition to configure panel" >&2
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

sync

# Eject disk safely
diskutil eject "$DEVICE" 2>/dev/null || true
"#;

// ---------------------------------------------------------------------------
// Windows: admin manifest provides elevation at startup (like Rufus).
// No runtime UAC, no PowerShell elevation layers, no visible windows.
// ---------------------------------------------------------------------------
#[cfg(target_os = "windows")]
pub fn flash_image_privileged(
    app: &AppHandle,
    image_path: &str,
    device: &str,
    custom_dtbo_path: &str,
    variant: &str,
) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    const CREATE_NO_WINDOW: u32 = 0x08000000;

    validate_device(device)?;

    let is_xz = image_path.ends_with(".xz");

    if is_xz {
        check_temp_space(image_path)?;
    }

    // Step 1: Decompress .xz in user space (with progress)
    let img_to_flash = if is_xz {
        emit_progress(app, 0.0, "decompressing");
        let temp_img = std::env::temp_dir().join("archr-flash-temp.img");
        decompress_xz(app, image_path, &temp_img)?;
        temp_img
    } else {
        PathBuf::from(image_path)
    };

    let image_size = fs::metadata(&img_to_flash)
        .map(|m| m.len()).unwrap_or(0);

    // Step 2: Generate PS1 script with embedded absolute paths
    let temp = std::env::temp_dir();
    let script_path = temp.join("archr-flash.ps1");
    let progress_file = temp.join("archr-flash-progress");

    let _ = fs::remove_file(&progress_file);
    fs::write(&progress_file, "0").ok();

    let script_content = generate_windows_script(
        &img_to_flash.to_string_lossy(),
        device,
        custom_dtbo_path,
        variant,
        &progress_file.to_string_lossy(),
    );
    fs::write(&script_path, &script_content)
        .map_err(|e| format!("Cannot write flash script: {}", e))?;

    // Step 3: Run PowerShell directly — app is already elevated via manifest,
    // no Start-Process -Verb RunAs needed! CREATE_NO_WINDOW = no visible console.
    emit_progress(app, 60.0, "writing");

    let stop = Arc::new(AtomicBool::new(false));
    let poll_handle = poll_flash_progress(
        app.clone(), progress_file.clone(), image_size, stop.clone(),
    );

    let output = Command::new("powershell")
        .args([
            "-NoProfile", "-ExecutionPolicy", "Bypass",
            "-File", script_path.to_str().unwrap_or(""),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to run PowerShell: {}", e))?
        .wait_with_output()
        .map_err(|e| format!("Failed to wait for PowerShell: {}", e))?;

    stop.store(true, Ordering::Relaxed);
    let _ = poll_handle.join();

    // Cleanup temp files
    let _ = fs::remove_file(&script_path);
    let _ = fs::remove_file(&progress_file);
    if is_xz {
        let _ = fs::remove_file(&img_to_flash);
    }

    // Direct exit code check — reliable since there's no UAC layer in between
    if output.status.success() {
        emit_progress(app, 95.0, "syncing");
        emit_progress(app, 100.0, "done");
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!("Flash failed: {}", stderr.trim()))
}

/// Generate a PowerShell flash script using Rufus-inspired techniques:
/// - Clear-Disk for proper volume lock + dismount + MBR/GPT clearing
/// - .NET FileStream write with retry loop (4 attempts, 5s delay)
/// - Write-retry with file pointer reposition on failure
/// - Update-Disk for proper partition table refresh
#[cfg(target_os = "windows")]
fn generate_windows_script(
    image_path: &str,
    device: &str,
    custom_dtbo_path: &str,
    variant: &str,
    progress_file: &str,
) -> String {
    let esc = |s: &str| s.replace('\'', "''");
    format!(
        r#"$ErrorActionPreference = "Stop"
try {{
    $ImagePath = '{image}'
    $Device = '{device}'
    $CustomDTBO = '{custom_dtbo}'
    $Variant = '{variant}'
    $ProgressFile = '{progress}'

    $diskNum = [int]([regex]::Match($Device, '\d+$').Value)

    # Rufus technique: Clear-Disk handles volume lock, dismount, and MBR/GPT
    # clearing in one call (equivalent to FSCTL_LOCK_VOLUME + FSCTL_DISMOUNT_VOLUME
    # + zeroing MBR/GPT sectors that Rufus does manually).
    Clear-Disk -Number $diskNum -RemoveData -RemoveOEM -Confirm:$false -ErrorAction Stop
    Start-Sleep -Seconds 2

    # Write raw image via .NET FileStream
    # Rufus technique: retry loop (4 attempts, 5s delay) with file pointer reposition
    $bufSize = 4 * 1024 * 1024
    $buf = New-Object byte[] $bufSize
    $totalWritten = [long]0
    $lastReport = [System.Diagnostics.Stopwatch]::StartNew()

    $src = $null
    $dst = $null
    for ($retry = 0; $retry -lt 4; $retry++) {{
        try {{
            $src = [System.IO.FileStream]::new(
                $ImagePath,
                [System.IO.FileMode]::Open,
                [System.IO.FileAccess]::Read,
                [System.IO.FileShare]::Read,
                $bufSize
            )
            $dst = [System.IO.FileStream]::new(
                $Device,
                [System.IO.FileMode]::Open,
                [System.IO.FileAccess]::Write,
                [System.IO.FileShare]::ReadWrite,
                $bufSize
            )
            break
        }} catch {{
            if ($src) {{ $src.Dispose(); $src = $null }}
            if ($retry -eq 3) {{ throw }}
            Start-Sleep -Seconds 5
        }}
    }}

    try {{
        while (($read = $src.Read($buf, 0, $bufSize)) -gt 0) {{
            # Rufus technique: write with retry + reposition on failure
            $written = $false
            for ($wr = 0; $wr -lt 4; $wr++) {{
                try {{
                    $dst.Write($buf, 0, $read)
                    $written = $true
                    break
                }} catch {{
                    if ($wr -eq 3) {{ throw }}
                    Start-Sleep -Seconds 5
                    $dst.Position = $totalWritten
                }}
            }}
            $totalWritten += $read
            if ($lastReport.ElapsedMilliseconds -ge 500) {{
                [System.IO.File]::WriteAllText($ProgressFile, $totalWritten.ToString())
                $lastReport.Restart()
            }}
        }}
        $dst.Flush()
    }} finally {{
        if ($src) {{ $src.Dispose() }}
        if ($dst) {{ $dst.Dispose() }}
    }}

    # Rufus technique: IOCTL_DISK_UPDATE_PROPERTIES equivalent
    Update-Disk -Number $diskNum -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 3

    # Find and mount boot partition
    $bootPart = Get-Partition -DiskNumber $diskNum -PartitionNumber 1 -ErrorAction SilentlyContinue
    if ($bootPart -and (-not $bootPart.DriveLetter -or $bootPart.DriveLetter -eq [char]0)) {{
        $bootPart | Add-PartitionAccessPath -AssignDriveLetter -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 2
        $bootPart = Get-Partition -DiskNumber $diskNum -PartitionNumber 1 -ErrorAction SilentlyContinue
    }}

    if (-not ($bootPart -and $bootPart.DriveLetter -and $bootPart.DriveLetter -ne [char]0)) {{
        throw "Could not mount boot partition to configure panel"
    }}

    $bootDrive = "$($bootPart.DriveLetter):\"
    if (-not (Test-Path $bootDrive)) {{
        throw "Boot partition drive $bootDrive not accessible"
    }}

    # Copy custom DTBO as mipi-panel.dtbo
    $overlayDir = Join-Path $bootDrive "overlays"
    if (-not (Test-Path $overlayDir)) {{
        New-Item -ItemType Directory -Path $overlayDir -Force | Out-Null
    }}
    if (-not (Test-Path $CustomDTBO)) {{
        throw "Custom DTBO not found at $CustomDTBO"
    }}
    Copy-Item $CustomDTBO (Join-Path $overlayDir "mipi-panel.dtbo") -Force

    # Write variant file
    Set-Content -Path (Join-Path $bootDrive "variant") -Value $Variant -NoNewline

    exit 0
}} catch {{
    Write-Error $_.Exception.Message
    exit 1
}}
"#,
        image = esc(image_path),
        device = esc(device),
        custom_dtbo = esc(custom_dtbo_path),
        variant = esc(variant),
        progress = esc(progress_file),
    )
}
