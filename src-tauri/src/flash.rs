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
    panel_dtb: &str,
    panel_id: &str,
    variant: &str,
) -> Result<(), String> {
    use std::process::{Command, Stdio};
    use std::os::unix::fs::PermissionsExt;

    validate_device(device)?;

    let is_xz = image_path.ends_with(".xz");

    // Check temp space before decompression
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
        .arg(panel_dtb)
        .arg(panel_id)
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
        if stderr.contains("dismissed") || stderr.contains("Not authorized") {
            return Err("cancelled".into());
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

#[cfg(target_os = "linux")]
const FLASH_SCRIPT: &str = r#"#!/bin/bash
set -e

IMAGE="$1"
DEVICE="$2"
PANEL_DTB="$3"
PANEL_ID="$4"
VARIANT="$5"
PROGRESS_FILE="$6"

# Write raw image to device with progress tracking
IMAGE_SIZE=$(stat -c%s "$IMAGE")
dd if="$IMAGE" of="$DEVICE" bs=4M conv=fsync status=none &
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
wait $DD_PID

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
    panel_dtb: &str,
    panel_id: &str,
    variant: &str,
) -> Result<(), String> {
    use std::process::{Command, Stdio};
    use std::os::unix::fs::PermissionsExt;

    validate_device(device)?;

    let is_xz = image_path.ends_with(".xz");

    // Check temp space before decompression
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

    // Get decompressed image size for progress tracking
    let image_size = fs::metadata(&img_to_flash)
        .map(|m| m.len()).unwrap_or(0);

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

    // Step 4: Set up progress file
    let progress_file = std::env::temp_dir().join("archr-flash-progress");
    fs::write(&progress_file, "0").ok();

    emit_progress(app, 60.0, "writing");

    let stop = Arc::new(AtomicBool::new(false));
    let poll_handle = poll_flash_progress(
        app.clone(), progress_file.clone(), image_size, stop.clone(),
    );

    // Step 5: Run via osascript with administrator privileges
    // Sanitize all values for AppleScript single-quote context:
    // escape ' → '\'' (end quote, literal quote, reopen quote)
    let esc = |s: &str| s.replace('\'', "'\\''");
    let cmd = format!(
        "do shell script \"bash '{}' '{}' '{}' '{}' '{}' '{}' '{}'\" with administrator privileges",
        esc(&script_path.display().to_string()),
        esc(&img_to_flash.display().to_string()),
        esc(device), esc(panel_dtb), esc(panel_id), esc(variant),
        esc(&progress_file.display().to_string())
    );

    let child = Command::new("osascript")
        .args(["-e", &cmd])
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
PANEL_DTB="$3"
PANEL_ID="$4"
VARIANT="$5"
PROGRESS_FILE="$6"

# Unmount all partitions (in case auto-mounted again)
diskutil unmountDisk "$DEVICE" 2>/dev/null || true

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
    app: &AppHandle,
    image_path: &str,
    device: &str,
    panel_dtb: &str,
    panel_id: &str,
    variant: &str,
) -> Result<(), String> {
    use std::process::{Command, Stdio};

    validate_device(device)?;

    let is_xz = image_path.ends_with(".xz");

    // Check temp space before decompression
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

    // Get decompressed image size for progress tracking
    let image_size = fs::metadata(&img_to_flash)
        .map(|m| m.len()).unwrap_or(0);

    // Step 2: Write helper script and params to temp
    let script_path = std::env::temp_dir().join("archr-flash.ps1");
    fs::write(&script_path, FLASH_SCRIPT_WINDOWS)
        .map_err(|e| format!("Cannot write helper script: {}", e))?;

    let progress_file = std::env::temp_dir().join("archr-flash-progress");
    fs::write(&progress_file, "0").ok();

    let params_path = std::env::temp_dir().join("archr-flash-params.json");
    let params = serde_json::json!({
        "image": img_to_flash.to_string_lossy().to_string(),
        "device": device,
        "panel_dtb": panel_dtb,
        "panel_id": panel_id,
        "variant": variant,
        "progress_file": progress_file.to_string_lossy().to_string(),
    });
    fs::write(&params_path, serde_json::to_string(&params).unwrap())
        .map_err(|e| format!("Cannot write params: {}", e))?;

    // Step 3: Log file for the elevated process (its console is invisible)
    let log_file = std::env::temp_dir().join("archr-flash-log.txt");
    let _ = fs::remove_file(&log_file);

    // Step 4: Run elevated via UAC (Start-Process -Verb RunAs triggers UAC prompt)
    emit_progress(app, 60.0, "writing");

    let stop = Arc::new(AtomicBool::new(false));
    let poll_handle = poll_flash_progress(
        app.clone(), progress_file.clone(), image_size, stop.clone(),
    );

    // Encode script path for PowerShell — use -EncodedCommand to avoid path escaping issues
    let inner_cmd = format!(
        "Set-ExecutionPolicy Bypass -Scope Process -Force; & '{}' *> '{}'",
        script_path.display().to_string().replace('\'', "''"),
        log_file.display().to_string().replace('\'', "''"),
    );
    let encoded = {
        let utf16: Vec<u8> = inner_cmd.encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(&utf16)
    };

    let launcher_cmd = format!(
        "try {{ $p = Start-Process powershell -Verb RunAs -Wait -PassThru -WindowStyle Hidden -ArgumentList @('-NoProfile','-EncodedCommand','{}'); if ($p) {{ exit $p.ExitCode }} else {{ exit 1 }} }} catch {{ exit 1 }}",
        encoded
    );

    let child = Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &launcher_cmd])
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to run PowerShell: {}", e))?;

    let output = child.wait_with_output()
        .map_err(|e| format!("Failed to wait for PowerShell: {}", e))?;

    // Stop polling thread
    stop.store(true, Ordering::Relaxed);
    let _ = poll_handle.join();

    // Read the elevated process log for error details
    let log_content = fs::read_to_string(&log_file).unwrap_or_default();

    // Cleanup temp files
    let _ = fs::remove_file(&script_path);
    let _ = fs::remove_file(&params_path);
    let _ = fs::remove_file(&progress_file);
    let _ = fs::remove_file(&log_file);
    if is_xz {
        let _ = fs::remove_file(&img_to_flash);
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let combined = format!("{}{}{}", stderr, stdout, log_content);
        if combined.contains("canceled") || combined.contains("cancelled")
            || combined.contains("The operation was canceled") {
            return Err("cancelled".into());
        }
        let error_msg = if !log_content.trim().is_empty() {
            log_content.trim().to_string()
        } else {
            format!("{}{}", stderr.trim(), stdout.trim())
        };
        return Err(format!("Flash failed: {}", error_msg));
    }

    emit_progress(app, 95.0, "syncing");
    emit_progress(app, 100.0, "done");

    Ok(())
}

#[cfg(target_os = "windows")]
const FLASH_SCRIPT_WINDOWS: &str = r#"
$ErrorActionPreference = "Stop"
try {
    $paramsFile = Join-Path $env:TEMP "archr-flash-params.json"
    $params = Get-Content $paramsFile -Raw | ConvertFrom-Json
    $ImagePath = $params.image
    $Device = $params.device
    $PanelDTB = $params.panel_dtb
    $PanelID = $params.panel_id
    $Variant = $params.variant
    $ProgressFile = $params.progress_file

    # Extract disk number from \\.\PhysicalDriveN
    $diskNum = [int]([regex]::Match($Device, '\d+$').Value)

    # Step 1: Take disk offline, clean via diskpart script file (pipe is unreliable)
    $dpScript = Join-Path $env:TEMP "archr-diskpart.txt"
    "select disk $diskNum`r`noffline disk`r`nonline disk`r`nclean" | Out-File -Encoding ascii $dpScript
    $dpOut = & diskpart /s $dpScript 2>&1
    Remove-Item $dpScript -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 2

    # Step 2: Write raw image to physical drive via .NET FileStream
    $bufSize = 4 * 1024 * 1024
    $buf = New-Object byte[] $bufSize
    $totalWritten = [long]0
    $lastReport = [System.Diagnostics.Stopwatch]::StartNew()

    $src = [System.IO.FileStream]::new(
        $ImagePath,
        [System.IO.FileMode]::Open,
        [System.IO.FileAccess]::Read,
        [System.IO.FileShare]::Read,
        $bufSize
    )
    # Open physical drive for raw write
    $dst = [System.IO.FileStream]::new(
        $Device,
        [System.IO.FileMode]::Open,
        [System.IO.FileAccess]::Write,
        [System.IO.FileShare]::ReadWrite,
        $bufSize
    )

    try {
        while (($read = $src.Read($buf, 0, $bufSize)) -gt 0) {
            $dst.Write($buf, 0, $read)
            $totalWritten += $read
            if ($lastReport.ElapsedMilliseconds -ge 500) {
                [System.IO.File]::WriteAllText($ProgressFile, $totalWritten.ToString())
                $lastReport.Restart()
            }
        }
        $dst.Flush()
    } finally {
        $src.Dispose()
        $dst.Dispose()
    }

    # Step 3: Rescan disks to detect new partition table
    $dpScript2 = Join-Path $env:TEMP "archr-diskpart2.txt"
    "rescan" | Out-File -Encoding ascii $dpScript2
    & diskpart /s $dpScript2 2>&1 | Out-Null
    Remove-Item $dpScript2 -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 3

    # Step 4: Find boot partition (FAT32, partition 1) and mount it
    $bootPart = Get-Partition -DiskNumber $diskNum -PartitionNumber 1 -ErrorAction SilentlyContinue

    if ($bootPart -and (-not $bootPart.DriveLetter -or $bootPart.DriveLetter -eq [char]0)) {
        $bootPart | Add-PartitionAccessPath -AssignDriveLetter -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 2
        $bootPart = Get-Partition -DiskNumber $diskNum -PartitionNumber 1 -ErrorAction SilentlyContinue
    }

    if ($bootPart -and $bootPart.DriveLetter -and $bootPart.DriveLetter -ne [char]0) {
        $bootDrive = "$($bootPart.DriveLetter):\"

        if (Test-Path $bootDrive) {
            # Copy selected panel DTB as kernel.dtb
            $panelFile = Join-Path $bootDrive $PanelDTB
            if (Test-Path $panelFile) {
                Copy-Item $panelFile (Join-Path $bootDrive "kernel.dtb") -Force
            }

            # Write panel configuration (UTF-8, no BOM)
            $enc = New-Object System.Text.UTF8Encoding($false)
            [System.IO.File]::WriteAllText(
                (Join-Path $bootDrive "panel.txt"),
                "PanelNum=$PanelID`nPanelDTB=$PanelDTB`n",
                $enc
            )
            [System.IO.File]::WriteAllText(
                (Join-Path $bootDrive "panel-confirmed"),
                "confirmed`n",
                $enc
            )
            [System.IO.File]::WriteAllText(
                (Join-Path $bootDrive "variant"),
                "$Variant`n",
                $enc
            )
        }
    }

    exit 0
} catch {
    Write-Error $_.Exception.Message
    exit 1
}
"#;
