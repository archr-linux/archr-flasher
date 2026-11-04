// File-backed regression tests. No block devices or privilege escalation.
use crate::preflight::validate_flash_inputs;
use crate::rawwrite_unix::{patch_boot_partition, verify_image_on_raw_fd, write_image_to_raw_fd};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const BOOT_START: u64 = 1024 * 1024;
const BOOT_SIZE: u64 = 40 * 1024 * 1024;

fn boot_image(dir: &Path) -> PathBuf {
    let path = dir.join("source.img");
    let mut img = File::create(&path).unwrap();
    img.set_len(BOOT_START + BOOT_SIZE).unwrap();
    let mut mbr = [0u8; 512];
    mbr[450] = 0x0c;
    mbr[454..458].copy_from_slice(&((BOOT_START / 512) as u32).to_le_bytes());
    mbr[458..462].copy_from_slice(&((BOOT_SIZE / 512) as u32).to_le_bytes());
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
    let bytes = vec![0x5a; 4 * 1024 * 1024 + 123];
    fs::write(&image, &bytes).unwrap();
    fs::write(&target, vec![0xa5; 8 * 1024 * 1024]).unwrap();
    let args = [&image, &target, &progress].map(|p| p.to_string_lossy().into_owned());
    assert_eq!(crate::flashwrite::entry(&args), 0);
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
