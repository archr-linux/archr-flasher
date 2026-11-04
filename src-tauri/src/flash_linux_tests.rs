// Exercise the shipped Bash orchestration against regular files. All device,
// service, mount and delay commands are functions scoped to the child shell.
use super::FLASH_SCRIPT;
use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn flash(dir: &Path, overlay: &str, verify: bool) -> Output {
    let script = dir.join("flash.sh");
    fs::write(&script, FLASH_SCRIPT).unwrap();
    let mocks = dir.join("mocks.sh");
    fs::write(
        &mocks,
        r#"
[() {
    local -a args=("${@:1:$(($# - 1))}")
    local i
    for i in "${!args[@]}"; do
        if [[ "${args[$i]}" == -b ]]; then args[$i]=-f; fi
    done
    builtin test "${args[@]}"
}
systemctl() { return 1; }
blockdev() { if [[ "$1" == --getsize64 ]]; then stat -c%s "$2"; fi; }
udisksctl() { return 0; }
partprobe() { return 0; }
sleep() { return 0; }
sync() { return 0; }
eject() { return 0; }
mount() {
    cp -a "$TEST_DIR/boot/." "$2/"
    printf '%s' "$2" > "$TEST_DIR/mount-path"
}
umount() {
    if [[ -f "$TEST_DIR/mount-path" && "$1" == "$(cat "$TEST_DIR/mount-path")" ]]; then
        cp -a "$1/." "$TEST_DIR/boot/"
        rm -rf -- "$1/overlays" "$1/variant"
    fi
}
"#,
    )
    .unwrap();
    let helper = dir.join("helper.sh");
    fs::write(
        &helper,
        r#"#!/bin/bash
set -e
[[ "$1" == __flash-write ]]
shift
VERIFY=1
if [[ "$1" == --no-verify ]]; then VERIFY=0; shift; fi
# The image is stdin; $1 is only its identity.
[[ "$1" == "$TEST_IMAGE_ID" ]]
printf 'write\n' >> "$TEST_DIR/events"
cat > "$2"
if [[ "$VERIFY" == 1 ]]; then
    cmp "$TEST_DIR/source.img" "$2"
    printf 'verify\n' >> "$TEST_DIR/events"
fi
# Overlay must still be the default when write/verify completes.
cmp "$TEST_DIR/boot/overlays/mipi-panel.dtbo" "$TEST_DIR/default.dtbo"
"#,
    )
    .unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    let source = dir.join("source.img");
    let (stdin, id) = match File::open(&source) {
        Ok(file) => {
            let id = crate::flashwrite::image_identity(&file).unwrap();
            (Stdio::from(file), id)
        }
        Err(_) => (Stdio::null(), "missing".to_string()),
    };
    Command::new("bash")
        .arg(script)
        .arg(&id)
        .arg(dir.join("disk"))
        .arg(overlay)
        .arg("original")
        .arg(dir.join("progress"))
        .arg(helper)
        .arg(if verify { "1" } else { "0" })
        .env("BASH_ENV", mocks)
        .env("TEST_DIR", dir)
        .env("TEST_IMAGE_ID", &id)
        .stdin(stdin)
        .env("TMPDIR", dir)
        .output()
        .unwrap()
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("boot/overlays")).unwrap();
    fs::write(dir.path().join("boot/overlays/mipi-panel.dtbo"), b"default").unwrap();
    fs::write(dir.path().join("default.dtbo"), b"default").unwrap();
    fs::write(dir.path().join("source.img"), b"image data").unwrap();
    fs::write(dir.path().join("disk"), b"untouched target").unwrap();
    fs::write(dir.path().join("disk1"), b"fake boot partition").unwrap();
    dir
}

#[test]
fn linux_skip_overlay_writes_verifies_and_finishes_successfully() {
    let dir = fixture();
    let output = flash(dir.path(), "", true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(dir.path().join("disk")).unwrap(), b"image data");
    assert_eq!(
        fs::read(dir.path().join("boot/overlays/mipi-panel.dtbo")).unwrap(),
        b"default"
    );
    assert_eq!(
        fs::read(dir.path().join("boot/variant")).unwrap(),
        b"original"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("events")).unwrap(),
        "write\nverify\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("progress"))
            .unwrap()
            .trim(),
        "STAGE:done"
    );
}

#[test]
fn linux_selected_overlay_is_applied_after_verification() {
    let dir = fixture();
    let overlay = dir.path().join("custom.dtbo");
    fs::write(&overlay, b"custom").unwrap();
    let output = flash(dir.path(), overlay.to_str().unwrap(), true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(dir.path().join("boot/overlays/mipi-panel.dtbo")).unwrap(),
        b"custom"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("events")).unwrap(),
        "write\nverify\n"
    );
}

#[test]
fn linux_missing_selected_overlay_never_calls_writer() {
    let dir = fixture();
    let output = flash(
        dir.path(),
        dir.path().join("missing.dtbo").to_str().unwrap(),
        true,
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("DTBO"));
    assert_eq!(
        fs::read(dir.path().join("disk")).unwrap(),
        b"untouched target"
    );
    assert!(!dir.path().join("events").exists());
}

#[test]
fn linux_missing_image_never_calls_writer() {
    let dir = fixture();
    fs::remove_file(dir.path().join("source.img")).unwrap();
    let output = flash(dir.path(), "", true);
    assert!(!output.status.success());
    assert_eq!(
        fs::read(dir.path().join("disk")).unwrap(),
        b"untouched target"
    );
    assert!(!dir.path().join("events").exists());
}

#[test]
fn linux_skip_overlay_also_succeeds_without_verify() {
    let dir = fixture();
    let output = flash(dir.path(), "", false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("events")).unwrap(),
        "write\n"
    );
    assert_eq!(
        fs::read(dir.path().join("boot/overlays/mipi-panel.dtbo")).unwrap(),
        b"default"
    );
}
