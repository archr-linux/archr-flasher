//! Panel customization: extract panel_description from DTBO, build custom DTBO
//! with rotation, stick inversion, and HP-detect inversion baked in.

use crate::dtbo_builder::FdtBuilder;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PanelConfig {
    pub rotation: u32,
    pub invert_left_stick: bool,
    pub invert_right_stick: bool,
    pub hp_invert: bool,
}

impl PanelConfig {
    #[allow(dead_code)]
    pub fn is_default(&self) -> bool {
        self.rotation == 0
            && !self.invert_left_stick
            && !self.invert_right_stick
            && !self.hp_invert
    }
}

/// Extract the raw `panel_description` property bytes from a panel DTBO.
/// Walks the FDT structure looking for a property named "panel_description".
pub fn extract_panel_description(dtbo_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let fdt = fdt::Fdt::new(dtbo_bytes)
        .map_err(|e| format!("Invalid DTBO: {:?}", e))?;

    // Walk all nodes looking for panel_description
    fn find_prop<'a>(node: fdt::node::FdtNode<'a, 'a>) -> Option<&'a [u8]> {
        if let Some(prop) = node.property("panel_description") {
            return Some(prop.value);
        }
        for child in node.children() {
            if let Some(val) = find_prop(child) {
                return Some(val);
            }
        }
        None
    }

    let root = fdt.find_node("/")
        .ok_or("DTBO has no root node")?;
    let desc = find_prop(root)
        .ok_or("panel_description property not found in DTBO")?;

    Ok(desc.to_vec())
}

/// Extract current customization config from an existing mipi-panel.dtbo.
pub fn extract_config(dtbo_bytes: &[u8]) -> PanelConfig {
    let fdt = match fdt::Fdt::new(dtbo_bytes) {
        Ok(f) => f,
        Err(_) => return PanelConfig::default(),
    };

    let mut config = PanelConfig::default();

    // Look for rotation in panel overlay fragment
    fn find_u32_prop(fdt: &fdt::Fdt, prop_name: &str) -> Option<u32> {
        fn search<'a>(node: fdt::node::FdtNode<'a, 'a>, name: &str) -> Option<u32> {
            if let Some(prop) = node.property(name) {
                if prop.value.len() >= 4 {
                    return Some(u32::from_be_bytes([
                        prop.value[0], prop.value[1], prop.value[2], prop.value[3],
                    ]));
                }
            }
            for child in node.children() {
                if let Some(v) = search(child, name) {
                    return Some(v);
                }
            }
            None
        }
        let root = fdt.find_node("/")?;
        search(root, prop_name)
    }

    fn has_prop(fdt: &fdt::Fdt, prop_name: &str) -> bool {
        fn search(node: fdt::node::FdtNode, name: &str) -> bool {
            if node.property(name).is_some() {
                return true;
            }
            for child in node.children() {
                if search(child, name) {
                    return true;
                }
            }
            false
        }
        match fdt.find_node("/") {
            Some(root) => search(root, prop_name),
            None => false,
        }
    }

    if let Some(rot) = find_u32_prop(&fdt, "rotation") {
        config.rotation = rot;
    }
    config.invert_left_stick = has_prop(&fdt, "invert-absx");
    config.invert_right_stick = has_prop(&fdt, "invert-absrx");
    config.hp_invert = has_prop(&fdt, "__fixups__")
        || find_u32_prop(&fdt, "simple-audio-card,hp-det-gpio").is_some();

    // Better HP invert detection: check if __fixups__ node exists
    if let Some(root) = fdt.find_node("/") {
        for child in root.children() {
            if child.name == "__fixups__" {
                config.hp_invert = true;
                break;
            }
        }
    }

    config
}

/// Build a custom DTBO by cloning the original and injecting customization properties.
/// Preserves ALL hardware nodes (reset-gpios, pinctrl, power supply, __fixups__).
pub fn build_custom_dtbo(original_dtbo: &[u8], config: &PanelConfig) -> Result<Vec<u8>, String> {
    let fdt_parsed = fdt::Fdt::new(original_dtbo)
        .map_err(|e| format!("Invalid DTBO: {:?}", e))?;
    let root = fdt_parsed.find_node("/")
        .ok_or("DTBO has no root node")?;

    let mut builder = FdtBuilder::new();
    clone_node_with_config(&root, &mut builder, config);
    Ok(builder.finish())
}

/// Recursively clone a DT node, injecting customization properties where appropriate.
/// Detects node type by existing properties:
///   - panel_description → panel overlay: inject rotation
///   - joypad-name → joypad overlay: inject stick inversion
///   - simple-audio-card,hp-det-gpio → audio overlay: flip HP polarity
fn clone_node_with_config(
    node: &fdt::node::FdtNode,
    builder: &mut FdtBuilder,
    config: &PanelConfig,
) {
    builder.begin_node(node.name);

    // Detect what kind of __overlay__ this is
    let has_panel_desc = node.property("panel_description").is_some();
    let has_joypad_name = node.property("joypad-name").is_some();
    let has_hp_det = node.property("simple-audio-card,hp-det-gpio").is_some();

    // Determine which properties to skip (will be written with new values)
    let override_rotation = has_panel_desc && config.rotation != 0;
    let override_stick = has_joypad_name
        && (config.invert_left_stick || config.invert_right_stick);
    let override_hp = has_hp_det && config.hp_invert;

    // Copy all properties, skipping ones we'll override
    for prop in node.properties() {
        if override_rotation && prop.name == "rotation" {
            continue;
        }
        if override_stick
            && matches!(
                prop.name,
                "invert-absx" | "invert-absy" | "invert-absrx" | "invert-absry"
            )
        {
            continue;
        }
        if override_hp && prop.name == "simple-audio-card,hp-det-gpio" {
            continue;
        }
        builder.prop_bytes(prop.name, prop.value);
    }

    // Inject customizations
    if override_rotation {
        builder.prop_u32("rotation", config.rotation);
    }
    if has_joypad_name {
        if config.invert_left_stick {
            builder.prop_u32("invert-absx", 1);
            builder.prop_u32("invert-absy", 1);
        }
        if config.invert_right_stick {
            builder.prop_u32("invert-absrx", 1);
            builder.prop_u32("invert-absry", 1);
        }
    }
    if override_hp {
        // Flip polarity: [phandle, pin, flags] → toggle flags (0↔1)
        if let Some(prop) = node.property("simple-audio-card,hp-det-gpio") {
            if prop.value.len() >= 12 {
                let mut data = prop.value.to_vec();
                let flags = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
                let new_flags = if flags == 0 { 1u32 } else { 0u32 };
                data[8..12].copy_from_slice(&new_flags.to_be_bytes());
                builder.prop_bytes("simple-audio-card,hp-det-gpio", &data);
            }
        }
    }

    // Recurse into children
    for child in node.children() {
        clone_node_with_config(&child, builder, config);
    }

    builder.end_node();
}

/// Read a panel DTBO from inside a raw .img file (FAT32 BOOT partition).
/// The BOOT partition starts at offset 16 MiB in our image layout.
pub fn read_dtbo_from_image(image_path: &Path, panel_dtbo: &str) -> Result<Vec<u8>, String> {
    use fscommon::StreamSlice;
    use std::fs::File;

    let file = File::open(image_path)
        .map_err(|e| format!("Cannot open image: {}", e))?;

    let file_len = file.metadata()
        .map_err(|e| format!("Metadata error: {}", e))?.len();

    // BOOT partition starts at 16 MiB
    const BOOT_OFFSET: u64 = 16 * 1024 * 1024;
    // BOOT partition is 256 MiB in our layout
    const BOOT_SIZE: u64 = 256 * 1024 * 1024;

    let end = (BOOT_OFFSET + BOOT_SIZE).min(file_len);
    if end <= BOOT_OFFSET {
        return Err("Image too small — no BOOT partition".into());
    }

    let slice = StreamSlice::new(file, BOOT_OFFSET, end)
        .map_err(|e| format!("StreamSlice error: {}", e))?;

    let fs = fatfs::FileSystem::new(slice, fatfs::FsOptions::new())
        .map_err(|e| format!("FAT32 parse error: {}", e))?;

    let root_dir = fs.root_dir();
    let overlays_dir = root_dir.open_dir("overlays")
        .map_err(|e| format!("Cannot open overlays/: {}", e))?;

    let mut dtbo_file = overlays_dir.open_file(panel_dtbo)
        .map_err(|e| format!("Cannot open overlays/{}: {}", panel_dtbo, e))?;

    let mut buf = Vec::new();
    dtbo_file.read_to_end(&mut buf)
        .map_err(|e| format!("Read error: {}", e))?;

    Ok(buf)
}

/// Write a custom DTBO to a temp file and return its path.
pub fn write_temp_dtbo(data: &[u8]) -> Result<std::path::PathBuf, String> {
    use std::io::Write;

    let path = std::env::temp_dir().join("archr-custom.dtbo");
    let mut file = std::fs::File::create(&path)
        .map_err(|e| format!("Cannot create temp DTBO: {}", e))?;
    file.write_all(data)
        .map_err(|e| format!("Write error: {}", e))?;
    file.sync_all()
        .map_err(|e| format!("Sync error: {}", e))?;
    Ok(path)
}
