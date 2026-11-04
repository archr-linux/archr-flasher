// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) 2026 ArchR

use std::fs::File;
use std::path::Path;

/// Check every selected source before decompression, elevation or device I/O.
/// An empty overlay means keep the panel configuration shipped in the image.
/// Keep this shared by all OS entry points, not only the Tauri command.
pub(crate) fn validate_flash_inputs(image: &str, overlay: &str) -> Result<(), String> {
    readable_file(Path::new(image), "Image")?;
    let overlay = overlay.trim();
    if !overlay.is_empty() {
        let path = Path::new(overlay);
        if !path.is_absolute() {
            return Err(format!("Overlay path must be absolute: {}", overlay));
        }
        readable_file(path, "Overlay")?;
    }
    Ok(())
}

/// Hold a private snapshot until post-write configuration has finished.
/// Removing or editing the selected file during a long flash cannot turn
/// a successful write into a late "overlay missing" error.
pub(crate) fn stage_overlay(overlay: &str) -> Result<Option<tempfile::NamedTempFile>, String> {
    let overlay = overlay.trim();
    if overlay.is_empty() {
        return Ok(None);
    }
    let mut source =
        File::open(overlay).map_err(|e| format!("Cannot open selected overlay: {}", e))?;
    let mut staged =
        tempfile::NamedTempFile::new().map_err(|e| format!("Cannot stage overlay: {}", e))?;
    std::io::copy(&mut source, staged.as_file_mut())
        .map_err(|e| format!("Cannot stage overlay: {}", e))?;
    Ok(Some(staged))
}

fn readable_file(path: &Path, label: &str) -> Result<(), String> {
    // Reject directories and special files before open (opening a FIFO could block).
    if !path.is_file() {
        return Err(format!(
            "{} path is not a regular file: {}",
            label,
            path.display()
        ));
    }
    let file = File::open(path).map_err(|e| {
        format!(
            "{} file cannot be opened at {}: {}",
            label,
            path.display(),
            e
        )
    })?;
    let metadata = file
        .metadata()
        .map_err(|e| format!("Cannot read {} metadata: {}", label, e))?;
    if !metadata.is_file() {
        return Err(format!(
            "{} path is not a regular file: {}",
            label,
            path.display()
        ));
    }
    if label == "Image" && metadata.len() == 0 {
        return Err("Image file is empty".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_flash_inputs;
    use std::fs;

    #[test]
    fn staged_overlay_survives_removal_of_the_selected_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("panel.dtbo");
        fs::write(&source, b"selected overlay").unwrap();
        let staged = super::stage_overlay(source.to_str().unwrap())
            .unwrap()
            .unwrap();
        fs::remove_file(source).unwrap();
        assert_eq!(fs::read(staged.path()).unwrap(), b"selected overlay");
        let path = staged.path().to_path_buf();
        drop(staged);
        assert!(!path.exists());
        assert!(super::stage_overlay("").unwrap().is_none());
    }

    #[test]
    fn missing_overlay_cannot_be_staged() {
        let dir = tempfile::tempdir().unwrap();
        assert!(super::stage_overlay(dir.path().join("missing.dtbo").to_str().unwrap()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_selected_overlay_fails_before_writing() {
        use std::os::unix::fs::PermissionsExt;
        // Root may read mode-000 files; this case applies to unprivileged GUI users.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("image.img");
        let overlay = dir.path().join("panel.dtbo");
        fs::write(&image, b"image").unwrap();
        fs::write(&overlay, b"overlay").unwrap();
        fs::set_permissions(&overlay, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(validate_flash_inputs(image.to_str().unwrap(), overlay.to_str().unwrap()).is_err());
        assert!(super::stage_overlay(overlay.to_str().unwrap()).is_err());
    }

    #[test]
    fn accepts_image_without_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("image.img");
        fs::write(&image, b"image").unwrap();
        assert!(validate_flash_inputs(image.to_str().unwrap(), "").is_ok());
        assert!(validate_flash_inputs(image.to_str().unwrap(), " \t ").is_ok());
    }

    #[test]
    fn accepts_selected_readable_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("image.img");
        let overlay = dir.path().join("panel.dtbo");
        fs::write(&image, b"image").unwrap();
        fs::write(&overlay, b"overlay").unwrap();
        assert!(validate_flash_inputs(image.to_str().unwrap(), overlay.to_str().unwrap()).is_ok());
    }

    #[test]
    fn missing_selected_overlay_fails_before_target_can_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("image.img");
        let target = dir.path().join("target.img");
        fs::write(&image, b"new image").unwrap();
        fs::write(&target, b"untouched target").unwrap();
        let write = || -> Result<(), String> {
            validate_flash_inputs(
                image.to_str().unwrap(),
                dir.path().join("missing.dtbo").to_str().unwrap(),
            )?;
            fs::copy(&image, &target).map_err(|e| e.to_string())?;
            Ok(())
        };
        assert!(write().unwrap_err().contains("Overlay"));
        assert_eq!(fs::read(&target).unwrap(), b"untouched target");
    }

    #[test]
    fn rejects_missing_image_even_without_overlay() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            validate_flash_inputs(dir.path().join("missing.img").to_str().unwrap(), "").is_err()
        );
    }

    #[test]
    fn rejects_directories_relative_overlay_and_empty_image() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("image.img");
        fs::write(&image, b"image").unwrap();
        assert!(validate_flash_inputs(dir.path().to_str().unwrap(), "").is_err());
        assert!(
            validate_flash_inputs(image.to_str().unwrap(), dir.path().to_str().unwrap()).is_err()
        );
        assert!(validate_flash_inputs(image.to_str().unwrap(), "relative.dtbo").is_err());
        fs::write(&image, b"").unwrap();
        assert!(validate_flash_inputs(image.to_str().unwrap(), "").is_err());
    }
}
