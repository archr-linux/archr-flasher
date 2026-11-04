use std::fs::File;
use std::io::Write;
use std::path::Path;

/// Small sparse disk with the same three-entry MBR structure as ArchR.
/// No device paths, loop devices or privileged operations are used.
pub(crate) fn valid_image(path: &Path, size: u64) {
    let mut file = File::create(path).unwrap();
    file.set_len(size).unwrap();
    let mut mbr = [0u8; 512];
    for (index, (kind, start, count)) in [
        (0x0c, 2048u32, 16384u32),
        (0xda, 18432, 8192),
        (0x83, 26624, 4096),
    ].into_iter().enumerate() {
        let entry = &mut mbr[446 + index * 16..462 + index * 16];
        entry[4] = kind;
        entry[8..12].copy_from_slice(&start.to_le_bytes());
        entry[12..16].copy_from_slice(&count.to_le_bytes());
    }
    mbr[510..512].copy_from_slice(&[0x55, 0xaa]);
    file.write_all(&mbr).unwrap();
}

/// Bytes that differ per file and per run of the generator, without a RNG
/// dependency (xorshift64).
pub(crate) fn pattern(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

pub(crate) const ARCHR_BOOT_START: u64 = 1024 * 1024;
pub(crate) const ARCHR_BOOT_SIZE: u64 = 64 * 1024 * 1024;
pub(crate) const ARCHR_DTBS: &[&str] = &[
    "rk3326-gameconsole-r36s.dtb",
    "rk3326-gameconsole-r36s-v20.dtb",
    "rk3326-gameconsole-r36h.dtb",
    "rk3326-odroid-go2.dtb",
    "rk3326-odroid-go3.dtb",
    "rk3326-powkiddy-rgb10.dtb",
];

/// ArchR-like card image: FAT32 boot with 16 KiB clusters and fewer
/// clusters than the FAT32 minimum (the quirk the vendor U-Boot needs),
/// a DTB set, the default panel overlay and both extlinux configs,
/// followed by root and storage partitions.
pub(crate) fn archr_boot_image(path: &Path) {
    use std::io::{Seek, SeekFrom};
    let boot_sectors = (ARCHR_BOOT_SIZE / 512) as u32;
    let start = (ARCHR_BOOT_START / 512) as u32;
    let mut file = std::fs::OpenOptions::new()
        .read(true).write(true).create(true).truncate(true)
        .open(path).unwrap();
    file.set_len(ARCHR_BOOT_START + ARCHR_BOOT_SIZE + 3 * 1024 * 1024).unwrap();
    let mut mbr = [0u8; 512];
    for (index, (kind, first, count)) in [
        (0x0c, start, boot_sectors),
        (0xda, start + boot_sectors, 2048),
        (0x83, start + boot_sectors + 2048, 2048),
    ].into_iter().enumerate() {
        let entry = &mut mbr[446 + index * 16..462 + index * 16];
        entry[4] = kind;
        entry[8..12].copy_from_slice(&first.to_le_bytes());
        entry[12..16].copy_from_slice(&count.to_le_bytes());
    }
    mbr[510..512].copy_from_slice(&[0x55, 0xaa]);
    file.write_all(&mbr).unwrap();

    let mut slice = fscommon::StreamSlice::new(
        file, ARCHR_BOOT_START, ARCHR_BOOT_START + ARCHR_BOOT_SIZE,
    ).unwrap();
    fatfs::format_volume(
        &mut slice,
        fatfs::FormatVolumeOptions::new()
            .fat_type(fatfs::FatType::Fat32)
            .bytes_per_cluster(16 * 1024),
    ).unwrap();
    slice.seek(SeekFrom::Start(0)).unwrap();
    let fat = fatfs::FileSystem::new(slice, fatfs::FsOptions::new()).unwrap();
    {
        let root = fat.root_dir();
        root.create_file("KERNEL").unwrap().write_all(&pattern(1, 300_000)).unwrap();
        let dtbs = root.create_dir("dtbs").unwrap();
        for (i, name) in ARCHR_DTBS.iter().enumerate() {
            let bytes = pattern(100 + i as u64, 40_000 + i * 7_919);
            dtbs.create_file(name).unwrap().write_all(&bytes).unwrap();
        }
        root.create_dir("overlays").unwrap()
            .create_file("mipi-panel.dtbo").unwrap()
            .write_all(b"original panel").unwrap();
        let extlinux = root.create_dir("extlinux").unwrap();
        extlinux.create_file("extlinux.conf").unwrap().write_all(b"default config").unwrap();
        extlinux.create_file("extlinux.conf.soysauce").unwrap().write_all(b"soysauce config").unwrap();
    }
    fat.unmount().unwrap();
}

/// One file of the boot FAT of an image or a file-backed card.
pub(crate) fn boot_file(image: &Path, name: &str) -> Vec<u8> {
    use std::io::Read;
    let file = File::open(image).unwrap();
    let slice = fscommon::StreamSlice::new(
        file, ARCHR_BOOT_START, ARCHR_BOOT_START + ARCHR_BOOT_SIZE,
    ).unwrap();
    let fat = fatfs::FileSystem::new(slice, fatfs::FsOptions::new()).unwrap();
    let mut bytes = Vec::new();
    fat.root_dir().open_file(name).unwrap().read_to_end(&mut bytes).unwrap();
    bytes
}
