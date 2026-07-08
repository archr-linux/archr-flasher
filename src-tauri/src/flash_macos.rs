// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) 2026 ArchR
//
// macOS flash path: unmount via diskutil, open the raw /dev/rdisk through
// /usr/libexec/authopen (the fd comes back over SCM_RIGHTS) and stream the
// image from this process, exactly like rpi-imager's macfile.cpp. The
// previous osascript+dd approach died at the raw open on current macOS
// before writing a single byte; authopen is the sanctioned way for a GUI
// app to get an authorized raw-device fd. The boot-partition setup happens
// afterwards as the plain user on the remounted FAT volume, no root needed.
#![allow(unused_imports)]

use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use serde::Serialize;
use crate::flash::{emit_progress, validate_device, check_temp_space, poll_flash_progress, decompress_xz, decompress_gz};
use crate::rawwrite_unix::{receive_fd_from_helper, write_image_to_raw_fd, configure_boot_volume, FdHelperError};
use crate::diaglog::DiagLog;

#[cfg(target_os = "macos")]
pub fn flash_image_privileged(
    app: &AppHandle,
    image_path: &str,
    device: &str,
    custom_dtbo_path: &str,
    variant: &str,
    verify: bool,
) -> Result<(), String> {
    let mut diag = DiagLog::new();
    let result = flash_core(app, image_path, device, custom_dtbo_path, variant, verify, &mut diag);
    match &result {
        Ok(()) => diag.push_str("result: success\n"),
        Err(e) => diag.push_str(&format!("result: {}\n", e)),
    }
    result.map_err(crate::diaglog::with_log_hint)
}

#[cfg(target_os = "macos")]
fn flash_core(
    app: &AppHandle,
    image_path: &str,
    device: &str,
    custom_dtbo_path: &str,
    variant: &str,
    verify: bool,
    diag: &mut DiagLog,
) -> Result<(), String> {
    use std::process::Command;

    validate_device(device)?;
    diag.push_str(&format!("device: {}\nimage: {}\nverify: {}\n", device, image_path, verify));

    let is_xz = image_path.ends_with(".xz");
    let is_gz = image_path.ends_with(".gz") && !is_xz;
    let needs_decompress = is_xz || is_gz;

    if needs_decompress {
        check_temp_space(image_path)?;
    }

    // Step 1: decompress in user space (with progress)
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

    let image_size = fs::metadata(&img_to_flash).map(|m| m.len()).unwrap_or(0);

    // Step 2: device size for the last-MB signature wipe (best effort)
    let device_size = diskutil_device_bytes(device);
    diag.push_str(&format!("device bytes: {:?}\n", device_size));

    // Step 3: unmount everything on the disk (macOS auto-mounts SD cards)
    let unmount = Command::new("diskutil")
        .args(["unmountDisk", "force", device])
        .output();
    if let Ok(o) = &unmount {
        diag.push_str(&format!(
            "unmountDisk: {:?} {}\n", o.status,
            String::from_utf8_lossy(&o.stderr)
        ));
    }

    // Step 4: authorized raw fd via authopen. The password prompt is
    // authopen's own; the user dismissing it surfaces as a non-zero exit.
    let rdisk = device.replace("/dev/disk", "/dev/rdisk");
    let args = vec![
        "-stdoutpipe".to_string(),
        "-o".to_string(),
        libc::O_RDWR.to_string(),
        rdisk.clone(),
    ];
    emit_progress(app, 55.0, "writing");
    let fd = match receive_fd_from_helper("/usr/libexec/authopen", &args) {
        Ok(fd) => fd,
        Err(FdHelperError::Refused(code, err)) => {
            diag.push_str(&format!("authopen refused (code {}): {}\n", code, err));
            return Err("cancelled".into());
        }
        Err(FdHelperError::NoFd(err)) => {
            diag.push_str(&format!("authopen sent no fd: {}\n", err));
            return Err("err:auth_cancelled authopen returned no descriptor".into());
        }
        Err(FdHelperError::Spawn(err)) => {
            diag.push_str(&format!("authopen spawn: {}\n", err));
            return Err(format!("Failed to run authopen: {}", err));
        }
    };
    diag.push_str(&format!("authopen ok, writing to {}\n", rdisk));

    // Step 5: stream the image, feeding the existing progress poller
    // through the progress file it already understands.
    let progress_file = std::env::temp_dir().join("archr-flash-progress");
    fs::write(&progress_file, "0").ok();
    let stop = Arc::new(AtomicBool::new(false));
    let poll_handle = poll_flash_progress(
        app.clone(), progress_file.clone(), image_size, stop.clone(),
    );

    let mut dev = File::from(fd);
    let write_result = write_image_to_raw_fd(&mut dev, &img_to_flash, device_size, |done| {
        let _ = fs::write(&progress_file, done.to_string());
    });

    match &write_result {
        Ok(outcome) => {
            diag.push_str(&format!("wrote {} bytes\n", outcome.bytes));
            if let Some(w) = &outcome.sync_warning {
                diag.push_str(&format!("sync warning (non-fatal): {}\n", w));
            }
        }
        Err(e) => diag.push_str(&format!("write failed: {}\n", e)),
    }

    // Verify while the authorized descriptor is still open: reopening the
    // raw device would need a second password prompt.
    let verify_result = if verify && write_result.is_ok() {
        diag.push_str("verifying against the image, read back over the same fd\n");
        crate::rawwrite_unix::verify_image_on_raw_fd(&mut dev, &img_to_flash, image_size, |pct| {
            let _ = fs::write(&progress_file, format!("STAGE:verifying:{}", pct));
        })
    } else {
        Ok(())
    };
    if let Err(e) = &verify_result {
        diag.push_str(&format!("verify failed: {}\n", e));
    } else if verify && write_result.is_ok() {
        diag.push_str("verify ok\n");
    }

    drop(dev); // close the raw fd before diskutil touches the disk again

    stop.store(true, Ordering::Relaxed);
    let _ = poll_handle.join();
    let _ = fs::remove_file(&progress_file);
    if needs_decompress {
        let _ = fs::remove_file(&img_to_flash);
    }

    write_result?;
    verify_result?;

    // Step 6: remount and configure the boot partition as the plain user.
    emit_progress(app, 92.0, "configuring");
    let boot_vol = match mount_boot_volume(device, diag) {
        Some(v) => v,
        None => {
            let _ = Command::new("diskutil").args(["eject", device]).status();
            return Err(format!(
                "err:mount_boot could not mount {}s1 after flashing (image was written)",
                device
            ));
        }
    };
    diag.push_str(&format!("boot volume: {}\n", boot_vol.display()));

    let cfg = configure_boot_volume(&boot_vol, Path::new(custom_dtbo_path), variant);
    if let Err(e) = &cfg {
        diag.push_str(&format!("configure failed: {}\n", e));
        let _ = Command::new("diskutil").args(["eject", device]).status();
    }
    cfg?;

    emit_progress(app, 95.0, "syncing");
    let _ = Command::new("sync").status();
    let _ = Command::new("diskutil").args(["eject", device]).status();

    emit_progress(app, 100.0, "done");
    Ok(())
}

/// Disk size in bytes from `diskutil info`, e.g. "Disk Size: 31.9 GB
/// (31914983424 Bytes)".
#[cfg(target_os = "macos")]
fn diskutil_device_bytes(device: &str) -> Option<u64> {
    use std::process::Command;
    let out = Command::new("diskutil").args(["info", device]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    for line in text.lines() {
        if line.contains("Disk Size:") {
            let start = line.find('(')?;
            let end = line.find(" Bytes")?;
            let num: String = line[start + 1..end]
                .chars().filter(|c| c.is_ascii_digit()).collect();
            return num.parse().ok();
        }
    }
    None
}

/// Remount the freshly written disk and resolve the boot (s1) mount point.
/// mountDisk mounts every volume; the explicit s1 mount covers the case
/// where diskutil skips the boot FAT on the first pass.
#[cfg(target_os = "macos")]
fn mount_boot_volume(device: &str, diag: &mut DiagLog) -> Option<PathBuf> {
    use std::process::Command;
    let part = format!("{}s1", device);
    for attempt in 1..=5 {
        thread::sleep(Duration::from_secs(2));
        let _ = Command::new("diskutil").args(["mountDisk", device]).output();
        let _ = Command::new("diskutil").args(["mount", &part]).output();
        thread::sleep(Duration::from_secs(1));
        if let Ok(out) = Command::new("diskutil").args(["info", &part]).output() {
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            for line in text.lines() {
                if line.contains("Mount Point:") {
                    if let Some(mp) = line.splitn(2, ':').nth(1) {
                        let mp = mp.trim();
                        if !mp.is_empty() && Path::new(mp).is_dir() {
                            return Some(PathBuf::from(mp));
                        }
                    }
                }
            }
            if attempt == 5 {
                diag.push_str(&format!("diskutil info {}:\n{}\n", part, text));
            }
        }
    }
    None
}
