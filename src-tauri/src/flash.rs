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
    /// Bytes already written/processed in the current stage. Zero when
    /// the byte counter is not available (e.g. while decompressing).
    bytes_done: u64,
    /// Total bytes for the current stage (image size during writing
    /// and verifying). Zero when unknown.
    bytes_total: u64,
}

fn emit_progress(app: &AppHandle, percent: f64, stage: &str) {
    emit_progress_bytes(app, percent, stage, 0, 0);
}

fn emit_progress_bytes(
    app: &AppHandle,
    percent: f64,
    stage: &str,
    bytes_done: u64,
    bytes_total: u64,
) {
    let _ = app.emit("flash-progress", FlashProgress {
        percent,
        stage: stage.to_string(),
        bytes_done,
        bytes_total,
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
/// Supports two formats:
///   - Raw number: bytes written → maps to writing progress (55%..90%)
///   - "STAGE:verifying": switches to verification stage (90%..98%)
///   - "STAGE:done": emits 100%
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
                let content = content.trim();
                if content.starts_with("STAGE:verifying:") {
                    let pct_str = content.trim_start_matches("STAGE:verifying:");
                    let verify_pct = pct_str.parse::<f64>().unwrap_or(0.0);
                    let pct = 90.0 + (verify_pct / 100.0 * 8.0); // 90% to 98%
                    emit_progress(&app, pct, "verifying");
                } else if content.starts_with("STAGE:testing") {
                    emit_progress(&app, 55.0, "testing_sd");
                } else if content.starts_with("STAGE:speed_slow:") {
                    let speed = content.trim_start_matches("STAGE:speed_slow:");
                    let _ = app.emit("sd-speed-result", serde_json::json!({
                        "quality": "slow",
                        "speed_mbs": speed.parse::<u32>().unwrap_or(0)
                    }));
                    emit_progress(&app, 56.0, "writing_safe");
                } else if content.starts_with("STAGE:speed_medium:") {
                    let speed = content.trim_start_matches("STAGE:speed_medium:");
                    let _ = app.emit("sd-speed-result", serde_json::json!({
                        "quality": "medium",
                        "speed_mbs": speed.parse::<u32>().unwrap_or(0)
                    }));
                    emit_progress(&app, 56.0, "writing");
                } else if content.starts_with("STAGE:speed_ok:") {
                    let speed = content.trim_start_matches("STAGE:speed_ok:");
                    let _ = app.emit("sd-speed-result", serde_json::json!({
                        "quality": "fast",
                        "speed_mbs": speed.parse::<u32>().unwrap_or(0)
                    }));
                    emit_progress(&app, 56.0, "writing");
                } else if content.starts_with("STAGE:done") {
                    emit_progress(&app, 98.0, "finalizing");
                } else if content.starts_with("VERIFY_FAILED") {
                    // Surface as a dedicated event so the UI shows a
                    // clear "verification failed" panel instead of the
                    // generic "Flash failed" path. The bash script
                    // also exits 1 right after writing this marker, so
                    // the Rust side will pick up the failure from the
                    // child process exit code too.
                    let detail = content.trim_start_matches("VERIFY_FAILED:");
                    let _ = app.emit("flash-verify-failed", serde_json::json!({
                        "detail": detail
                    }));
                    emit_progress(&app, 0.0, "verify_failed");
                } else if let Ok(bytes) = content.parse::<u64>() {
                    if image_size > 0 {
                        let pct = 55.0 + (bytes as f64 / image_size as f64 * 35.0).min(35.0);
                        emit_progress_bytes(&app, pct, "writing", bytes, image_size);
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
    verify: bool,
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

    // Wrap pkexec inside systemd-inhibit so the screen/lid/idle locks
    // are paused for the whole flash window. Sleeping in the middle of
    // a write can corrupt the SD (kernel writes-in-flight get lost
    // when the controller resumes). If systemd-inhibit isn't available
    // (very old systemd / non-systemd init), we fall back to plain
    // pkexec; suspend is then user-responsibility.
    let has_inhibit = Command::new("which")
        .arg("systemd-inhibit").output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    let mut cmd = if has_inhibit {
        let mut c = Command::new("systemd-inhibit");
        c.arg("--what=idle:sleep:shutdown")
         .arg("--who=ArchR Flasher")
         .arg("--why=Writing SD card")
         .arg("--mode=block")
         .arg("pkexec");
        c
    } else {
        Command::new("pkexec")
    };

    // Locate the native write helper. Tauri installs both binaries
    // side-by-side; the debug/release build dirs (target/debug,
    // target/release) also have them as siblings.
    let helper_path = std::env::current_exe()
        .map_err(|e| format!("cannot resolve exe path: {}", e))?
        .parent()
        .ok_or_else(|| "no parent dir on exe path".to_string())?
        .join("archr-flash-write");
    if !helper_path.is_file() {
        return Err(format!(
            "Native write helper not found at {}",
            helper_path.display()
        ));
    }

    let child = cmd
        .arg("bash")
        .arg(&script_path)
        .arg(img_to_flash.to_str().unwrap_or(""))
        .arg(device)
        .arg(custom_dtbo_path)
        .arg(variant)
        .arg(&progress_file)
        .arg(helper_path.to_str().unwrap_or(""))
        .arg(if verify { "1" } else { "0" })
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to run pkexec: {}", e))?;

    let output = child.wait_with_output()
        .map_err(|e| format!("Failed to wait for pkexec: {}", e))?;

    // Stop polling thread
    stop.store(true, Ordering::Relaxed);
    let _ = poll_handle.join();

    // Cleanup temp files. The decompressed .img is removed regardless
    // of the source format. Previously only is_xz triggered the cleanup
    // and gz-derived .img files stayed at /tmp/archr-flash-temp.img
    // (5 GB) until reboot. needs_decompress covers both branches.
    let _ = fs::remove_file(&script_path);
    let _ = fs::remove_file(&progress_file);
    if needs_decompress {
        let _ = fs::remove_file(&img_to_flash);
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("[flash] script exit code: {:?}", output.status.code());
        eprintln!("[flash] stderr: {}", stderr);
        if stderr.contains("dismissed") || stderr.contains("Not authorized") {
            return Err("cancelled".into());
        }
        // Pass errors through so frontend shows the real cause.
        // The bash preamble dumps diagnostic lines before any failure
        // marker, so we search line-by-line rather than expecting
        // a clean prefix on the whole stderr buffer.
        let stderr = stderr.trim();
        for line in stderr.lines() {
            let l = line.trim();
            if let Some(rest) = l.strip_prefix("dd failed:") {
                return Err(format!("dd error: {}", rest.trim()));
            }
            if let Some(rest) = l.strip_prefix("Script error at line") {
                return Err(format!("dd error: script aborted at line{}", rest));
            }
        }
        if stderr.contains("Verification failed") {
            // Extract expected vs got hashes if the script printed them
            // so the UI can show a precise message rather than the
            // generic "flash failed" label.
            let mut detail = String::new();
            for line in stderr.lines() {
                if line.contains("expected") || line.contains("got") {
                    if !detail.is_empty() { detail.push('\n'); }
                    detail.push_str(line.trim());
                }
            }
            if detail.is_empty() {
                return Err("verify_failed".into());
            }
            return Err(format!("verify_failed: {}", detail));
        }
        // Last-resort: include the last 4 stderr lines in the message
        // so the UI shows something actionable instead of an opaque
        // "flash failed" path. The catch-all translation in main.js
        // maps "flash failed" to the generic "write-protected?" copy,
        // which is misleading when the real cause is e.g. "no space".
        let tail: Vec<&str> = stderr.lines().rev().take(4).collect();
        let tail_str = tail.into_iter().rev().collect::<Vec<_>>().join(" | ");
        return Err(format!("Flash failed: {}", tail_str));
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
set -eE
trap 'echo "Script error at line $LINENO: $BASH_COMMAND" >&2' ERR

IMAGE="$1"
DEVICE="$2"
CUSTOM_DTBO="$3"
VARIANT="$4"
PROGRESS_FILE="$5"
HELPER="$6"
VERIFY="$7"   # "1" = run the SHA-256 verify pass, "0" = skip it

# Up-front diagnostics. When the script aborts later for any reason,
# this preamble lets the maintainer correlate "what was the input?"
# without depending on the UI capturing the full stderr stream.
echo "=== flash.sh start $(date -u +%FT%TZ) ===" >&2
echo "  IMAGE   = $IMAGE" >&2
echo "  DEVICE  = $DEVICE" >&2
echo "  VARIANT = $VARIANT" >&2
echo "  CUSTOM_DTBO = $CUSTOM_DTBO" >&2
echo "  HELPER  = $HELPER" >&2

if [ ! -f "$IMAGE" ]; then
    echo "dd failed: source image missing at $IMAGE" >&2
    exit 1
fi
if [ ! -b "$DEVICE" ]; then
    echo "dd failed: target $DEVICE is not a block device" >&2
    exit 1
fi
if [ ! -x "$HELPER" ]; then
    echo "dd failed: native write helper missing or not executable at $HELPER" >&2
    exit 1
fi
echo "  IMAGE size = $(stat -c%s "$IMAGE") bytes" >&2
echo "  DEVICE size = $(blockdev --getsize64 "$DEVICE" 2>/dev/null || echo unknown) bytes" >&2

# Recreate progress file as root (fs.protected_regular=2 on Ubuntu blocks
# root from writing to user-owned files in /tmp via O_CREAT)
rm -f "$PROGRESS_FILE"
echo "0" > "$PROGRESS_FILE"
chmod 666 "$PROGRESS_FILE"

# Suspend udisks2 for the entire flash. Without this, the desktop
# automount kicks in after `dd` finishes (or after `partprobe` later),
# mounts the freshly-written ext4 partitions, and the kernel updates
# inode mtime/atime + replays the journal, which invalidates every
# metadata_csum baked into the image. The R36S initramfs e2fsck then
# rejects the rootfs as "Filesystem corruption has been detected!"
# even though the dd write was byte-perfect. We verified this in
# 20260514: same image, with udisks2 stopped the SD's partition MD5s
# match the .img bit-for-bit; with udisks2 running they diverge in a
# 256-byte pattern that exactly tracks ext4 inode boundaries.
#
# `systemctl stop` is asynchronous by default, so we poll until the
# service actually reports inactive. Without the wait the dd race
# against the dying udisks2 was the most common cause of intermittent
# corruption on otherwise byte-perfect writes.
UDISKS2_WAS_ACTIVE=0
if systemctl is-active --quiet udisks2.service 2>/dev/null; then
    UDISKS2_WAS_ACTIVE=1
    # Mask the service first. `systemctl stop` alone is not enough on
    # systems where another process (a DBus client, a udev rule, an
    # auto-mount agent) bus-activates udisks2 again seconds later.
    # `mask` symlinks the unit to /dev/null, which blocks every form
    # of (re)activation until we unmask. This is the only reliable
    # way to keep the daemon down during a write.
    systemctl mask udisks2.service 2>/dev/null || true
    systemctl stop udisks2.service 2>/dev/null || true
    # Poll up to 10 s for the stop job to complete.
    for _wait in 1 2 3 4 5 6 7 8 9 10; do
        if ! systemctl is-active --quiet udisks2.service 2>/dev/null; then
            break
        fi
        sleep 1
    done
    # If it is somehow still alive, SIGKILL the process itself. With
    # the unit masked nothing can restart it.
    if systemctl is-active --quiet udisks2.service 2>/dev/null; then
        pkill -9 -x udisksd 2>/dev/null || true
        sleep 1
    fi
fi
restore_udisks2() {
    if [ "$UDISKS2_WAS_ACTIVE" -eq 1 ]; then
        systemctl unmask udisks2.service 2>/dev/null || true
        systemctl start udisks2.service 2>/dev/null || true
    fi
}
trap restore_udisks2 EXIT

# Unmount any existing mount points for partitions of this device in a
# four-level fallback chain matching rpi-imager's platformquirks_linux:
# normal -> MNT_EXPIRE x2 -> MNT_DETACH (lazy) -> MNT_FORCE.
# umount2(MNT_EXPIRE) marks a mountpoint "expired"; the second call
# expires it for real if no access happened in between, which is the
# graceful path. MNT_FORCE is documented as "may cause data loss" and
# is only used as a last resort.
unmount_all() {
    for part in "${DEVICE}"*; do
        [ -b "$part" ] || continue
        # Politely tell the desktop session manager to release the mount
        udisksctl unmount -b "$part" --no-user-interaction 2>/dev/null || true
        # Normal umount
        umount "$part" 2>/dev/null && continue
        # MNT_EXPIRE pair (graceful retire)
        umount --no-canonicalize "$part" 2>/dev/null || true
        sleep 0.2
        umount --no-canonicalize "$part" 2>/dev/null && continue
        # Lazy unmount (MNT_DETACH): hides from namespace, lets in-flight
        # I/O complete in background.
        umount -l "$part" 2>/dev/null && continue
        # Force (MNT_FORCE): last resort. We log it because if we get
        # here the filesystem may have unflushed writes.
        if umount -f "$part" 2>/dev/null; then
            echo "warning: forced unmount of $part (data may be lost)" >&2
        fi
    done
}

# Aggressive unmount + give the kernel a beat to settle uevents
unmount_all
sleep 2

# Flush kernel buffers (do NOT wipefs or write garbage to the device
# before the real dd: every byte written before the image is bytes the
# image has to overwrite, and on cheap SD controllers the in-flight
# writes from those pre-passes were producing a deterministic post-dd
# hash mismatch even when the image dd itself completed cleanly).
blockdev --flushbufs "$DEVICE" 2>/dev/null || true

# Tiny pause for udev events from the unmounts above to settle. We
# deliberately do NOT zero the first/last MB any more: the image
# writes through the same byte range, and the pre-passes were
# implicated in the deterministic-mismatch corruption pattern.
sleep 1
blockdev --flushbufs "$DEVICE" 2>/dev/null || true

# Inform the GUI we're entering the write stage. The native helper
# below publishes live byte counts to PROGRESS_FILE during write and
# "STAGE:verifying:NN" during verify; the Rust poller in flash.rs
# already understands both formats. Skipping the legacy STAGE:speed_*
# events: they were tied to the dd path and the GUI just shows a
# generic "writing" label now.
echo "STAGE:speed_ok:0" > "$PROGRESS_FILE"
echo "STAGE:writing" > "$PROGRESS_FILE"

# === WRITE + VERIFY (native helper) ===
# The previous version invoked `dd if=$IMAGE of=$DEVICE oflag=dsync
# conv=fsync` inline. oflag=dsync forced a per-chunk sync, which we
# needed back when udisks2 was racing the write; with systemctl-mask
# udisks2 above, the race is gone and the per-chunk sync is just
# burning throughput. We now spawn the archr-flash-write helper which
# does pwrite() in 4 MiB chunks (O_DIRECT when supported), a periodic
# fsync every 64 MiB to bound the dirty-page backlog, then streams
# the SD content through SHA-256 for in-process verification. The
# helper publishes live byte counts to PROGRESS_FILE for the writing
# bar and "STAGE:verifying:NN" for the verify bar; the existing Rust
# poller already speaks both.
HELPER_ARGS=()
if [ "$VERIFY" = "0" ]; then
    HELPER_ARGS+=("--no-verify")
fi
"$HELPER" "${HELPER_ARGS[@]}" "$IMAGE" "$DEVICE" "$PROGRESS_FILE"
HELPER_RC=$?

if [ "$HELPER_RC" -ne 0 ]; then
    # "dd failed:" prefix kept for the Rust matcher in flash.rs.
    echo "dd failed: archr-flash-write exit $HELPER_RC" >&2
    exit 1
fi

# Belt-and-suspenders: extra global sync after the helper. The helper
# already fsynced its own fd, but this catches any other dirty pages
# the kernel was holding for this device under different fds.
sync

# archr-flash-write already streamed the device through SHA-256 and
# aborted with non-zero exit if the hash differed from the image. No
# second verify here.

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

echo "STAGE:done" > "$PROGRESS_FILE"

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
    verify: bool,
) -> Result<(), String> {
    let _ = verify;  // TODO: thread through into PowerShell flash
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    const CREATE_NO_WINDOW: u32 = 0x08000000;

    validate_device(device)?;

    let is_xz = image_path.ends_with(".xz");
    let is_gz = image_path.ends_with(".gz") && !is_xz;
    let needs_decompress = is_xz || is_gz;

    if needs_decompress {
        check_temp_space(image_path)?;
    }

    // Step 1: Decompress .xz or .gz in user space (with progress). The
    // PowerShell writer streams the file to the device raw, so a gzipped
    // image (the format ArchR ships) must be expanded here first, exactly
    // like the Linux and macOS paths do.
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

    // Cleanup temp files. The decompressed .img is removed regardless
    // of the source format. Previously only is_xz triggered the cleanup
    // and gz-derived .img files stayed at /tmp/archr-flash-temp.img
    // (5 GB) until reboot. needs_decompress covers both branches.
    let _ = fs::remove_file(&script_path);
    let _ = fs::remove_file(&progress_file);
    if needs_decompress {
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
