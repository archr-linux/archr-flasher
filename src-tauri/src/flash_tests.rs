// File-backed regression tests. No block devices or privilege escalation.
use crate::preflight::validate_flash_inputs;
use crate::rawwrite_unix::{patch_boot_partition as patch_fd, verify_image_on_raw_fd as verify_fd, write_image_to_raw_fd as write_prepared};
use crate::preflight::PreparedImage;

fn patch_boot_partition(image: &Path, dtbo: &Path, variant: &str) -> Result<(), String> {
    let mut file = OpenOptions::new().read(true).write(true).open(image).map_err(|e| e.to_string())?;
    patch_fd(&mut file, dtbo, variant)
}

fn write_image_to_raw_fd(dev: &mut File, path: &Path, size: Option<u64>, progress: impl FnMut(u64)) -> Result<crate::rawwrite_unix::WriteOutcome, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut image = PreparedImage::from_file(file, path.to_path_buf())?;
    write_prepared(dev, &mut image, size, progress)
}

fn verify_image_on_raw_fd(dev: &mut File, path: &Path, size: u64, progress: impl FnMut(u64)) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    verify_fd(dev, &mut file, size, progress)
}
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const BOOT_START: u64 = 1024 * 1024;
const BOOT_SIZE: u64 = 40 * 1024 * 1024;

fn boot_image(dir: &Path) -> PathBuf {
    let path = dir.join("source.img");
    let mut img = File::create(&path).unwrap();
    img.set_len(BOOT_START + BOOT_SIZE + 2 * 1024 * 1024).unwrap();
    let mut mbr = [0u8; 512];
    mbr[450] = 0x0c;
    mbr[454..458].copy_from_slice(&((BOOT_START / 512) as u32).to_le_bytes());
    mbr[458..462].copy_from_slice(&((BOOT_SIZE / 512) as u32).to_le_bytes());
    for (index, kind) in [(1, 0xda), (2, 0x83)] {
        let entry = &mut mbr[446 + index * 16..462 + index * 16];
        entry[4] = kind;
        let start = (BOOT_START + BOOT_SIZE) / 512 + (index as u64 - 1) * 2048;
        entry[8..12].copy_from_slice(&(start as u32).to_le_bytes());
        entry[12..16].copy_from_slice(&2048u32.to_le_bytes());
    }
    mbr[510..512].copy_from_slice(&[0x55, 0xaa]);
    img.write_all(&mbr).unwrap();
    drop(img);
    let img = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut slice = fscommon::StreamSlice::new(img, BOOT_START, BOOT_START + BOOT_SIZE).unwrap();
    fatfs::format_volume(
        &mut slice,
        fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat32),
    )
    .unwrap();
    slice.seek(SeekFrom::Start(0)).unwrap();
    let fat = fatfs::FileSystem::new(slice, fatfs::FsOptions::new()).unwrap();
    {
        let root = fat.root_dir();
        root.create_dir("overlays")
            .unwrap()
            .create_file("mipi-panel.dtbo")
            .unwrap()
            .write_all(b"original panel")
            .unwrap();
        let extlinux = root.create_dir("extlinux").unwrap();
        extlinux
            .create_file("extlinux.conf")
            .unwrap()
            .write_all(b"default config")
            .unwrap();
        extlinux
            .create_file("extlinux.conf.soysauce")
            .unwrap()
            .write_all(b"soysauce config")
            .unwrap();
    }
    fat.unmount().unwrap();
    path
}

fn boot_file(image: &Path, name: &str) -> Vec<u8> {
    let img = File::open(image).unwrap();
    let slice = fscommon::StreamSlice::new(img, BOOT_START, BOOT_START + BOOT_SIZE).unwrap();
    let fat = fatfs::FileSystem::new(slice, fatfs::FsOptions::new()).unwrap();
    let mut bytes = Vec::new();
    fat.root_dir()
        .open_file(name)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

#[test]
fn skip_overlay_preserves_default_and_writes_variant() {
    let dir = tempfile::tempdir().unwrap();
    let image = boot_image(dir.path());
    validate_flash_inputs(image.to_str().unwrap(), "").unwrap();
    patch_boot_partition(&image, Path::new(""), "original").unwrap();
    assert_eq!(
        boot_file(&image, "overlays/mipi-panel.dtbo"),
        b"original panel"
    );
    assert_eq!(boot_file(&image, "variant"), b"original");
}

#[test]
fn selected_overlay_replaces_default_without_trailing_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let image = boot_image(dir.path());
    let overlay = dir.path().join("panel.dtbo");
    fs::write(&overlay, b"new").unwrap();
    patch_boot_partition(&image, &overlay, "clone").unwrap();
    assert_eq!(boot_file(&image, "overlays/mipi-panel.dtbo"), b"new");
    assert_eq!(boot_file(&image, "variant"), b"clone");
}

#[test]
fn skip_overlay_still_switches_soysauce_config() {
    let dir = tempfile::tempdir().unwrap();
    let image = boot_image(dir.path());
    patch_boot_partition(&image, Path::new(""), "soysauce").unwrap();
    assert_eq!(
        boot_file(&image, "overlays/mipi-panel.dtbo"),
        b"original panel"
    );
    assert_eq!(
        boot_file(&image, "extlinux/extlinux.conf"),
        b"soysauce config"
    );
    assert_eq!(
        boot_file(&image, "extlinux/extlinux.conf.bak"),
        b"default config"
    );
}

#[test]
fn missing_selected_overlay_does_not_modify_image() {
    let dir = tempfile::tempdir().unwrap();
    let image = boot_image(dir.path());
    let before = fs::read(&image).unwrap();
    assert!(patch_boot_partition(&image, &dir.path().join("missing.dtbo"), "original").is_err());
    assert_eq!(fs::read(&image).unwrap(), before);
}

#[test]
fn missing_image_does_not_zero_target() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.img");
    let original = vec![0xa5; 3 * 1024 * 1024];
    fs::write(&target, &original).unwrap();
    let mut dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&target)
        .unwrap();
    assert!(
        write_image_to_raw_fd(
            &mut dev,
            &dir.path().join("missing.img"),
            Some(original.len() as u64),
            |_| {}
        )
        .is_err()
    );
    assert!(
        fs::read(target).unwrap() == original,
        "target changed before the source image was opened"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_native_writer_verifies_file_target_without_wiping_unused_tail() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("source.img");
    let target = dir.path().join("target.img");
    let progress = dir.path().join("progress");
    crate::test_support::valid_image(&image, 16 * 1024 * 1024 + 512);
    let bytes = fs::read(&image).unwrap();
    fs::write(&target, vec![0xa5; 20 * 1024 * 1024]).unwrap();
    assert_eq!(helper(&image, &target, &progress), 0);
    let written = fs::read(&target).unwrap();
    assert!(written[..bytes.len()] == bytes);
    assert_eq!(&written[written.len() - 1024..], &[0xa5; 1024]);
}

#[test]
fn skip_overlay_can_write_and_verify_a_file_backed_target() {
    let dir = tempfile::tempdir().unwrap();
    let image = boot_image(dir.path());
    validate_flash_inputs(image.to_str().unwrap(), "").unwrap();
    patch_boot_partition(&image, Path::new(""), "original").unwrap();
    let target = dir.path().join("target.img");
    let mut dev = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&target)
        .unwrap();
    let size = fs::metadata(&image).unwrap().len();
    dev.set_len(size + 2 * 1024 * 1024).unwrap();
    let outcome =
        write_image_to_raw_fd(&mut dev, &image, Some(size + 2 * 1024 * 1024), |_| {}).unwrap();
    assert_eq!(outcome.bytes, size);
    verify_image_on_raw_fd(&mut dev, &image, size, |_| {}).unwrap();
    assert_eq!(
        boot_file(&target, "overlays/mipi-panel.dtbo"),
        b"original panel"
    );
}

#[test]
fn preflight_rejects_tiny_unaligned_and_non_mbr_images() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("invalid.img");
    for bytes in [vec![1], vec![0; 1025], vec![0; 16 * 1024 * 1024]] {
        fs::write(&image, bytes).unwrap();
        assert!(validate_flash_inputs(image.to_str().unwrap(), "").is_err());
    }
}

#[test]
fn image_larger_than_target_is_rejected_without_writes() {
    let dir = tempfile::tempdir().unwrap();
    let image = boot_image(dir.path());
    let target = dir.path().join("small-target.img");
    let original = vec![0xa5; 3 * 1024 * 1024];
    fs::write(&target, &original).unwrap();
    let mut dev = OpenOptions::new().read(true).write(true).open(&target).unwrap();
    let result = write_image_to_raw_fd(&mut dev, &image, Some(original.len() as u64), |_| {});
    assert!(result.is_err(), "must not extend a file-backed target or start writing a small disk");
    assert!(fs::read(&target).unwrap() == original);
}

#[test]
fn directory_substituted_after_preflight_does_not_zero_target() {
    let dir = tempfile::tempdir().unwrap();
    let image = boot_image(dir.path());
    let inputs = validate_flash_inputs(image.to_str().unwrap(), "").unwrap();
    let mut prepared = PreparedImage::from_file(inputs.image, image.clone()).unwrap();
    fs::rename(&image, dir.path().join("saved.img")).unwrap();
    fs::create_dir(&image).unwrap();
    let target = dir.path().join("target.img");
    let original = vec![0xa5; 45 * 1024 * 1024];
    fs::write(&target, &original).unwrap();
    let mut dev = OpenOptions::new().read(true).write(true).open(&target).unwrap();
    write_prepared(&mut dev, &mut prepared, Some(original.len() as u64), |_| {}).unwrap();
    let written = fs::read(&target).unwrap();
    let saved = fs::read(dir.path().join("saved.img")).unwrap();
    assert!(written[..saved.len()] == saved, "must write the file held by preflight, not its substituted pathname");
}

#[test]
fn overlay_replacement_after_validation_cannot_change_staged_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let image = boot_image(dir.path());
    let overlay = dir.path().join("panel.dtbo");
    fs::write(&overlay, b"validated").unwrap();
    let inputs = validate_flash_inputs(image.to_str().unwrap(), overlay.to_str().unwrap()).unwrap();
    fs::rename(&overlay, dir.path().join("validated.dtbo")).unwrap();
    fs::write(&overlay, b"replacement").unwrap();
    let staged = crate::preflight::stage_overlay(inputs.overlay).unwrap().unwrap();
    assert_eq!(fs::read(staged.path()).unwrap(), b"validated");
}

// ---------------------------------------------------------------------------
// Regressions from the external review probes. Each reproduces a case that
// fa9d0de accepted; every target here is a regular file, never a device.
// ---------------------------------------------------------------------------

fn helper(image: &Path, target: &Path, progress: &Path) -> i32 {
    // As the GUI does: open once, pass that descriptor and its identity.
    let file = File::open(image).unwrap();
    let id = crate::flashwrite::image_identity(&file).unwrap();
    let args = [id, target.to_string_lossy().into_owned(), progress.to_string_lossy().into_owned()];
    crate::flashwrite::entry_with_image(file, &args)
}

#[test]
fn helper_rejects_one_byte_image_before_touching_target() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("corrupt.img");
    let target = dir.path().join("target.img");
    let original = vec![0xa5; 3 * 1024 * 1024];
    fs::write(&image, [0x42]).unwrap();
    fs::write(&target, &original).unwrap();
    assert!(validate_flash_inputs(image.to_str().unwrap(), "").is_err());
    assert_ne!(helper(&image, &target, &dir.path().join("progress")), 0);
    assert!(fs::read(&target).unwrap() == original);
}

#[test]
fn helper_rejects_image_without_archr_partition_layout() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("plain.img");
    let target = dir.path().join("target.img");
    // Right size and alignment, valid signature, but no partitions.
    let mut bytes = vec![0u8; 16 * 1024 * 1024];
    bytes[510] = 0x55;
    bytes[511] = 0xaa;
    fs::write(&image, &bytes).unwrap();
    let original = vec![0xa5; 20 * 1024 * 1024];
    fs::write(&target, &original).unwrap();
    assert_ne!(helper(&image, &target, &dir.path().join("progress")), 0);
    assert!(fs::read(&target).unwrap() == original);
}

#[test]
fn helper_rejects_image_larger_than_target_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("image.img");
    let target = dir.path().join("small-target.img");
    crate::test_support::valid_image(&image, 16 * 1024 * 1024);
    let original = vec![0xa5; 3 * 1024 * 1024];
    fs::write(&target, &original).unwrap();
    assert_ne!(helper(&image, &target, &dir.path().join("progress")), 0);
    // A file-backed target must not have been extended or overwritten.
    assert!(fs::read(&target).unwrap() == original);
}

#[test]
fn rawwriter_rejects_unknown_capacity_of_non_file_target() {
    // A capacity the writer cannot determine is a refusal, not "assume it fits".
    assert!(crate::preflight::validate_capacity(16 * 1024 * 1024, None, 512).is_err());
    assert!(crate::preflight::validate_capacity(16 * 1024 * 1024, Some(16 * 1024 * 1024), 512).is_ok());
    assert!(crate::preflight::validate_capacity(16 * 1024 * 1024, Some(16 * 1024 * 1024 - 512), 512).is_err());
    // O_DIRECT pads the tail to 4 KiB; that padding must also fit.
    assert!(crate::preflight::validate_capacity(16 * 1024 * 1024 + 512, Some(16 * 1024 * 1024 + 512), 4096).is_err());
}

#[test]
fn image_truncated_after_preflight_does_not_touch_target() {
    let dir = tempfile::tempdir().unwrap();
    let image = boot_image(dir.path());
    let inputs = validate_flash_inputs(image.to_str().unwrap(), "").unwrap();
    let mut prepared = PreparedImage::from_file(inputs.image, image.clone()).unwrap();
    // Same inode, changed in place: the retained descriptor must notice.
    OpenOptions::new().write(true).open(&image).unwrap().set_len(1).unwrap();
    let target = dir.path().join("target.img");
    let original = vec![0xa5; 45 * 1024 * 1024];
    fs::write(&target, &original).unwrap();
    let mut dev = OpenOptions::new().read(true).write(true).open(&target).unwrap();
    assert!(write_prepared(&mut dev, &mut prepared, Some(original.len() as u64), |_| {}).is_err());
    assert!(fs::read(&target).unwrap() == original, "no wipe before a successful first read");
}

#[test]
fn compressed_retry_never_reuses_output_of_failed_decompression() {
    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("raw.img");
    crate::test_support::valid_image(&raw, 16 * 1024 * 1024);
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(&fs::read(&raw).unwrap()).unwrap();
    let mut bytes = enc.finish().unwrap();
    let crc_at = bytes.len() - 8;
    bytes[crc_at] ^= 1;
    let archive = dir.path().join("corrupt.img.gz");
    fs::write(&archive, &bytes).unwrap();
    let cache = dir.path().join("cache");
    for _ in 0..2 {
        let inputs = validate_flash_inputs(archive.to_str().unwrap(), "").unwrap();
        assert!(crate::image_cache::prepare(inputs.image, &archive, &cache).is_err());
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 0, "failed output must be removed");
    }
}

#[test]
fn linux_helper_writes_the_validated_image_even_if_its_path_is_replaced() {
    // Validate A, then put B (also a valid ArchR layout) at the same path.
    // The helper must write A, the file whose descriptor it was given.
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("selected.img");
    let b = dir.path().join("replacement.img");
    let target = dir.path().join("target.img");
    let progress = dir.path().join("progress");
    crate::test_support::valid_image(&a, 16 * 1024 * 1024);
    crate::test_support::valid_image(&b, 16 * 1024 * 1024);
    for (path, marker) in [(&a, b'A'), (&b, b'B')] {
        let mut file = OpenOptions::new().write(true).open(path).unwrap();
        file.seek(SeekFrom::Start(1024)).unwrap();
        file.write_all(&[marker]).unwrap();
    }
    let inputs = validate_flash_inputs(a.to_str().unwrap(), "").unwrap();
    let prepared = PreparedImage::from_file(inputs.image, a.clone()).unwrap();
    let id = crate::flashwrite::image_identity(&prepared.file).unwrap();
    fs::rename(&b, &a).unwrap();
    File::create(&target).unwrap().set_len(20 * 1024 * 1024).unwrap();
    let args = [id.clone(), target.to_string_lossy().into_owned(), progress.to_string_lossy().into_owned()];
    assert_eq!(crate::flashwrite::entry_with_image(prepared.file.try_clone().unwrap(), &args), 0);
    let written = fs::read(&target).unwrap();
    assert_eq!(written[1024], b'A', "helper wrote the replacement, not the validated image");

    // A descriptor that is not the validated file is refused before any write.
    let original = vec![0xa5; 20 * 1024 * 1024];
    fs::write(&target, &original).unwrap();
    let other = File::open(&a).unwrap(); // now B
    assert_ne!(crate::flashwrite::entry_with_image(other, &args), 0);
    assert!(fs::read(&target).unwrap() == original);
}

#[test]
fn linux_helper_refuses_a_writable_image_descriptor() {
    // The decompressed cache is opened O_RDWR. The helper must refuse it
    // unless the GUI handed over the read-only reopening.
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.img");
    let progress = dir.path().join("progress");
    let cached = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
    crate::test_support::valid_image(cached.path(), 16 * 1024 * 1024);
    let original = vec![0xa5; 20 * 1024 * 1024];
    fs::write(&target, &original).unwrap();
    let id = crate::flashwrite::image_identity(cached.as_file()).unwrap();
    let args = [id, target.to_string_lossy().into_owned(), progress.to_string_lossy().into_owned()];
    assert_ne!(crate::flashwrite::entry_with_image(cached.as_file().try_clone().unwrap(), &args), 0);
    assert!(fs::read(&target).unwrap() == original);
    let read_only = crate::flashwrite::read_only_descriptor(cached.as_file()).unwrap();
    assert_eq!(crate::flashwrite::entry_with_image(read_only, &args), 0);
}
