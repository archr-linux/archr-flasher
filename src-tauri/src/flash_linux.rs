// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) 2026 ArchR
//
// Linux flash path: pkexec + bash orchestration driving the native
// archr-flash-write helper (see flashwrite.rs). Split out of flash.rs so
// each OS owns its own flash logic.
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
use crate::diaglog::DiagLog;

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
    let mut diag = DiagLog::new();
    let result = flash_core_linux(
        app, image_path, device, custom_dtbo_path, variant, verify, &mut diag,
    );
    match &result {
        Ok(()) => diag.push_str("result: success\n"),
        Err(e) => diag.push_str(&format!("result: {}\n", e)),
    }
    result.map_err(crate::diaglog::with_log_hint)
}

#[cfg(target_os = "linux")]
fn flash_core_linux(
    app: &AppHandle,
    image_path: &str,
    device: &str,
    custom_dtbo_path: &str,
    variant: &str,
    verify: bool,
    diag: &mut DiagLog,
) -> Result<(), String> {
    use std::process::{Command, Stdio};
    use std::os::unix::fs::PermissionsExt;

    validate_device(device)?;
    diag.push_str(&format!(
        "device: {}\nimage: {}\nverify: {}\n", device, image_path, verify
    ));

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
    diag.push_str(&format!("systemd-inhibit available: {}\n", has_inhibit));

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

    // The native write helper is the app binary itself, run as the
    // `__flash-write` subcommand (see main.rs). Using our own exe means
    // it is always present next to the GUI (it IS the GUI), so there is
    // nothing extra for the bundler to ship. The script invokes it as
    // `"$HELPER" __flash-write ...`.
    let helper_path = std::env::current_exe()
        .map_err(|e| format!("cannot resolve exe path: {}", e))?;
    if !helper_path.is_file() {
        return Err(format!(
            "Native write helper not found at {}",
            helper_path.display()
        ));
    }

    // Under an AppImage, current_exe() points inside the user's FUSE
    // mount (/tmp/.mount_*), which root cannot read: FUSE denies other
    // users by default, root included. The pkexec'd script then failed
    // to exec the helper and the flash died immediately with a generic
    // error, while the .deb/.rpm installs (exe in /usr/bin) worked.
    // Stage a root-readable copy of the exe outside the mount.
    let helper_path = if std::env::var_os("APPIMAGE").is_some() {
        let staged = std::env::temp_dir().join("archr-flash-helper");
        fs::copy(&helper_path, &staged)
            .map_err(|e| format!("Cannot stage helper for pkexec: {}", e))?;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("Cannot set helper permissions: {}", e))?;
        staged
    } else {
        helper_path
    };

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

    // The helper script narrates every stage to stderr; keep the whole
    // capture in the diagnostic log regardless of the outcome (no env
    // or secrets ever go through this channel).
    diag.push_str(&format!(
        "helper exit: {:?}\n--- helper stderr ---\n{}\n---\n",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    ));

    // Stop polling thread
    stop.store(true, Ordering::Relaxed);
    let _ = poll_handle.join();

    // Cleanup temp files. The decompressed .img is removed regardless
    // of the source format. Previously only is_xz triggered the cleanup
    // and gz-derived .img files stayed at /tmp/archr-flash-temp.img
    // (5 GB) until reboot. needs_decompress covers both branches.
    let _ = fs::remove_file(&script_path);
    let _ = fs::remove_file(&progress_file);
    if std::env::var_os("APPIMAGE").is_some() {
        let _ = fs::remove_file(std::env::temp_dir().join("archr-flash-helper"));
    }
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
"$HELPER" __flash-write "${HELPER_ARGS[@]}" "$IMAGE" "$DEVICE" "$PROGRESS_FILE"
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
