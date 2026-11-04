// Exercise the shipped Bash orchestration against regular files. All device,
// service, mount and delay commands are functions scoped to the child shell.
use super::FLASH_SCRIPT;
use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn flash(dir: &Path, verify: bool) -> Output {
    let image = File::open(dir.join("source.img")).ok();
    flash_with(dir, verify, image)
}

fn flash_with(dir: &Path, verify: bool, image: Option<File>) -> Output {
    flash_in_locale(dir, verify, image, "C")
}

/// Run the script as pkexec would for a user with this LANG: no LC_ALL or
/// LC_MESSAGES override, so localized tools speak the user's language.
fn flash_in_locale(dir: &Path, verify: bool, image: Option<File>, lang: &str) -> Output {
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
    # The message follows the locale, as util-linux and glibc strerror do
    # (LC_ALL, then LC_MESSAGES, then LANG). pkexec keeps the user's locale.
    if [[ "$1" == --rereadpt && -f "$TEST_DIR/held" ]]; then
        case "${LC_ALL:-${LC_MESSAGES:-$LANG}}" in
            pt_BR*) echo "blockdev: erro de ioctl em BLKRRPART: Dispositivo ou recurso está ocupado" >&2 ;;
            es*) echo "blockdev: error de ioctl en BLKRRPART: Dispositivo o recurso ocupado" >&2 ;;
            ru*) echo "blockdev: ошибка ioctl на BLKRRPART: Устройство или ресурс занято" >&2 ;;
            zh*) echo "blockdev: BLKRRPART 上的 ioctl 出错: 设备或资源忙" >&2 ;;
            de*) echo "blockdev: ioctl-Fehler bei BLKRRPART: Das Gerät oder die Ressource ist belegt" >&2 ;;
            *) echo "blockdev: ioctl error on BLKRRPART: Device or resource busy" >&2 ;;
        esac
        return 1
    fi
    return 0
}
udisksctl() { probe udisksctl; return 0; }
partprobe() { probe partprobe; return 0; }
sleep() { return 0; }
sync() { probe sync; return 0; }
eject() { return 0; }
# The script must never mount anything: record the attempt and fail.
mount() { probe mount; printf 'mount\n' >> "$TEST_DIR/events"; return 32; }
# A partition counts as mounted while $TEST_DIR/mounted-<name> exists. A
# busy- marker makes a plain umount fail with EBUSY, as when a file manager
# holds the volume. Lazy (-l) and forced (-f) unmounts "succeed" and are
# logged: that is the detach that left a live vfat superblock behind.
findmnt() { [[ -f "$TEST_DIR/mounted-${!###*/}" ]]; }
umount() {
    local target="${!#}" name="${!###*/}" opt
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
    cmp /proc/$$/fd/0 "$2"
    printf 'verify\n' >> "$TEST_DIR/events"
fi
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
        .arg(dir.join("progress"))
        .arg(helper)
        .arg(if verify { "1" } else { "0" })
        .env("BASH_ENV", mocks)
        .env("TEST_DIR", dir)
        .env("TEST_IMAGE_ID", &id)
        .stdin(stdin)
        .env("TMPDIR", dir)
        .env_remove("LC_ALL")
        .env_remove("LC_MESSAGES")
        .env("LANG", lang)
        .output()
        .unwrap()
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("source.img"), b"image data").unwrap();
    fs::write(dir.path().join("disk"), b"untouched target").unwrap();
    fs::write(dir.path().join("disk1"), b"fake boot partition").unwrap();
    dir
}

#[test]
fn linux_writes_verifies_and_finishes_without_mounting() {
    let dir = fixture();
    let output = flash(dir.path(), true);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(fs::read(dir.path().join("disk")).unwrap(), b"image data");
    // No "mount" event: the card is never mounted after the write.
    assert_eq!(fs::read_to_string(dir.path().join("events")).unwrap(), "write\nverify\n");
    assert_eq!(fs::read_to_string(dir.path().join("progress")).unwrap().trim(), "STAGE:done");
}

#[test]
fn linux_missing_image_never_calls_writer() {
    let dir = fixture();
    fs::remove_file(dir.path().join("source.img")).unwrap();
    let output = flash(dir.path(), true);
    assert!(!output.status.success());
    assert_eq!(fs::read(dir.path().join("disk")).unwrap(), b"untouched target");
    assert!(!dir.path().join("events").exists());
}

#[test]
fn linux_also_succeeds_without_verify() {
    let dir = fixture();
    let output = flash(dir.path(), false);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(fs::read_to_string(dir.path().join("events")).unwrap(), "write\n");
}

#[test]
fn linux_image_descriptor_reaches_only_the_helper() {
    // Probes replace date, systemctl (also run by the EXIT trap), udisksctl,
    // partprobe, mount and sync, before and after the write. None of them
    // may read the image through the fd the script holds for the helper.
    let dir = fixture();
    let output = flash(dir.path(), true);
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
    let output = flash_with(dir.path(), true, Some(stdin));
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
    let output = flash(dir.path(), true);
    assert_aborted_before_writing(dir.path(), &output);
    assert!(dir.path().join("mounted-disk1").exists());
}

#[test]
fn linux_partition_held_without_a_mountpoint_aborts_before_writing() {
    // A superblock detached earlier is invisible to findmnt but still holds
    // the partition. BLKRRPART reports it as EBUSY.
    let dir = fixture();
    fs::write(dir.path().join("held"), b"").unwrap();
    let output = flash(dir.path(), true);
    assert_aborted_before_writing(dir.path(), &output);
}

#[test]
fn linux_idle_mounted_partition_is_unmounted_and_written() {
    let dir = fixture();
    fs::write(dir.path().join("mounted-disk1"), b"").unwrap();
    let output = flash(dir.path(), true);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(!dir.path().join("mounted-disk1").exists());
    assert_eq!(fs::read(dir.path().join("disk")).unwrap(), b"image data");
}

// ---------------------------------------------------------------------------
// The boot configuration is patched into a private image before the write.
// A real FAT32 image (16 KiB clusters, below the FAT32 cluster minimum, as
// shipped) goes through stage_boot_config, the shipped script and the real
// helper, into regular files standing in for the card.
// ---------------------------------------------------------------------------
use crate::preflight::PreparedImage;
use crate::test_support::{archr_boot_image, boot_file, pattern, ARCHR_BOOT_START, ARCHR_DTBS};

/// Independent reader: mtools, when installed, on the same byte offset.
fn mtools_file(card: &Path, name: &str) -> Option<Vec<u8>> {
    let out = Command::new("mtype")
        .arg("-i")
        .arg(format!("{}@@{}", card.display(), ARCHR_BOOT_START))
        .arg(format!("::/{}", name))
        .env("MTOOLS_SKIP_CHECK", "1")
        .output()
        .ok()?;
    assert!(out.status.success(), "mtype {}: {}", name, String::from_utf8_lossy(&out.stderr));
    Some(out.stdout)
}

/// fsck.fat -n on the boot FAT of an image, when dosfstools is installed.
/// Returns its report; panics on any error it finds.
fn fsck_boot(image: &Path) -> Option<String> {
    let bytes = fs::read(image).unwrap();
    let boot = &bytes[ARCHR_BOOT_START as usize..][..crate::test_support::ARCHR_BOOT_SIZE as usize];
    let dir = tempfile::tempdir().unwrap();
    let slice = dir.path().join("boot.fat");
    fs::write(&slice, boot).unwrap();
    let out = Command::new("fsck.fat").arg("-n").arg("-v").arg(&slice).output().ok()?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "fsck.fat found errors: {}", text);
    // Only the expected below-minimum warning may follow the summary.
    let summary = text.find(" files, ").expect("fsck.fat summary");
    let before = &text[..summary];
    for marker in ["share clusters", "Bad short file name", "Expected a valid", "Deleting", "Truncating", "should be", "uninitialized"] {
        assert!(!before.contains(marker), "fsck.fat: {} in {}", marker, text);
    }
    Some(text)
}

fn assert_card_configured(card: &Path, source: &Path, dtbo: &[u8]) {
    fsck_boot(card);
    // Geometry of the written boot FAT is the image's: 16 KiB clusters.
    let bytes = fs::read(card).unwrap();
    let bpb = &bytes[ARCHR_BOOT_START as usize..][..512];
    let (fat32, cluster, clusters) = crate::test_support::fat_geometry(bpb);
    assert!(fat32, "boot FAT is not FAT32");
    assert_eq!(cluster, 16 * 1024, "boot FAT cluster size");
    assert!(clusters < 65_525, "boot FAT must stay below the FAT32 cluster minimum: {}", clusters);
    let mut expected: Vec<(String, Vec<u8>)> = vec![
        ("overlays/mipi-panel.dtbo".into(), dtbo.to_vec()),
        ("variant".into(), b"soysauce".to_vec()),
        ("extlinux/extlinux.conf".into(), b"soysauce config".to_vec()),
        ("extlinux/extlinux.conf.bak".into(), b"default config".to_vec()),
        ("KERNEL".into(), boot_file(source, "KERNEL")),
    ];
    for name in ARCHR_DTBS {
        let path = format!("dtbs/{}", name);
        let original = boot_file(source, &path);
        expected.push((path, original));
    }
    for (name, want) in &expected {
        assert!(boot_file(card, name) == *want, "fatfs: {} differs on the card", name);
        if let Some(got) = mtools_file(card, name) {
            assert!(got == *want, "mtools: {} differs on the card", name);
        }
    }
}

#[test]
fn linux_flashed_card_has_overlay_variant_and_intact_dtbs() {
    let dir = fixture();
    let source = dir.path().join("archr.img");
    archr_boot_image(&source);
    let original = fs::read(&source).unwrap();
    let dtbo_bytes = pattern(7, 3_000);
    let dtbo = dir.path().join("staged.dtbo");
    fs::write(&dtbo, &dtbo_bytes).unwrap();
    let work = dir.path().join("cache");

    let prepared = PreparedImage::from_file(File::open(&source).unwrap(), source.clone()).unwrap();
    let staged = super::stage_boot_config(prepared, &dtbo, "soysauce", &work).unwrap();
    assert!(staged.path.starts_with(&work), "patch copy is not private: {}", staged.path.display());
    assert!(fs::read(&source).unwrap() == original, "the selected image was modified");

    // Shipped script: the descriptor of the patched image reaches the helper,
    // which writes it to the file-backed "disk". No mount happens.
    let stdin = crate::flashwrite::read_only_descriptor(&staged.file).unwrap();
    let output = flash_with(dir.path(), true, Some(stdin));
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(fs::read_to_string(dir.path().join("events")).unwrap(), "write\nverify\n");
    assert_card_configured(&dir.path().join("disk"), &source, &dtbo_bytes);

    // Real helper with verify on: it hashes the patched image, so the
    // comparison covers the overlay and variant bytes too.
    let card = dir.path().join("card.img");
    File::create(&card).unwrap().set_len(staged.size).unwrap();
    let id = crate::flashwrite::image_identity(&staged.file).unwrap();
    let args = [id, card.to_string_lossy().into_owned(), dir.path().join("progress2").to_string_lossy().into_owned()];
    let read_only = crate::flashwrite::read_only_descriptor(&staged.file).unwrap();
    assert_eq!(crate::flashwrite::entry_with_image(read_only, &args), 0);
    assert_card_configured(&card, &source, &dtbo_bytes);

    let temp = staged.path.clone();
    drop(staged);
    assert!(!temp.exists(), "patch copy left behind");
}

#[test]
fn linux_decompressed_image_is_patched_in_place_and_revalidated() {
    let dir = tempfile::tempdir().unwrap();
    let mut private = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
    archr_boot_image(private.path());
    std::io::Seek::rewind(private.as_file_mut()).unwrap();
    let prepared = PreparedImage::from_temp(private).unwrap();
    let path = prepared.path.clone();
    let staged = super::stage_boot_config(prepared, Path::new(""), "clone", &dir.path().join("cache")).unwrap();
    assert_eq!(staged.path, path, "an owned temporary needs no second copy");
    assert_eq!(boot_file(&path, "variant"), b"clone");
    assert_eq!(boot_file(&path, "overlays/mipi-panel.dtbo"), b"original panel");
    assert!(!dir.path().join("cache").exists());
}

#[test]
fn linux_missing_overlay_fails_before_copying_or_writing() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("archr.img");
    archr_boot_image(&source);
    let original = fs::read(&source).unwrap();
    let work = dir.path().join("cache");
    let prepared = PreparedImage::from_file(File::open(&source).unwrap(), source.clone()).unwrap();
    let err = super::stage_boot_config(prepared, &dir.path().join("missing.dtbo"), "original", &work)
        .err()
        .unwrap();
    assert!(err.contains("dtbo"), "{}", err);
    assert!(fs::read(&source).unwrap() == original);
    assert!(!work.exists() || fs::read_dir(&work).unwrap().count() == 0);
}

#[test]
fn linux_held_partition_aborts_in_every_user_locale() {
    // BLKRRPART EBUSY must abort whatever language blockdev answers in.
    for lang in ["C", "en_US.UTF-8", "pt_BR.UTF-8", "es_ES.UTF-8", "ru_RU.UTF-8", "zh_CN.UTF-8", "de_DE.UTF-8"] {
        let dir = fixture();
        fs::write(dir.path().join("held"), b"").unwrap();
        let image = File::open(dir.path().join("source.img")).ok();
        let output = flash_in_locale(dir.path(), true, image, lang);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "LANG={}: flashed a held card: {}", lang, stderr);
        assert!(stderr.contains("card in use:"), "LANG={}: {}", lang, stderr);
        assert_eq!(fs::read(dir.path().join("disk")).unwrap(), b"untouched target", "LANG={}", lang);
        assert!(!dir.path().join("events").exists(), "LANG={}: the writer ran", lang);
    }
}

#[test]
fn archr_fixture_is_fat32_with_the_shipped_geometry() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("archr.img");
    archr_boot_image(&image);
    let bytes = fs::read(&image).unwrap();
    let boot = &bytes[ARCHR_BOOT_START as usize..][..crate::test_support::ARCHR_BOOT_SIZE as usize];
    let (fat32, cluster, clusters) = crate::test_support::fat_geometry(&boot[..512]);
    assert!(fat32, "fixture boot FAT is not FAT32");
    assert_eq!(boot[13], crate::test_support::ARCHR_SECTORS_PER_CLUSTER);
    assert_eq!(cluster, 16 * 1024);
    assert!(clusters < 65_525, "{} clusters", clusters);
    // Independent checker, when installed: dosfstools.
    if let Some(report) = fsck_boot(&image) {
        assert!(report.contains("32 bit entries"), "fsck.fat does not see FAT32: {}", report);
        assert!(report.contains("16384 bytes per cluster"), "{}", report);
    }
    if let Some(kernel) = mtools_file(&image, "KERNEL") {
        assert!(kernel == pattern(1, 300_000));
    }
}
