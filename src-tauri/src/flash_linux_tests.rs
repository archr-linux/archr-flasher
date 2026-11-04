// Exercise the shipped Bash orchestration against regular files. All device,
// service, mount and delay commands are functions scoped to the child shell.
use super::FLASH_SCRIPT;
use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn flash(dir: &Path, overlay: &str, verify: bool) -> Output {
    let image = File::open(dir.join("source.img")).ok();
    flash_with(dir, overlay, verify, image)
}

fn flash_with(dir: &Path, overlay: &str, verify: bool, image: Option<File>) -> Output {
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
# Probe: a separate process that tries to read the image through fd 3
# (via /proc/self/fd/3, its own offset, so the helper's stream is not
# disturbed) and to write through the inherited fd 3 itself. Any success
# is recorded in $TEST_DIR/leaks.
probe() {
    if /bin/bash -c 'head -c1 /proc/self/fd/3 >/dev/null 2>&1'; then
        printf '%s:read\n' "$1" >> "$TEST_DIR/leaks"
    fi
    if /bin/bash -c 'printf Z >&3' 2>/dev/null; then
        printf '%s:write\n' "$1" >> "$TEST_DIR/leaks"
    fi
}
date() { probe date; command date "$@"; }
systemctl() { probe systemctl; return 1; }
blockdev() {
    if [[ "$1" == --getsize64 ]]; then stat -c%s "$2"; fi
    # A superblock still holding a partition (lazily detached or not)
    # makes BLKRRPART fail with EBUSY.
    if [[ "$1" == --rereadpt && -f "$TEST_DIR/held" ]]; then
        echo "blockdev: ioctl error on BLKRRPART: Device or resource busy" >&2
        return 1
    fi
    return 0
}
udisksctl() { probe udisksctl; return 0; }
partprobe() { probe partprobe; return 0; }
sleep() { return 0; }
sync() { probe sync; return 0; }
eject() { return 0; }
mount() {
    probe mount
    cp -a "$TEST_DIR/boot/." "$2/"
    printf '%s' "$2" > "$TEST_DIR/mount-path"
}
# A partition counts as mounted while $TEST_DIR/mounted-<name> exists. A
# busy- marker makes a plain umount fail with EBUSY, as when a file manager
# holds the volume. Lazy (-l) and forced (-f) unmounts "succeed" and are
# logged: that is the detach that left a live vfat superblock behind.
findmnt() { [[ -f "$TEST_DIR/mounted-${!###*/}" ]]; }
umount() {
    local target="${!#}" name="${!###*/}" opt
    if [[ -f "$TEST_DIR/mount-path" && "$target" == "$(cat "$TEST_DIR/mount-path")" ]]; then
        cp -a "$target/." "$TEST_DIR/boot/"
        rm -rf -- "$target/overlays" "$target/variant"
        return 0
    fi
    [[ -f "$TEST_DIR/mounted-$name" ]] || return 32
    for opt in "$@"; do
        if [[ "$opt" == -l || "$opt" == -f ]]; then
            printf '%s %s\n' "$opt" "$name" >> "$TEST_DIR/umount-log"
            rm -f "$TEST_DIR/mounted-$name"
            return 0
        fi
    done
    if [[ -f "$TEST_DIR/busy-$name" ]]; then
        echo "umount: $target: target is busy." >&2
        return 32
    fi
    rm -f "$TEST_DIR/mounted-$name"
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
# The image is stdin; $1 is only its identity. fd 3 must be closed here too.
[[ "$1" == "$TEST_IMAGE_ID" ]]
if [[ -e /proc/$$/fd/3 ]]; then printf 'helper:fd3\n' >> "$TEST_DIR/leaks"; fi
# Its stdin must be read-only (access mode bits of fdinfo flags are 0).
flags=$(awk '/^flags:/ {print $2}' /proc/$$/fdinfo/0)
if (( (8#$flags & 3) != 0 )); then printf 'helper:stdin-writable\n' >> "$TEST_DIR/leaks"; fi
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
    let (stdin, id) = match image {
        Some(file) => {
            let id = crate::flashwrite::image_identity(&file).unwrap();
            (Stdio::from(file), id)
        }
        None => (Stdio::null(), "missing".to_string()),
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

#[test]
fn linux_image_descriptor_reaches_only_the_helper() {
    // Probes replace date, systemctl (also run by the EXIT trap), udisksctl,
    // partprobe, mount and sync, before and after the write. None of them
    // may read the image through the fd the script holds for the helper.
    let dir = fixture();
    let output = flash(dir.path(), "", true);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(fs::read(dir.path().join("disk")).unwrap(), b"image data");
    let leaks = fs::read_to_string(dir.path().join("leaks")).unwrap_or_default();
    assert!(leaks.is_empty(), "image fd leaked to: {}", leaks.replace('\n', " "));
}

#[test]
fn linux_cached_image_reaches_helper_read_only_and_nowhere_else() {
    // A decompressed image is a NamedTempFile opened O_RDWR. What reaches
    // pkexec must be read-only, and still only the helper may see it.
    let dir = fixture();
    let mut cached = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
    std::io::Write::write_all(cached.as_file_mut(), b"image data").unwrap();
    std::io::Seek::rewind(cached.as_file_mut()).unwrap();
    let stdin = crate::flashwrite::read_only_descriptor(cached.as_file()).unwrap();
    let output = flash_with(dir.path(), "", true, Some(stdin));
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(fs::read(cached.path()).unwrap(), b"image data", "image was modified");
    assert_eq!(fs::read(dir.path().join("disk")).unwrap(), b"image data");
    let leaks = fs::read_to_string(dir.path().join("leaks")).unwrap_or_default();
    assert!(leaks.is_empty(), "image fd leaked to: {}", leaks.replace('\n', " "));
}

fn assert_aborted_before_writing(dir: &Path, output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "flash must abort: {}", stderr);
    assert!(stderr.contains("card in use:"), "missing busy marker: {}", stderr);
    assert_eq!(fs::read(dir.join("disk")).unwrap(), b"untouched target");
    assert!(!dir.join("events").exists(), "the writer ran");
    let log = fs::read_to_string(dir.join("umount-log")).unwrap_or_default();
    assert!(log.is_empty(), "lazy or forced unmount used: {}", log);
}

#[test]
fn linux_busy_partition_aborts_before_writing() {
    // A file manager holds the old BOOT partition: plain umount gets EBUSY.
    // Detaching it (umount -l) leaves its superblock alive, and the later
    // mount reused it with the old FAT geometry. Nothing may be written.
    let dir = fixture();
    fs::write(dir.path().join("mounted-disk1"), b"").unwrap();
    fs::write(dir.path().join("busy-disk1"), b"").unwrap();
    let output = flash(dir.path(), "", true);
    assert_aborted_before_writing(dir.path(), &output);
    assert!(dir.path().join("mounted-disk1").exists());
}

#[test]
fn linux_partition_held_without_a_mountpoint_aborts_before_writing() {
    // A superblock detached earlier is invisible to findmnt but still holds
    // the partition. BLKRRPART reports it as EBUSY.
    let dir = fixture();
    fs::write(dir.path().join("held"), b"").unwrap();
    let output = flash(dir.path(), "", true);
    assert_aborted_before_writing(dir.path(), &output);
}

#[test]
fn linux_idle_mounted_partition_is_unmounted_and_written() {
    let dir = fixture();
    fs::write(dir.path().join("mounted-disk1"), b"").unwrap();
    let output = flash(dir.path(), "", true);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(!dir.path().join("mounted-disk1").exists());
    assert_eq!(fs::read(dir.path().join("disk")).unwrap(), b"image data");
}
