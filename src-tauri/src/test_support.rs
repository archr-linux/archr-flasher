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
