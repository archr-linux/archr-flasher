// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) 2026 ArchR

use crate::preflight::PreparedImage;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::Path;

struct HashingReader<'a> {
    source: File,
    hash: &'a mut Sha256,
}

impl Read for HashingReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let count = self.source.read(buf)?;
        self.hash.update(&buf[..count]);
        Ok(count)
    }
}

/// A decompressed image is private to this operation, never reused on retry.
/// Only return it after decoder EOF (including gzip CRC/XZ checks), optional
/// archive SHA-256 and MBR validation. RAII removes partial output on any error.
pub(crate) fn prepare(source: File, path: &Path, cache: &Path) -> Result<PreparedImage, String> {
    fs::create_dir_all(cache).map_err(|e| format!("Cannot create image cache: {}", e))?;
    // Older versions left this fixed-name file on CRC/decompression failure.
    // It has no integrity proof and must never be treated as a valid cache.
    match fs::remove_file(cache.join("archr-flash-temp.img")) {
        Ok(()) => {},
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        Err(e) => return Err(format!("Cannot remove unvalidated legacy cache: {}", e)),
    }
    let checksum = Path::new(&format!("{}.sha256", path.display())).to_path_buf();
    let expected = match fs::read_to_string(&checksum) {
        Ok(text) => {
            let hash = text.split_whitespace().next().unwrap_or("");
            if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err("Invalid image SHA-256 sidecar".into());
            }
            Some(hash.to_ascii_lowercase())
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("Cannot read image SHA-256: {}", e)),
    };
    let mut output = tempfile::Builder::new().prefix("archr-image-").suffix(".img").tempfile_in(cache)
        .map_err(|e| format!("Cannot create private image: {}", e))?;
    let mut hash = Sha256::new();
    {
        let reader = BufReader::new(HashingReader { source, hash: &mut hash });
        let mut decoder: Box<dyn Read + '_> = if path.to_string_lossy().ends_with(".xz") {
            Box::new(xz2::read::XzDecoder::new_multi_decoder(reader))
        } else {
            Box::new(flate2::read::MultiGzDecoder::new(reader))
        };
        std::io::copy(&mut decoder, output.as_file_mut()).map_err(|e| format!("Decompress error: {}", e))?;
    }
    if let Some(expected) = expected
        && format!("{:x}", hash.finalize()) != expected {
        return Err("Image SHA-256 mismatch before decompressed cache promotion".into());
    }
    output.flush().map_err(|e| format!("Cannot flush private image: {}", e))?;
    // Promotion is ownership transfer of the validated file, not a shared
    // filename: no other attempt can accidentally reuse incomplete output.
    PreparedImage::from_temp(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(dir: &Path) -> (std::path::PathBuf, Vec<u8>) {
        let image = dir.join("source.img");
        crate::test_support::valid_image(&image, 16 * 1024 * 1024);
        let bytes = fs::read(&image).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&bytes).unwrap();
        let compressed = encoder.finish().unwrap();
        let path = dir.join("source.img.gz");
        fs::write(&path, &compressed).unwrap();
        (path, compressed)
    }

    #[test]
    fn invalid_gzip_crc_never_becomes_reusable_on_retry() {
        let dir = tempfile::tempdir().unwrap();
        let (source, mut bytes) = archive(dir.path());
        let cache = dir.path().join("cache");
        let crc_at = bytes.len() - 8;
        bytes[crc_at] ^= 1;
        fs::write(&source, bytes).unwrap();
        for _ in 0..2 {
            assert!(prepare(File::open(&source).unwrap(), &source, &cache).is_err());
            assert_eq!(fs::read_dir(&cache).unwrap().count(), 0);
        }
    }

    #[test]
    fn wrong_sha256_does_not_promote_or_leave_partial_output() {
        let dir = tempfile::tempdir().unwrap();
        let (source, _) = archive(dir.path());
        let cache = dir.path().join("cache");
        fs::write(format!("{}.sha256", source.display()), "0".repeat(64)).unwrap();
        assert!(prepare(File::open(&source).unwrap(), &source, &cache).is_err());
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 0);
    }

    #[test]
    fn valid_archive_and_sha_are_promoted_privately_and_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let (source, compressed) = archive(dir.path());
        let cache = dir.path().join("cache");
        fs::create_dir(&cache).unwrap();
        fs::write(cache.join("archr-flash-temp.img"), b"untrusted old cache").unwrap();
        fs::write(format!("{}.sha256", source.display()), format!("{:x}  source.img.gz\n", Sha256::digest(&compressed))).unwrap();
        let prepared = prepare(File::open(&source).unwrap(), &source, &cache).unwrap();
        assert_eq!(prepared.size, 16 * 1024 * 1024);
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 1);
        drop(prepared);
        assert_eq!(fs::read_dir(cache).unwrap().count(), 0);
    }
}
