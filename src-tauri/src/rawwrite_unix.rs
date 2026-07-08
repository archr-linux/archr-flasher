// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) 2026 ArchR
//
// Unix building blocks for the macOS raw-write path: spawn a helper
// (/usr/libexec/authopen on macOS) that opens the target device on our
// behalf and hands the open fd back over an AF_UNIX socketpair via
// SCM_RIGHTS, then stream the image onto that fd from this process.
// This mirrors what rpi-imager does in macfile.cpp and is the mechanism
// Apple sanctions for raw disk access from GUI apps; running dd as root
// under osascript stopped being reliable on current macOS.
//
// Everything here is OS-generic unix so the exact shipped code compiles
// and gets exercised on the Linux build host too; the macOS specifics
// (diskutil orchestration, the authopen binary path) live in
// flash_macos.rs. No inner attributes here: the test harness includes
// this file verbatim inside a mod block, where they are not allowed;
// the allow(dead_code) lives on the mod declaration in main.rs.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::process::{Command, Stdio};

const SECTOR: u64 = 512;
const CHUNK: usize = 4 * 1024 * 1024;
const MB: u64 = 1024 * 1024;

#[derive(Debug)]
pub enum FdHelperError {
    /// The helper could not be spawned at all.
    Spawn(String),
    /// The helper ran but exited non-zero without delivering an fd; for
    /// authopen this is the user dismissing the authorization prompt.
    Refused(i32, String),
    /// The helper exited cleanly yet no fd arrived (protocol breakage).
    NoFd(String),
}

/// Spawn `cmd args...` with its stdout connected to one end of an AF_UNIX
/// socketpair and receive an open file descriptor from it via SCM_RIGHTS.
pub fn receive_fd_from_helper(cmd: &str, args: &[String]) -> Result<OwnedFd, FdHelperError> {
    let mut sv = [0i32; 2];
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()) };
    if rc != 0 {
        return Err(FdHelperError::Spawn(format!(
            "socketpair: {}", std::io::Error::last_os_error()
        )));
    }
    let (parent_sock, child_sock) = (sv[0], sv[1]);

    // Stdio::from_raw_fd takes ownership of child_sock: after spawn the
    // parent copy is closed, so recvmsg sees EOF if the helper exits
    // without ever sending the fd.
    let spawned = unsafe {
        Command::new(cmd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::from_raw_fd(child_sock))
            .stderr(Stdio::piped())
            .spawn()
    };
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            unsafe { libc::close(parent_sock) };
            return Err(FdHelperError::Spawn(format!("{}: {}", cmd, e)));
        }
    };

    // Blocks until the helper sends the fd or closes its end. For
    // authopen this covers the whole time the password prompt is up.
    let received = recv_fd(parent_sock);
    unsafe { libc::close(parent_sock) };

    let mut stderr_text = String::new();
    if let Some(mut se) = child.stderr.take() {
        let _ = se.read_to_string(&mut stderr_text);
    }
    let status = child.wait();

    match received {
        Some(fd) => Ok(unsafe { OwnedFd::from_raw_fd(fd) }),
        None => {
            let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
            if code != 0 {
                Err(FdHelperError::Refused(code, stderr_text))
            } else {
                Err(FdHelperError::NoFd(stderr_text))
            }
        }
    }
}

/// recvmsg until an SCM_RIGHTS control message arrives; returns the fd.
fn recv_fd(sock: RawFd) -> Option<RawFd> {
    loop {
        let mut data = [0u8; 64];
        let mut iov = libc::iovec {
            iov_base: data.as_mut_ptr() as *mut libc::c_void,
            iov_len: data.len(),
        };
        // Room for one fd worth of control data, kept comfortably large.
        let mut cbuf = [0u8; 128];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1 as _;
        msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cbuf.len() as _;

        let n = unsafe { libc::recvmsg(sock, &mut msg, 0) };
        if n < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return None;
        }
        if n == 0 {
            return None; // helper closed its end without sending an fd
        }
        let mut c = unsafe { libc::CMSG_FIRSTHDR(&msg) };
        while !c.is_null() {
            let hdr = unsafe { &*c };
            if hdr.cmsg_level == libc::SOL_SOCKET && hdr.cmsg_type == libc::SCM_RIGHTS {
                let fd = unsafe { *(libc::CMSG_DATA(c) as *const libc::c_int) };
                return Some(fd);
            }
            c = unsafe { libc::CMSG_NXTHDR(&msg, c) };
        }
        // Plain data with no rights attached (authopen may chat first):
        // keep reading until the fd or EOF shows up.
    }
}

/// Zero the first and last MB (stale MBR/GPT/FS signatures survive a
/// reflash otherwise), then stream the image in 4MB chunks. Raw character
/// devices reject partial-sector writes, so the final short chunk is
/// padded with zeros up to the sector size; the padded tail lands past
/// the image end where the disk is unused. `progress` receives the count
/// of real image bytes written so far. Returns the image byte count.
pub fn write_image_to_raw_fd(
    dev: &mut File,
    image: &Path,
    device_size: Option<u64>,
    mut progress: impl FnMut(u64),
) -> Result<WriteOutcome, String> {
    let zeros = vec![0u8; MB as usize];
    dev.seek(SeekFrom::Start(0))
        .map_err(|e| format!("err:write_failed seek 0: {}", e))?;
    dev.write_all(&zeros)
        .map_err(|e| format!("err:write_failed wiping first MB: {}", e))?;
    if let Some(size) = device_size {
        if size > 2 * MB {
            let last_mb = (size / MB - 1) * MB;
            dev.seek(SeekFrom::Start(last_mb))
                .map_err(|e| format!("err:write_failed seek last MB: {}", e))?;
            dev.write_all(&zeros)
                .map_err(|e| format!("err:write_failed wiping last MB: {}", e))?;
        }
    }
    dev.seek(SeekFrom::Start(0))
        .map_err(|e| format!("err:write_failed seek rewind: {}", e))?;

    let mut src = File::open(image)
        .map_err(|e| format!("Cannot open image: {}", e))?;
    let mut buf = vec![0u8; CHUNK];
    let mut done: u64 = 0;
    loop {
        // Fill the chunk fully so short reads from the fs cache don't
        // translate into tiny unaligned device writes.
        let mut filled = 0usize;
        while filled < CHUNK {
            let n = src.read(&mut buf[filled..])
                .map_err(|e| format!("Image read at {}: {}", done + filled as u64, e))?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }
        let aligned = ((filled as u64 + SECTOR - 1) & !(SECTOR - 1)) as usize;
        buf[filled..aligned].fill(0);
        dev.write_all(&buf[..aligned])
            .map_err(|e| format!("err:write_failed at byte {}: {}", done, e))?;
        done += filled as u64;
        progress(done);
        if filled < CHUNK {
            break;
        }
    }

    // Raw character devices write unbuffered, and /dev/rdisk on macOS
    // rejects F_FULLFSYNC outright; rpi-imager treats sync failures here
    // as warnings for the same reason. Report instead of failing a flash
    // whose bytes are already on the card.
    if let Err(e) = full_sync(dev) {
        return Ok(WriteOutcome { bytes: done, sync_warning: Some(e.to_string()) });
    }
    Ok(WriteOutcome { bytes: done, sync_warning: None })
}

/// Result of a completed raw write: byte count plus a non-fatal sync
/// warning when the final flush was rejected by the device.
pub struct WriteOutcome {
    pub bytes: u64,
    pub sync_warning: Option<String>,
}

/// Read the device back over the same descriptor and compare against the
/// image file, chunk by chunk. Reports progress as 0..100 percent.
pub fn verify_image_on_raw_fd(
    dev: &mut File,
    image: &Path,
    image_size: u64,
    mut progress: impl FnMut(u64),
) -> Result<(), String> {
    dev.seek(SeekFrom::Start(0))
        .map_err(|e| format!("verify_failed seek: {}", e))?;
    let mut src = File::open(image)
        .map_err(|e| format!("verify_failed open image: {}", e))?;
    let mut img_buf = vec![0u8; CHUNK];
    let mut dev_buf = vec![0u8; CHUNK];
    let mut done: u64 = 0;
    while done < image_size {
        let want = std::cmp::min(CHUNK as u64, image_size - done) as usize;
        src.read_exact(&mut img_buf[..want])
            .map_err(|e| format!("verify_failed image read at {}: {}", done, e))?;
        // Device reads must stay sector aligned on raw devices.
        let aligned = ((want as u64 + SECTOR - 1) & !(SECTOR - 1)) as usize;
        dev.read_exact(&mut dev_buf[..aligned])
            .map_err(|e| format!("verify_failed device read at {}: {}", done, e))?;
        if img_buf[..want] != dev_buf[..want] {
            return Err(format!("verify_failed mismatch within {} bytes at offset {}", want, done));
        }
        done += want as u64;
        progress(done * 100 / image_size.max(1));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn full_sync(f: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // F_FULLFSYNC forces the drive itself to flush, plain fsync on macOS
    // only pushes to the drive cache.
    if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
        // Raw devices reject F_FULLFSYNC; plain fsync is the best left.
        if unsafe { libc::fsync(f.as_raw_fd()) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn full_sync(f: &File) -> std::io::Result<()> {
    f.sync_all()
}


/// Install the panel overlay, variant marker and (for soysauce) the
/// extlinux switch INSIDE the image file, before anything touches the
/// card. This is how the macOS path configures the boot partition: the
/// ArchR boot FAT is a below-spec-cluster-count FAT32 that the macOS
/// msdos driver refuses to mount, so post-flash configuration through
/// the OS is impossible there. Patching the image up front also means a
/// subsequent verify pass covers the configuration bytes too.
pub fn patch_boot_partition(image: &Path, dtbo: &Path, variant: &str) -> Result<(), String> {
    use std::io::Read;

    if !dtbo.is_file() {
        return Err(format!("err:image_patch custom DTBO not found at {}", dtbo.display()));
    }
    let mut dtbo_bytes = Vec::new();
    File::open(dtbo)
        .and_then(|mut f| f.read_to_end(&mut dtbo_bytes))
        .map_err(|e| format!("err:image_patch reading dtbo: {}", e))?;

    let mut img = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .map_err(|e| format!("err:image_patch opening image: {}", e))?;

    // First MBR partition entry: status byte at 446, type at 446+4,
    // start LBA (little endian u32) at 446+8, sector count at 446+12.
    let mut mbr = [0u8; 512];
    img.read_exact(&mut mbr)
        .map_err(|e| format!("err:image_patch reading MBR: {}", e))?;
    if mbr[510] != 0x55 || mbr[511] != 0xAA {
        return Err("err:image_patch image has no MBR signature".into());
    }
    let entry = &mbr[446..462];
    let ptype = entry[4];
    // FAT32 LBA (0x0c) or FAT32 CHS (0x0b); everything ArchR ships uses 0x0c.
    if ptype != 0x0c && ptype != 0x0b {
        return Err(format!(
            "err:image_patch first partition is not FAT32 (type 0x{:02x})",
            ptype
        ));
    }
    let start_lba = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as u64;
    let num_sectors = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as u64;
    if start_lba == 0 || num_sectors == 0 {
        return Err("err:image_patch empty boot partition entry".into());
    }
    let part_start = start_lba * 512;
    let part_end = part_start + num_sectors * 512;

    let slice = fscommon::StreamSlice::new(img, part_start, part_end)
        .map_err(|e| format!("err:image_patch slicing partition: {}", e))?;
    let fs = fatfs::FileSystem::new(slice, fatfs::FsOptions::new())
        .map_err(|e| format!("err:image_patch opening FAT: {}", e))?;
    {
        let root = fs.root_dir();

        let overlays = root
            .create_dir("overlays")
            .map_err(|e| format!("err:image_patch overlays dir: {}", e))?;
        let mut f = overlays
            .create_file("mipi-panel.dtbo")
            .map_err(|e| format!("err:image_patch creating overlay: {}", e))?;
        f.truncate()
            .map_err(|e| format!("err:image_patch truncating overlay: {}", e))?;
        f.write_all(&dtbo_bytes)
            .map_err(|e| format!("err:image_patch writing overlay: {}", e))?;
        drop(f);

        let mut v = root
            .create_file("variant")
            .map_err(|e| format!("err:image_patch creating variant: {}", e))?;
        v.truncate()
            .map_err(|e| format!("err:image_patch truncating variant: {}", e))?;
        v.write_all(variant.as_bytes())
            .map_err(|e| format!("err:image_patch writing variant: {}", e))?;
        drop(v);

        if variant == "soysauce" {
            let extlinux = root
                .open_dir("extlinux")
                .map_err(|e| format!("err:image_patch extlinux dir: {}", e))?;
            let mut soys = Vec::new();
            match extlinux.open_file("extlinux.conf.soysauce") {
                Ok(mut f) => {
                    f.read_to_end(&mut soys)
                        .map_err(|e| format!("err:image_patch reading soysauce conf: {}", e))?;
                }
                // Older images without the alternate config keep the default.
                Err(_) => {}
            }
            if !soys.is_empty() {
                let mut cur = Vec::new();
                extlinux
                    .open_file("extlinux.conf")
                    .and_then(|mut f| f.read_to_end(&mut cur).map(|_| ()))
                    .map_err(|e| format!("err:image_patch reading extlinux.conf: {}", e))?;
                let mut bak = extlinux
                    .create_file("extlinux.conf.bak")
                    .map_err(|e| format!("err:image_patch creating conf backup: {}", e))?;
                bak.truncate()
                    .map_err(|e| format!("err:image_patch truncating conf backup: {}", e))?;
                bak.write_all(&cur)
                    .map_err(|e| format!("err:image_patch writing conf backup: {}", e))?;
                drop(bak);
                let mut conf = extlinux
                    .open_file("extlinux.conf")
                    .map_err(|e| format!("err:image_patch opening extlinux.conf: {}", e))?;
                conf.truncate()
                    .map_err(|e| format!("err:image_patch truncating extlinux.conf: {}", e))?;
                conf.write_all(&soys)
                    .map_err(|e| format!("err:image_patch writing extlinux.conf: {}", e))?;
                drop(conf);
            }
        }
    }
    fs.unmount()
        .map_err(|e| format!("err:image_patch unmounting FAT: {}", e))?;
    Ok(())
}
