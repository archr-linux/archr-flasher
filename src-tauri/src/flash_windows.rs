// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) 2026 ArchR
//
// Native Windows raw-disk writer, ported from rpi-imager's Windows engine
// (src/windows/diskpart_util.cpp + file_operations_windows.cpp).
//
// Why this exists: the previous Windows path shelled out to PowerShell
// (`Clear-Disk` + a .NET `FileStream` write). That is fragile — between
// clearing the disk and opening the FileStream, Windows can re-mount the
// volumes, and the raw write to sectors owned by a freshly-mounted volume
// comes back as "media is write protected" / access denied. Users saw the
// generic "Flash failed. Check that the SD card is ... write-protected"
// error even on perfectly good cards.
//
// rpi-imager avoids this by, in order:
//   1. Lock + dismount every volume on the target disk, then remove its
//      drive letter (DeleteVolumeMountPoint) so Explorer stops touching it.
//   2. Delete the partition layout (IOCTL_DISK_DELETE_DRIVE_LAYOUT).
//   3. Open \\.\PhysicalDriveN with FILE_FLAG_NO_BUFFERING |
//      FILE_FLAG_WRITE_THROUGH and write sector-aligned chunks.
//   4. Verify by reading the device back and comparing SHA-256.
//   5. Re-read the partition table (IOCTL_DISK_UPDATE_PROPERTIES) so the
//      boot partition re-enumerates for the post-write config step.
//
// The app already runs elevated (admin.manifest), so we can do all of this
// in-process with no PowerShell and no extra elevation.

use std::alloc::{alloc_zeroed, dealloc, handle_alloc_error, Layout};
use std::ffi::c_void;
use std::fs::File;
use std::io::Read;
use std::os::raw::c_ulong;
use sha2::{Sha256, Digest};

type Handle = isize;
const INVALID_HANDLE_VALUE: Handle = -1;

// Desired access / share / disposition / flags for CreateFileW.
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ: u32 = 0x0000_0001;
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;
const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;

// DeviceIoControl codes (CTL_CODE-encoded, see winioctl.h).
const FSCTL_LOCK_VOLUME: u32 = 0x0009_0018;
const FSCTL_DISMOUNT_VOLUME: u32 = 0x0009_0020;
const FSCTL_ALLOW_EXTENDED_DASD_IO: u32 = 0x0009_0083;
const IOCTL_DISK_UPDATE_PROPERTIES: u32 = 0x0007_0140;
const IOCTL_STORAGE_GET_DEVICE_NUMBER: u32 = 0x002D_1080;

// A subset of Win32 error codes we translate into actionable messages.
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_WRITE_PROTECT: u32 = 19;
const ERROR_SHARING_VIOLATION: u32 = 32;

const FILE_BEGIN: u32 = 0;

// STORAGE_DEVICE_NUMBER (winioctl.h): identifies which physical disk a
// volume lives on, so we can find every volume belonging to our target.
#[repr(C)]
struct StorageDeviceNumber {
    device_type: c_ulong,
    device_number: c_ulong,
    partition_number: c_ulong,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileW(
        lp_file_name: *const u16,
        dw_desired_access: u32,
        dw_share_mode: u32,
        lp_security_attributes: *mut c_void,
        dw_creation_disposition: u32,
        dw_flags_and_attributes: u32,
        h_template_file: Handle,
    ) -> Handle;
    fn DeviceIoControl(
        h_device: Handle,
        dw_io_control_code: u32,
        lp_in_buffer: *mut c_void,
        n_in_buffer_size: u32,
        lp_out_buffer: *mut c_void,
        n_out_buffer_size: u32,
        lp_bytes_returned: *mut u32,
        lp_overlapped: *mut c_void,
    ) -> i32;
    fn WriteFile(
        h_file: Handle,
        lp_buffer: *const u8,
        n_number_of_bytes_to_write: u32,
        lp_number_of_bytes_written: *mut u32,
        lp_overlapped: *mut c_void,
    ) -> i32;
    fn ReadFile(
        h_file: Handle,
        lp_buffer: *mut u8,
        n_number_of_bytes_to_read: u32,
        lp_number_of_bytes_read: *mut u32,
        lp_overlapped: *mut c_void,
    ) -> i32;
    fn SetFilePointerEx(
        h_file: Handle,
        li_distance_to_move: i64,
        lp_new_file_pointer: *mut i64,
        dw_move_method: u32,
    ) -> i32;
    fn FlushFileBuffers(h_file: Handle) -> i32;
    fn CloseHandle(h_object: Handle) -> i32;
    fn GetLastError() -> u32;
    fn GetLogicalDrives() -> u32;
    fn DeleteVolumeMountPointW(lpsz_volume_mount_point: *const u16) -> i32;
}

/// 4 KiB-aligned buffer, mandatory for FILE_FLAG_NO_BUFFERING: the kernel
/// rejects reads/writes whose buffer address or size is not sector-aligned.
struct AlignedBuffer {
    ptr: *mut u8,
    layout: Layout,
}

impl AlignedBuffer {
    fn new(size: usize) -> Self {
        let layout = Layout::from_size_align(size, 4096).expect("aligned layout");
        // SAFETY: layout has non-zero size and valid alignment.
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            handle_alloc_error(layout);
        }
        Self { ptr, layout }
    }
    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: ptr is valid for layout.size() bytes, bound to &mut self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        // SAFETY: ptr came from alloc_zeroed with this exact layout.
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

const CHUNK_BYTES: usize = 4 * 1024 * 1024; // multiple of the 4 KiB sector alignment
const SECTOR: u64 = 4096;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn write_progress(path: &str, content: &str) {
    let _ = std::fs::write(path, content);
}

/// Parse the disk number from a `\\.\PhysicalDriveN` path.
fn extract_disk_number(device: &str) -> Result<u32, String> {
    let upper = device.to_uppercase();
    let idx = upper
        .find("PHYSICALDRIVE")
        .ok_or_else(|| format!("Not a physical drive path: {}", device))?;
    let digits: String = upper[idx + "PHYSICALDRIVE".len()..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits
        .parse::<u32>()
        .map_err(|_| format!("No disk number in: {}", device))
}

/// RAII wrapper so every early return closes the handle.
struct OwnedHandle(Handle);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: self.0 is a valid handle we own.
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn open_handle(path: &str, access: u32, flags: u32) -> Result<OwnedHandle, u32> {
    let wpath = wide(path);
    // SAFETY: wpath is a valid NUL-terminated UTF-16 string; other args are
    // plain scalars. Returns INVALID_HANDLE_VALUE on failure.
    let h = unsafe {
        CreateFileW(
            wpath.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            flags,
            0,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        // SAFETY: no intervening calls since CreateFileW failed.
        Err(unsafe { GetLastError() })
    } else {
        Ok(OwnedHandle(h))
    }
}

fn ioctl(h: Handle, code: u32) -> bool {
    let mut returned: u32 = 0;
    // SAFETY: h is a valid handle; no in/out buffers for these control codes.
    unsafe {
        DeviceIoControl(
            h,
            code,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        ) != 0
    }
}

/// Lock + dismount every volume on the target physical disk and RETURN the
/// locked volume handles. The caller MUST keep them alive for the whole
/// write: Windows denies raw writes to a physical drive's sectors that fall
/// inside a mounted volume's extent unless that volume is locked (the classic
/// Win32DiskImager / Rufus pattern — dismounting then releasing the lock lets
/// Windows auto-remount the volume and re-arm the protection, which is why
/// WriteFile came back ERROR_ACCESS_DENIED). Dropping the returned handles
/// releases the locks. Best-effort: a volume that can't be locked is still
/// dismounted and its handle kept.
fn lock_and_dismount_volumes(disk: u32) -> Vec<OwnedHandle> {
    let mut held = Vec::new();
    // SAFETY: no arguments.
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let vol_path = format!("\\\\.\\{}:", letter);

        // Query which physical disk this volume sits on (0 access is enough
        // for IOCTL_STORAGE_GET_DEVICE_NUMBER).
        let query = match open_handle(&vol_path, 0, 0) {
            Ok(h) => h,
            Err(_) => continue,
        };
        let mut sdn = StorageDeviceNumber { device_type: 0, device_number: 0, partition_number: 0 };
        let mut returned: u32 = 0;
        // SAFETY: query.0 is valid; sdn is a correctly-sized out buffer.
        let ok = unsafe {
            DeviceIoControl(
                query.0,
                IOCTL_STORAGE_GET_DEVICE_NUMBER,
                std::ptr::null_mut(),
                0,
                &mut sdn as *mut _ as *mut c_void,
                std::mem::size_of::<StorageDeviceNumber>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            ) != 0
        };
        drop(query);
        if !ok || sdn.device_number != disk {
            continue;
        }

        eprintln!("[flash-win] locking + dismounting volume {}:", letter);
        let vh = match open_handle(&vol_path, GENERIC_READ | GENERIC_WRITE, 0) {
            Ok(h) => h,
            Err(_) => continue,
        };

        // Lock with geometric backoff (Explorer / AV / indexer may hold it).
        let mut delay = 100u64;
        let mut locked = false;
        for attempt in 0..8 {
            if ioctl(vh.0, FSCTL_LOCK_VOLUME) {
                locked = true;
                break;
            }
            if attempt < 7 {
                std::thread::sleep(std::time::Duration::from_millis(delay));
                delay *= 2;
            }
        }
        // Dismount whether or not the lock stuck; a dismounted+locked volume
        // is what lets the physical-drive write through.
        ioctl(vh.0, FSCTL_DISMOUNT_VOLUME);
        if !locked {
            eprintln!("[flash-win] warning: could not lock {}: — write may still be denied", letter);
        }

        // Remove the drive letter so Explorer stops probing it (mountpoint
        // must end in a backslash). This does not affect the lock we hold.
        let mp = wide(&format!("{}:\\", letter));
        // SAFETY: mp is a valid NUL-terminated UTF-16 string.
        unsafe { DeleteVolumeMountPointW(mp.as_ptr()) };

        // Keep the (locked) handle alive by moving it into the returned vec.
        held.push(vh);
    }
    held
}

/// Ask Windows to re-read the partition table so the boot partition
/// re-enumerates and can be mounted by the post-write config step.
fn rescan_disk(device: &str) {
    if let Ok(h) = open_handle(device, GENERIC_READ | GENERIC_WRITE, 0) {
        ioctl(h.0, IOCTL_DISK_UPDATE_PROPERTIES);
    }
    // Give the OS a moment to bring the new volume online before the
    // PowerShell config step tries to assign it a drive letter.
    std::thread::sleep(std::time::Duration::from_millis(1500));
}

// The returned strings are stable tokens (err:<kind>) that the frontend's
// translateError() maps to a localized message. The parenthetical detail is
// only for logs — the UI shows the translated key, never this English text.
fn classify_open_error(err: u32, device: &str) -> String {
    match err {
        ERROR_ACCESS_DENIED => format!("err:device_busy (access denied opening {})", device),
        ERROR_SHARING_VIOLATION => format!("err:device_busy ({} in use)", device),
        ERROR_WRITE_PROTECT => format!("err:write_protected ({})", device),
        _ => format!("err:open_device ({} win {})", device, err),
    }
}

fn classify_write_error(err: u32) -> String {
    match err {
        ERROR_WRITE_PROTECT => "err:write_protected (lock switch)".to_string(),
        ERROR_ACCESS_DENIED => "err:device_busy (write access denied)".to_string(),
        _ => format!("err:media (write win {})", err),
    }
}

/// Full raw write: unmount → clean → write → verify → rescan.
pub fn write_image(
    device: &str,
    image_path: &str,
    progress_path: &str,
    verify: bool,
) -> Result<(), String> {
    let disk = extract_disk_number(device)?;
    let image_size = std::fs::metadata(image_path)
        .map_err(|e| format!("cannot stat image: {}", e))?
        .len();

    eprintln!("=== archr flash (windows native) ===");
    eprintln!("  device = {} (disk {})", device, disk);
    eprintln!("  image  = {} ({} bytes)", image_path, image_size);
    eprintln!("  verify = {}", verify);

    // Lock + dismount the disk's volumes and HOLD the locks for the whole
    // write+verify. Two things matter here (learned from Rufus, format.c):
    //   - Releasing the locks early lets Windows remount the volume and deny
    //     the raw write (ERROR_ACCESS_DENIED).
    //   - We must NOT open+close a separate physical handle to "clean" the
    //     layout first: closing it makes Windows re-enumerate the disk and
    //     reassign the volume, which invalidates the lock we are holding and
    //     re-arms the write protection. The image write lays down the new
    //     partition table itself, so no pre-clean is needed.
    let volume_locks = lock_and_dismount_volumes(disk);

    write_progress(progress_path, "STAGE:writing");
    let write_hash = write_pass(device, image_path, image_size, progress_path)?;

    if verify {
        write_progress(progress_path, "STAGE:verifying:0");
        let read_hash = verify_pass(device, image_size, progress_path)?;
        if read_hash != write_hash {
            eprintln!("  verify mismatch: expected {} got {}", write_hash, read_hash);
            return Err("verify_failed".to_string());
        }
        eprintln!("  verify ok: {}", read_hash);
    }

    // Release the volume locks so the freshly-written partitions can mount
    // for the config step, then ask Windows to re-read the partition table.
    drop(volume_locks);
    rescan_disk(device);
    write_progress(progress_path, "STAGE:done");
    Ok(())
}

fn write_pass(
    device: &str,
    image_path: &str,
    image_size: u64,
    progress_path: &str,
) -> Result<String, String> {
    let mut image = File::open(image_path).map_err(|e| format!("cannot open image: {}", e))?;
    let dev = open_handle(
        device,
        GENERIC_READ | GENERIC_WRITE,
        FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH | FILE_FLAG_SEQUENTIAL_SCAN,
    )
    .map_err(|e| classify_open_error(e, device))?;

    // Disable I/O boundary checks on the write handle (Rufus does this before
    // every raw write — "I/O boundary checks disabled"). Without it, writes
    // that straddle the extents Windows still considers volume-owned can come
    // back ERROR_ACCESS_DENIED even with the volumes locked. Also take a
    // best-effort lock on the physical handle itself; it returns
    // ERROR_INVALID_FUNCTION on a non-volume handle, which is harmless.
    ioctl(dev.0, FSCTL_ALLOW_EXTENDED_DASD_IO);
    ioctl(dev.0, FSCTL_LOCK_VOLUME);

    let mut aligned = AlignedBuffer::new(CHUNK_BYTES);
    let buf = aligned.as_mut_slice();
    let mut hasher = Sha256::new();
    let mut written: u64 = 0;

    loop {
        let to_read = ((image_size - written).min(CHUNK_BYTES as u64)) as usize;
        if to_read == 0 {
            break;
        }
        let mut filled = 0;
        while filled < to_read {
            let n = image
                .read(&mut buf[filled..to_read])
                .map_err(|e| format!("image read at {}: {}", written + filled as u64, e))?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }

        // Hash the real image bytes (before zero-padding the tail).
        hasher.update(&buf[..filled]);

        // FILE_FLAG_NO_BUFFERING needs a sector-multiple write size; pad the
        // final short chunk with zeros (the disk tail is unused).
        let aligned_size = ((filled as u64 + SECTOR - 1) & !(SECTOR - 1)) as usize;
        for b in &mut buf[filled..aligned_size] {
            *b = 0;
        }

        let mut off = 0usize;
        while off < aligned_size {
            let mut wrote: u32 = 0;
            // SAFETY: dev is a valid handle; buf[off..aligned_size] is valid
            // for the given length; wrote is a valid out pointer.
            let ok = unsafe {
                WriteFile(
                    dev.0,
                    buf[off..].as_ptr(),
                    (aligned_size - off) as u32,
                    &mut wrote,
                    std::ptr::null_mut(),
                ) != 0
            };
            if !ok {
                // SAFETY: no intervening calls since WriteFile failed.
                return Err(classify_write_error(unsafe { GetLastError() }));
            }
            if wrote == 0 {
                return Err("err:media (write returned 0 bytes — card removed?)".to_string());
            }
            off += wrote as usize;
        }

        written += filled as u64;
        write_progress(progress_path, &written.to_string());
    }

    // SAFETY: dev is a valid handle.
    unsafe { FlushFileBuffers(dev.0) };
    eprintln!("  wrote {} bytes", written);
    Ok(format!("{:x}", hasher.finalize()))
}

fn verify_pass(device: &str, image_size: u64, progress_path: &str) -> Result<String, String> {
    let dev = open_handle(
        device,
        GENERIC_READ,
        FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN,
    )
    .map_err(|e| classify_open_error(e, device))?;
    // Rewind to the start of the disk.
    // SAFETY: dev is valid; null new-pointer is allowed.
    unsafe { SetFilePointerEx(dev.0, 0, std::ptr::null_mut(), FILE_BEGIN) };

    let mut aligned = AlignedBuffer::new(CHUNK_BYTES);
    let buf = aligned.as_mut_slice();
    let mut hasher = Sha256::new();
    let mut read_so_far: u64 = 0;
    let mut last_pct = u32::MAX;

    while read_so_far < image_size {
        // Reads under NO_BUFFERING must be sector-multiple, so always read a
        // full aligned chunk and only hash the real image bytes.
        let mut got: u32 = 0;
        // SAFETY: dev is valid; buf is valid for CHUNK_BYTES.
        let ok = unsafe {
            ReadFile(
                dev.0,
                buf.as_mut_ptr(),
                CHUNK_BYTES as u32,
                &mut got,
                std::ptr::null_mut(),
            ) != 0
        };
        if !ok {
            // SAFETY: no intervening calls since ReadFile failed.
            return Err(format!("err:media (read win {})", unsafe { GetLastError() }));
        }
        if got == 0 {
            break;
        }
        let want = ((image_size - read_so_far).min(got as u64)) as usize;
        hasher.update(&buf[..want]);
        read_so_far += want as u64;

        let pct = (read_so_far as f64 / image_size as f64 * 100.0) as u32;
        if pct != last_pct {
            write_progress(progress_path, &format!("STAGE:verifying:{}", pct));
            last_pct = pct;
        }
    }

    if read_so_far != image_size {
        return Err(format!(
            "err:media (short read {} of {} bytes)",
            read_so_far, image_size
        ));
    }
    Ok(format!("{:x}", hasher.finalize()))
}


// ---------------------------------------------------------------------------
// Windows flash orchestration (moved here from flash.rs). Uses the native
// write_image above, then a small PowerShell step for the post-write config.
// ---------------------------------------------------------------------------
#[allow(unused_imports)]
use crate::flash::{emit_progress, validate_device, check_temp_space, poll_flash_progress, decompress_xz, decompress_gz};
#[allow(unused_imports)]
use std::fs;
#[allow(unused_imports)]
use std::path::PathBuf;
#[allow(unused_imports)]
use std::sync::Arc;
#[allow(unused_imports)]
use std::sync::atomic::{AtomicBool, Ordering};
#[allow(unused_imports)]
use tauri::AppHandle;
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

    // Step 2: Raw write via the native writer (ported from rpi-imager). The
    // app is already elevated (admin.manifest), so this runs in-process with
    // no PowerShell. It unmounts/locks the disk's volumes, clears the layout,
    // writes with FILE_FLAG_NO_BUFFERING, verifies, then rescans — which is
    // what avoids the "media is write-protected" failures the old
    // Clear-Disk + FileStream path produced.
    let temp = std::env::temp_dir();
    let progress_file = temp.join("archr-flash-progress");
    let _ = fs::remove_file(&progress_file);
    fs::write(&progress_file, "0").ok();

    emit_progress(app, 60.0, "writing");

    let stop = Arc::new(AtomicBool::new(false));
    let poll_handle = poll_flash_progress(
        app.clone(), progress_file.clone(), image_size, stop.clone(),
    );

    let write_result = crate::flash_windows::write_image(
        device,
        &img_to_flash.to_string_lossy(),
        &progress_file.to_string_lossy(),
        verify,
    );

    stop.store(true, Ordering::Relaxed);
    let _ = poll_handle.join();

    if needs_decompress {
        let _ = fs::remove_file(&img_to_flash);
    }
    if let Err(e) = write_result {
        let _ = fs::remove_file(&progress_file);
        return Err(e);
    }

    // Step 3: Post-write config (mount boot partition, copy DTBO, write the
    // variant file, switch extlinux.conf for soysauce). This stays in
    // PowerShell — it only touches a mounted FAT partition with high-level
    // cmdlets, none of the fragile raw-disk work.
    emit_progress(app, 95.0, "syncing");

    let script_path = temp.join("archr-flash-config.ps1");
    let script_content = generate_windows_config_script(device, custom_dtbo_path, variant);
    fs::write(&script_path, &script_content)
        .map_err(|e| format!("Cannot write config script: {}", e))?;

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

    let _ = fs::remove_file(&script_path);
    let _ = fs::remove_file(&progress_file);

    if output.status.success() {
        emit_progress(app, 100.0, "done");
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!("Flash failed: {}", stderr.trim()))
}

/// Generate the post-write PowerShell config script. The raw image write is
/// done natively (see flash_windows::write_image), which already cleared,
/// wrote, verified and rescanned the disk. This script only mounts the boot
/// partition and lays down the panel config:
/// - Copy the custom DTBO as overlays/mipi-panel.dtbo
/// - Write the `variant` file
/// - Switch extlinux.conf for the soysauce variant
#[cfg(target_os = "windows")]
fn generate_windows_config_script(
    device: &str,
    custom_dtbo_path: &str,
    variant: &str,
) -> String {
    let esc = |s: &str| s.replace('\'', "''");
    format!(
        r#"$ErrorActionPreference = "Stop"
try {{
    $Device = '{device}'
    $CustomDTBO = '{custom_dtbo}'
    $Variant = '{variant}'

    $diskNum = [int]([regex]::Match($Device, '\d+$').Value)

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

    # Switch extlinux config for the soysauce variant (it boots with an
    # explicit FDT). This mirrors the Linux flash path; without it the
    # board keeps the default extlinux.conf, ignores extlinux.conf.soysauce,
    # and the console never boots.
    if ($Variant -eq 'soysauce') {{
        $extlinuxDir = Join-Path $bootDrive "extlinux"
        $extlinuxConf = Join-Path $extlinuxDir "extlinux.conf"
        $soysauceConf = Join-Path $extlinuxDir "extlinux.conf.soysauce"
        if (Test-Path $soysauceConf) {{
            if (Test-Path $extlinuxConf) {{
                Copy-Item $extlinuxConf (Join-Path $extlinuxDir "extlinux.conf.bak") -Force
            }}
            Copy-Item $soysauceConf $extlinuxConf -Force
        }}
    }}

    exit 0
}} catch {{
    Write-Error $_.Exception.Message
    exit 1
}}
"#,
        device = esc(device),
        custom_dtbo = esc(custom_dtbo_path),
        variant = esc(variant),
    )
}
