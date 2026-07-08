# Vendored fatfs 0.3.6

Vendored copy of https://github.com/rafalh/rust-fatfs (MIT, see
LICENSE.txt) with one behavioral change, marked with `ArchR:` comments
in `src/boot_sector.rs` and `src/fs.rs`:

The ArchR boot partition is FAT32 formatted with a cluster count below
the spec minimum of 65525. The vendor U-Boots shipped on R36S boards
only boot from exactly this geometry, so the image cannot be
"corrected". Upstream fatfs (like the macOS msdos driver) refuses to
touch such a volume because the FAT type inferred from the cluster
count disagrees with the BPB layout. The patch makes the BPB layout
signal (sectors_per_fat_16 == 0 means FAT32) authoritative, which is
what the Linux kernel driver and mtools do in practice.

Used by the flasher to install the panel overlay and variant marker
inside the image before writing, so no OS-level mount of the quirky
FAT is ever needed.
