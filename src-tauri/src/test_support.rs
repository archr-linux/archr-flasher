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

    format_archr_fat32(&mut file, ARCHR_BOOT_START, boot_sectors);
    drop(file);

    // Populate with mtools, like the release images: fatfs is the code
    // under test and must not also build the fixture it is checked against.
    let target = format!("{}@@{}", path.display(), ARCHR_BOOT_START);
    let mtools = |tool: &str, args: &[&str]| {
        let out = std::process::Command::new(tool)
            .arg("-i").arg(&target).args(args)
            .env("MTOOLS_SKIP_CHECK", "1")
            .output()
            .unwrap_or_else(|e| panic!("{} is required for the FAT fixture (install mtools): {}", tool, e));
        assert!(out.status.success(), "{} {:?}: {}", tool, args, String::from_utf8_lossy(&out.stderr));
    };
    mtools("mmd", &["::/dtbs", "::/overlays", "::/extlinux"]);
    let staging = tempfile::tempdir().unwrap();
    let put = |name: &str, bytes: &[u8]| {
        let local = staging.path().join("f");
        std::fs::write(&local, bytes).unwrap();
        mtools("mcopy", &[local.to_str().unwrap(), &format!("::/{}", name)]);
    };
    put("KERNEL", &pattern(1, 300_000));
    for (i, name) in ARCHR_DTBS.iter().enumerate() {
        put(&format!("dtbs/{}", name), &pattern(100 + i as u64, 40_000 + i * 7_919));
    }
    put("overlays/mipi-panel.dtbo", b"original panel");
    put("extlinux/extlinux.conf", b"default config");
    put("extlinux/extlinux.conf.soysauce", b"soysauce config");

    let file = std::fs::File::open(path).unwrap();
    let slice = fscommon::StreamSlice::new(
        file, ARCHR_BOOT_START, ARCHR_BOOT_START + ARCHR_BOOT_SIZE,
    ).unwrap();
    let fat = fatfs::FileSystem::new(slice, fatfs::FsOptions::new()).unwrap();
    assert_eq!(fat.fat_type(), fatfs::FatType::Fat32, "fixture must be FAT32");
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

/// Sectors per cluster of the ArchR boot FAT (16 KiB clusters).
pub(crate) const ARCHR_SECTORS_PER_CLUSTER: u8 = 32;
const ARCHR_RESERVED_SECTORS: u16 = 32;

/// Lay down an empty FAT32 by hand, with the shipped geometry: 512-byte
/// sectors, 32 sectors per cluster, 32 reserved sectors, two FATs, root
/// directory at cluster 2. On a small partition this gives fewer clusters
/// than the FAT32 minimum of 65525, the quirk the vendor U-Boot needs.
/// fatfs::format_volume cannot produce it: with an explicit cluster size it
/// silently picks FAT16.
pub(crate) fn format_archr_fat32(file: &mut File, start: u64, sectors: u32) {
    use std::io::{Seek, SeekFrom};
    let spc = ARCHR_SECTORS_PER_CLUSTER as u32;
    let reserved = ARCHR_RESERVED_SECTORS as u32;
    // Smallest FAT that covers every data cluster (4 bytes per entry).
    let mut fat_sectors = 1u32;
    loop {
        let clusters = (sectors - reserved - 2 * fat_sectors) / spc;
        if (clusters + 2) * 4 <= fat_sectors * 512 {
            break;
        }
        fat_sectors += 1;
    }

    let mut boot = [0u8; 512];
    boot[0..3].copy_from_slice(&[0xeb, 0x58, 0x90]);
    boot[3..11].copy_from_slice(b"mkfs.fat");
    boot[11..13].copy_from_slice(&512u16.to_le_bytes());
    boot[13] = ARCHR_SECTORS_PER_CLUSTER;
    boot[14..16].copy_from_slice(&ARCHR_RESERVED_SECTORS.to_le_bytes());
    boot[16] = 2; // FATs; root entries and 16-bit sizes stay 0 on FAT32
    boot[21] = 0xf8;
    boot[24..26].copy_from_slice(&32u16.to_le_bytes());
    boot[26..28].copy_from_slice(&64u16.to_le_bytes());
    boot[28..32].copy_from_slice(&((start / 512) as u32).to_le_bytes());
    boot[32..36].copy_from_slice(&sectors.to_le_bytes());
    boot[36..40].copy_from_slice(&fat_sectors.to_le_bytes());
    boot[44..48].copy_from_slice(&2u32.to_le_bytes()); // root cluster
    boot[48..50].copy_from_slice(&1u16.to_le_bytes()); // FSInfo sector
    boot[50..52].copy_from_slice(&6u16.to_le_bytes()); // backup boot sector
    boot[64] = 0x80;
    boot[66] = 0x29;
    boot[67..71].copy_from_slice(&0x4152_4348u32.to_le_bytes());
    boot[71..82].copy_from_slice(b"NO NAME    ");
    boot[82..90].copy_from_slice(b"FAT32   ");
    boot[510..512].copy_from_slice(&[0x55, 0xaa]);

    let mut info = [0u8; 512];
    info[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
    info[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
    let clusters = (sectors - reserved - 2 * fat_sectors) / spc;
    info[488..492].copy_from_slice(&(clusters - 1).to_le_bytes()); // all free but root
    info[492..496].copy_from_slice(&3u32.to_le_bytes()); // next free cluster
    info[508..512].copy_from_slice(&0xaa55_0000u32.to_le_bytes());

    let mut put = |sector: u64, bytes: &[u8]| {
        file.seek(SeekFrom::Start(start + sector * 512)).unwrap();
        file.write_all(bytes).unwrap();
    };
    for base in [0u64, 6] {
        put(base, &boot);
        put(base + 1, &info);
    }
    // Media descriptor, end-of-chain marker, and the root directory cluster.
    let mut fat_head = Vec::new();
    for entry in [0x0fff_fff8u32, 0x0fff_ffff, 0x0fff_ffff] {
        fat_head.extend_from_slice(&entry.to_le_bytes());
    }
    for copy in 0..2 {
        put(reserved as u64 + copy * fat_sectors as u64, &fat_head);
    }
    // The file is sparse and zero-filled, so the root cluster is already empty.
}

/// FAT type and geometry read straight from a boot sector, independent of
/// fatfs: (is FAT32 layout, bytes per cluster, data clusters).
pub(crate) fn fat_geometry(boot: &[u8]) -> (bool, u32, u32) {
    let le16 = |o: usize| u16::from_le_bytes([boot[o], boot[o + 1]]) as u32;
    let le32 = |o: usize| u32::from_le_bytes(boot[o..o + 4].try_into().unwrap());
    let fat32 = le16(22) == 0 && le16(17) == 0 && &boot[82..90] == b"FAT32   ";
    let bps = le16(11);
    let spc = boot[13] as u32;
    let fat_size = if fat32 { le32(36) } else { le16(22) };
    let total = if le16(19) != 0 { le16(19) } else { le32(32) };
    let root_dir_sectors = (le16(17) * 32).div_ceil(bps);
    let data = total - le16(14) - boot[16] as u32 * fat_size - root_dir_sectors;
    (fat32, bps * spc, data / spc)
}
