//! What each selectable console needs from the flasher: which release
//! image it boots, whether it takes a panel overlay, and which
//! extlinux.conf it boots with.
//!
//! Game Console (R36S family): original/clone images, a mipi-panel.dtbo
//! from the web generator and a `variant` marker on the boot partition.
//! Soysauce boots the original image with extlinux.conf.soysauce.
//!
//! AISLPC (RK3562): one image for every model. Each screen has its own
//! device tree and its own extlinux.conf.<board> on the boot partition;
//! selecting the console only copies that file over extlinux.conf. No
//! overlay, no variant marker.

/// Image family of a release asset: the marker in its file name.
pub const FAMILY_ORIGINAL: &str = "original";
pub const FAMILY_CLONE: &str = "clone";
pub const FAMILY_AISLPC: &str = "aislpc";

/// AISLPC console ids, as the frontend sends them. The id is also the
/// suffix of the board's extlinux config (extlinux.conf.<id>).
pub const AISLPC_BOARDS: &[&str] = &["rg43v-pro", "rg43v-pro-1024x600", "rg52-mini"];

pub fn is_aislpc(console: &str) -> bool {
    AISLPC_BOARDS.contains(&console)
}

/// Release image family a console (or a family name) boots.
pub fn image_family(console: &str) -> Option<&'static str> {
    match console {
        "original" | "soysauce" => Some(FAMILY_ORIGINAL),
        "clone" => Some(FAMILY_CLONE),
        FAMILY_AISLPC => Some(FAMILY_AISLPC),
        c if is_aislpc(c) => Some(FAMILY_AISLPC),
        _ => None,
    }
}

/// Consoles the flash command accepts.
pub fn is_known_console(console: &str) -> bool {
    matches!(console, "original" | "clone" | "soysauce") || is_aislpc(console)
}

/// The R36S family keeps its `variant` marker on the boot partition.
pub fn writes_variant_file(console: &str) -> bool {
    !is_aislpc(console)
}

/// extlinux config to copy over extlinux.conf, and whether its absence is
/// an error. Soysauce tolerates older images that ship without the
/// alternate config; an AISLPC image without the board's config is the
/// wrong image and must not be flashed as if it were configured.
/// Soysauce also keeps the replaced config as extlinux.conf.bak. AISLPC
/// does not: every board config stays on the card, so switching back is
/// one copy, and nothing but extlinux.conf changes.
pub struct ExtlinuxSwitch {
    pub source: String,
    pub required: bool,
    pub backup: bool,
}

pub fn extlinux_switch(console: &str) -> Option<ExtlinuxSwitch> {
    if console == "soysauce" {
        return Some(ExtlinuxSwitch {
            source: "extlinux.conf.soysauce".into(),
            required: false,
            backup: true,
        });
    }
    if is_aislpc(console) {
        return Some(ExtlinuxSwitch {
            source: format!("extlinux.conf.{}", console),
            required: true,
            backup: false,
        });
    }
    None
}

/// Refuse a flash request the console cannot boot: an unknown console, or
/// a panel overlay for an AISLPC board (those take no overlay at all; the
/// screen comes from the board's device tree).
pub fn check_flash_request(console: &str, overlay_path: &str) -> Result<(), String> {
    if !is_known_console(console) {
        return Err(format!("Unknown console '{}'", console));
    }
    if is_aislpc(console) && !overlay_path.trim().is_empty() {
        return Err(format!("{} takes no panel overlay", console));
    }
    Ok(())
}

/// Pick the image asset of a family among release asset names.
/// Asset names seen so far:
///   v1: ArchR-R36S-YYYYMMDD.img.gz / ArchR-R36S-clone-YYYYMMDD.img.gz
///   v2: ArchR-R36S.aarch64-YYYYMMDD-original.img.gz / ...-clone.img.gz
///   AISLPC: ArchR-RK3562.aarch64-YYYYMMDD-aislpc.img.gz
pub fn pick_image_asset<'a, I>(names: I, family: &str) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    names.into_iter().find(|name| {
        let is_image = name.ends_with(".img.xz") || name.ends_with(".img.gz");
        if !is_image {
            return false;
        }
        let aislpc = name.contains("-aislpc");
        match family {
            FAMILY_AISLPC => aislpc,
            FAMILY_CLONE => name.contains("-clone") && !aislpc,
            FAMILY_ORIGINAL => name.starts_with("ArchR-R36S") && !name.contains("-clone") && !aislpc,
            _ => false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE_3_1_1: &[&str] = &[
        "ArchR-R36S.aarch64-20261201-original.img.gz",
        "ArchR-R36S.aarch64-20261201-original.img.gz.sha256",
        "ArchR-R36S.aarch64-20261201-clone.img.gz",
        "ArchR-R36S.aarch64-20261201-clone.img.gz.sha256",
        "ArchR-RK3562.aarch64-20261201-aislpc.img.gz",
        "ArchR-RK3562.aarch64-20261201-aislpc.img.gz.sha256",
    ];

    fn pick(names: &[&'static str], console: &str) -> Option<&'static str> {
        pick_image_asset(names.iter().copied(), image_family(console)?)
    }

    #[test]
    fn game_consoles_pick_original_or_clone() {
        assert_eq!(pick(RELEASE_3_1_1, "original"), Some("ArchR-R36S.aarch64-20261201-original.img.gz"));
        assert_eq!(pick(RELEASE_3_1_1, "soysauce"), Some("ArchR-R36S.aarch64-20261201-original.img.gz"));
        assert_eq!(pick(RELEASE_3_1_1, "clone"), Some("ArchR-R36S.aarch64-20261201-clone.img.gz"));
    }

    #[test]
    fn every_aislpc_board_picks_the_aislpc_image() {
        for board in AISLPC_BOARDS {
            assert_eq!(pick(RELEASE_3_1_1, board), Some("ArchR-RK3562.aarch64-20261201-aislpc.img.gz"), "{board}");
        }
        assert_eq!(pick(RELEASE_3_1_1, "aislpc"), Some("ArchR-RK3562.aarch64-20261201-aislpc.img.gz"));
    }

    #[test]
    fn aislpc_image_never_stands_in_for_a_game_console() {
        // Asset order must not matter: the AISLPC image listed first.
        let names = ["ArchR-RK3562.aarch64-20261201-aislpc.img.gz", "ArchR-R36S.aarch64-20261201-clone.img.gz"];
        assert_eq!(pick(&names, "original"), None);
        assert_eq!(pick(&names, "clone"), Some("ArchR-R36S.aarch64-20261201-clone.img.gz"));
    }

    #[test]
    fn release_without_aislpc_has_no_image_for_aislpc() {
        // v3.1.0 and earlier: only the R36S images.
        let names = &RELEASE_3_1_1[..4];
        for board in AISLPC_BOARDS {
            assert_eq!(pick(names, board), None);
        }
    }

    #[test]
    fn v1_names_still_match() {
        let names = ["ArchR-R36S-20250101.img.gz", "ArchR-R36S-clone-20250101.img.gz"];
        assert_eq!(pick(&names, "original"), Some("ArchR-R36S-20250101.img.gz"));
        assert_eq!(pick(&names, "clone"), Some("ArchR-R36S-clone-20250101.img.gz"));
    }

    #[test]
    fn unknown_console_has_no_family() {
        assert_eq!(image_family("rg35xx"), None);
        assert!(!is_known_console("rg35xx"));
        assert!(!is_known_console("aislpc"), "the family is not a console");
    }

    #[test]
    fn extlinux_switch_per_console() {
        assert!(extlinux_switch("original").is_none());
        assert!(extlinux_switch("clone").is_none());
        let s = extlinux_switch("soysauce").unwrap();
        assert_eq!((s.source.as_str(), s.required, s.backup), ("extlinux.conf.soysauce", false, true));
        let s = extlinux_switch("rg52-mini").unwrap();
        assert_eq!((s.source.as_str(), s.required, s.backup), ("extlinux.conf.rg52-mini", true, false));
        let s = extlinux_switch("rg43v-pro-1024x600").unwrap();
        assert_eq!(s.source, "extlinux.conf.rg43v-pro-1024x600");
    }

    #[test]
    fn aislpc_flash_refuses_an_overlay() {
        assert!(check_flash_request("rg52-mini", "").is_ok());
        assert!(check_flash_request("rg52-mini", "  ").is_ok());
        assert!(check_flash_request("rg52-mini", "/home/u/mipi-panel.dtbo").is_err());
        assert!(check_flash_request("original", "/home/u/mipi-panel.dtbo").is_ok());
        assert!(check_flash_request("aislpc", "").is_err());
    }

    #[test]
    fn only_game_consoles_write_the_variant_marker() {
        assert!(writes_variant_file("original"));
        assert!(writes_variant_file("soysauce"));
        for board in AISLPC_BOARDS {
            assert!(!writes_variant_file(board));
        }
    }
}
