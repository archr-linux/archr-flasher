// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) 2026 ArchR
//
// Privileged flash-write helper, folded into the main binary as the
// hidden `__flash-write` subcommand (see main.rs). It used to be a
// separate cargo binary under src/bin/, but that gave the crate two
// binaries and the Tauri bundler could not tell which one was the app,
// so the installer shipped the wrong executable and the GUI never
// started. As a subcommand there is exactly one binary on every
// platform, always shipped side-by-side with itself.
//
// Invocation (under pkexec, from the Linux flash script):
//   archr-flasher __flash-write [--no-verify] <image> <device> <progress_file>
//
// Behaviour:
//   1. Open image (read) and device (write+O_DIRECT when supported).
//   2. Loop pwrite() in 4 MiB chunks; no dsync per chunk.
//   3. fsync() every 64 MiB so the kernel cannot accumulate gigabytes
//      of dirty pages and stall at the end.
//   4. After write: fsync, drop caches via posix_fadvise, stream-verify
//      SHA-256 of the SD content against the source.
//   5. Throughout: byte counters written to <progress_file> in two
//      forms:
//        - bare integer  -> bytes written so far (Rust polling thread
//          maps it to the 55:90% writing bar in the GUI)
//        - "STAGE:verifying:NN"  -> percent through verify
//
// This module is Linux-only (relies on O_DIRECT, posix_fadvise): it is
// only declared with #[cfg(target_os = "linux")] in main.rs, so the
// whole file compiles nowhere else.

use std::alloc::{alloc_zeroed, dealloc, handle_alloc_error, Layout};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::Instant;
use sha2::{Sha256, Digest};

/// Prepare a device fd for the verify pass, exactly like rpi-imager
/// does in LinuxFileOperations::PrepareForSequentialRead: invalidate
/// the page cache for the byte range, then hint the kernel to
/// read-ahead sequentially. Two calls, in this order.
///
///   POSIX_FADV_DONTNEED   = page cache for [offset, offset+len) is
///                           dropped, so verify reads from the SD,
///                           not from whatever the kernel buffered
///                           during the write.
///   POSIX_FADV_SEQUENTIAL = upcoming reads will be sequential; the
///                           kernel issues larger read-aheads.
///
/// Both calls are best-effort: a non-zero return means the verify is
/// slower, not incorrect.
fn prepare_for_sequential_read(fd: i32, len: u64) {
    unsafe extern "C" {
        fn posix_fadvise(fd: i32, offset: i64, len: i64, advice: i32) -> i32;
    }
    const POSIX_FADV_SEQUENTIAL: i32 = 2;
    const POSIX_FADV_DONTNEED: i32 = 4;
    // SAFETY: posix_fadvise on a valid fd with non-negative offset/len
    // is always well-defined.
    unsafe {
        posix_fadvise(fd, 0, len as i64, POSIX_FADV_DONTNEED);
        posix_fadvise(fd, 0, len as i64, POSIX_FADV_SEQUENTIAL);
    }
}

/// 4 KiB aligned byte buffer. Required when writing to a file
/// descriptor opened with O_DIRECT: the kernel rejects any write
/// whose buffer address is not aligned to the device's logical
/// block size (4 KiB covers every SD/eMMC controller we have seen)
/// with EINVAL. Vec<u8> from the global allocator gives at most
/// 16-byte alignment, which trips O_DIRECT on the very first
/// pwrite. AlignedBuffer goes through std::alloc::alloc with an
/// explicit 4 KiB layout.
struct AlignedBuffer {
    ptr: *mut u8,
    layout: Layout,
}

impl AlignedBuffer {
    fn new(size: usize, align: usize) -> Self {
        let layout = Layout::from_size_align(size, align)
            .expect("AlignedBuffer: invalid layout");
        // SAFETY: layout has non-zero size and valid alignment.
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            handle_alloc_error(layout);
        }
        Self { ptr, layout }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: ptr is valid for layout.size() bytes; lifetime is
        // bound to &mut self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        // SAFETY: ptr came from alloc_zeroed(layout); freeing with
        // the same layout is the documented contract.
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

const CHUNK_BYTES: usize = 4 * 1024 * 1024;      // pwrite chunk size
const FSYNC_INTERVAL_BYTES: u64 = 64 * 1024 * 1024; // fsync cadence
const O_DIRECT: i32 = 0x4000;                     // not exported by libc on every target

/// Subcommand entry point. `args` is everything after the
/// `__flash-write` token (image, device, progress_file, plus an
/// optional `--no-verify` anywhere). Returns the process exit code.
pub fn entry(args: &[String]) -> i32 {
    // `--no-verify` may appear anywhere among the args. The three
    // positional args (image, device, progress_file) keep the same
    // order. rpi-imager exposes the same toggle on the CLI as
    // `--disable-verify`; we keep our spelling consistent with the
    // existing flasher copy ("verify after writing").
    let verify_enabled = !args.iter().any(|a| a == "--no-verify");
    let positional: Vec<&String> = args.iter()
        .filter(|a| !a.starts_with("--"))
        .collect();
    if positional.len() < 3 {
        eprintln!("usage: archr-flasher __flash-write [--no-verify] <image> <device> <progress_file>");
        return 2;
    }
    let image_path = positional[0];
    let device_path = positional[1];
    let progress_path = positional[2];

    eprintln!("=== archr-flash-write start ===");
    eprintln!("  image    = {}", image_path);
    eprintln!("  device   = {}", device_path);
    eprintln!("  progress = {}", progress_path);
    eprintln!("  verify   = {}", if verify_enabled { "yes" } else { "no" });

    if let Err(e) = run(image_path, device_path, progress_path, verify_enabled) {
        eprintln!("flash error: {}", e);
        return 1;
    }
    eprintln!("=== archr-flash-write done ===");
    0
}

fn write_progress(path: &str, content: &str) {
    // Best-effort. If we can't write progress, the GUI just stops
    // updating; the write itself continues.
    let _ = std::fs::write(path, content);
}

fn run(
    image_path: &str,
    device_path: &str,
    progress_path: &str,
    verify_enabled: bool,
) -> Result<(), String> {
    // Sanity
    if !Path::new(image_path).is_file() {
        return Err(format!("source image not found: {}", image_path));
    }
    let metadata = std::fs::metadata(image_path)
        .map_err(|e| format!("cannot stat image: {}", e))?;
    let image_size = metadata.len();
    eprintln!("  image_size = {} bytes ({:.2} GiB)",
        image_size, image_size as f64 / (1024.0 * 1024.0 * 1024.0));

    let mut image = File::open(image_path)
        .map_err(|e| format!("cannot open image: {}", e))?;

    // Open the device O_DIRECT if possible. O_DIRECT bypasses the page
    // cache so writes go straight to the device; this is what makes the
    // measurable speedup over dd(1) without dsync : the kernel never
    // accumulates a multi-gigabyte dirty-page backlog that has to be
    // flushed at the end and looks like a stall.
    //
    // O_DIRECT requires the buffer to be aligned to the device's block
    // size (usually 512 bytes or 4 KiB). We allocate via AlignedBuffer
    // to satisfy that; the global allocator alone would not.
    let device = OpenOptions::new()
        .write(true)
        .custom_flags(O_DIRECT)
        .open(device_path);

    let direct = device.is_ok();
    let mut device = if direct {
        device.unwrap()
    } else {
        // Fallback: open without O_DIRECT. Still much faster than
        // dd|dsync because there are no per-chunk syncs; we just fsync
        // periodically.
        eprintln!("  O_DIRECT not available, falling back to buffered I/O");
        OpenOptions::new()
            .write(true)
            .open(device_path)
            .map_err(|e| format!("cannot open device: {}", e))?
    };

    write_progress(progress_path, "STAGE:writing");

    // 4 KiB aligned buffer is mandatory when the device fd was opened
    // with O_DIRECT. Even on the buffered fallback an aligned buffer
    // is cheap and harmless, so we use the same allocation in both
    // paths.
    let mut aligned = AlignedBuffer::new(CHUNK_BYTES, 4096);
    let buf = aligned.as_mut_slice();
    let mut written: u64 = 0;
    let mut last_fsync_at: u64 = 0;
    let started = Instant::now();

    // Hash the source as we stream it through. This is the rpi-imager
    // pattern: a single read pass over the image computes both the
    // bytes-to-write and the reference SHA-256. Skips re-reading the
    // 5 GB image from disk during verify (the previous implementation
    // did `hash_file(image_path)` after the write, which doubled the
    // total disk I/O on the source).
    let mut image_hasher = Sha256::new();

    loop {
        let to_read = ((image_size - written).min(CHUNK_BYTES as u64)) as usize;
        if to_read == 0 { break; }

        // Read from image. May return short read at EOF.
        let mut filled = 0;
        while filled < to_read {
            let n = image.read(&mut buf[filled..to_read])
                .map_err(|e| format!("image read at offset {}: {}", written + filled as u64, e))?;
            if n == 0 { break; }
            filled += n;
        }
        if filled == 0 { break; }

        // Hash the REAL image bytes (not the zero-padded tail). The
        // verify pass below will hash the same byte range from the
        // device, so the comparison is meaningful even though the
        // device may have an extra 0..4095 padding bytes at the tail.
        image_hasher.update(&buf[..filled]);

        // O_DIRECT requires write sizes aligned to the block size. The
        // last chunk may be a partial. Two options: pad with zero up to
        // 4 KiB alignment (works because the trailing tail of the disk
        // is unused), or close+reopen without O_DIRECT for the tail.
        // We pad: simpler and only affects the very last write.
        let aligned_size = if direct {
            (filled + 4095) & !4095
        } else {
            filled
        };
        if direct && aligned_size > filled {
            for byte in &mut buf[filled..aligned_size] {
                *byte = 0;
            }
        }

        device.write_all(&buf[..aligned_size])
            .map_err(|e| format!("device write at offset {}: {}", written, e))?;
        written += filled as u64;

        // Publish raw byte count for the GUI poller.
        write_progress(progress_path, &written.to_string());

        // Periodic fsync to bound the kernel's dirty-page backlog.
        if written - last_fsync_at >= FSYNC_INTERVAL_BYTES {
            device.sync_data()
                .map_err(|e| format!("fsync at offset {}: {}", written, e))?;
            last_fsync_at = written;
        }
    }

    // Final fsync.
    device.sync_all()
        .map_err(|e| format!("final fsync: {}", e))?;
    let elapsed = started.elapsed();
    let mb_per_s = (written as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64();
    eprintln!("  write done: {} bytes in {:.1}s ({:.1} MiB/s)",
        written, elapsed.as_secs_f64(), mb_per_s);

    let write_hash = format!("{:x}", image_hasher.finalize());

    if !verify_enabled {
        eprintln!("  verify skipped (--no-verify)");
        eprintln!("  write hash: {}", write_hash);
        write_progress(progress_path, "STAGE:done");
        return Ok(());
    }

    // Verify pass, in the same style as rpi-imager:
    //   - posix_fadvise(DONTNEED) + (SEQUENTIAL) on the device fd so
    //     the verify reads the SD content, not the cache left over
    //     from the write.
    //   - Stream the device through Sha256; compare only the final
    //     hashes (rpi-imager does NOT do per-chunk byte-by-byte
    //     compare with offset reporting, so we don't either).
    //   - Throughput is reported as a verifying percentage in the
    //     progress file at most once per percent.
    prepare_for_sequential_read(device.as_raw_fd(), written);
    drop(device);  // close so the verify open sees a fresh fd state

    write_progress(progress_path, "STAGE:verifying:0");
    let verify_hash = verify_device(device_path, written, progress_path)?;

    if verify_hash != write_hash {
        return Err(format!(
            "Verifying write failed. Contents of SD card is different from what was written to it.\n  expected: {}\n  got:      {}",
            write_hash, verify_hash));
    }
    eprintln!("  verify ok: {}", verify_hash);
    write_progress(progress_path, "STAGE:done");
    Ok(())
}

/// Stream the device through SHA-256 and return the hash. No per-chunk
/// compare: rpi-imager's `_verify()` only compares the final
/// `_verifyhash.result()` against `_writehash.result()`, and reaching
/// past those hashes adds I/O without catching anything they don't.
fn verify_device(
    device_path: &str,
    bytes: u64,
    progress_path: &str,
) -> Result<String, String> {
    let mut device = File::open(device_path)
        .map_err(|e| format!("verify: cannot open device: {}", e))?;
    device.seek(SeekFrom::Start(0))
        .map_err(|e| format!("verify: seek device: {}", e))?;
    prepare_for_sequential_read(device.as_raw_fd(), bytes);

    // 4 MiB matches the write chunk size: rpi-imager picks an adaptive
    // size based on system memory and image size; a fixed 4 MiB is the
    // simple version that still gets the kernel read-ahead going.
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_BYTES];
    let mut read_so_far: u64 = 0;
    let mut last_pct_reported: u32 = u32::MAX;

    while read_so_far < bytes {
        let want = ((bytes - read_so_far).min(buf.len() as u64)) as usize;
        let n = device.read(&mut buf[..want])
            .map_err(|e| format!("Error reading from storage. SD card may be broken. (offset {}: {})", read_so_far, e))?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
        read_so_far += n as u64;

        let pct = (read_so_far as f64 / bytes as f64 * 100.0) as u32;
        if pct != last_pct_reported {
            write_progress(progress_path, &format!("STAGE:verifying:{}", pct));
            last_pct_reported = pct;
        }
    }
    if read_so_far != bytes {
        return Err(format!(
            "verify: short read on device ({} of {} bytes)", read_so_far, bytes));
    }
    Ok(format!("{:x}", hasher.finalize()))
}
