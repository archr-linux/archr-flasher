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
    // HP invert defaults to false. The checkbox means "flip polarity from overlay default".
    // We can't know if the overlay polarity was already flipped without the original DTB,
    // so default to false (use overlay as-is).
    config.hp_invert = false;

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
    // Panel overlay: has panel_description
    let has_panel_desc = node.property("panel_description").is_some();
    // Joypad overlay: has compatible with "joypad" in name, or has io-channel-names,
    // or has button-adc-scale (covers both DTS base and overlay fragments)
    let has_joypad = node.property("joypad-name").is_some()
        || node.property("button-adc-scale").is_some()
        || node.property("io-channel-names").is_some()
        || node.property("compatible").map_or(false, |p| {
            std::str::from_utf8(p.value).unwrap_or("").contains("joypad")
        });
    // Audio HP detect: has the specific GPIO property (not just __fixups__)
    let has_hp_det = node.property("simple-audio-card,hp-det-gpio").is_some();

    // Determine which properties to override
    let override_rotation = has_panel_desc && config.rotation != 0;
    // For stick: always strip existing invert props on joypad nodes, then re-add based on config
    let override_stick = has_joypad;
    // For HP: always manage hp-det-gpio polarity when the property exists
    let override_hp = has_hp_det;

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
    if has_joypad {
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
        if let Some(prop) = node.property("simple-audio-card,hp-det-gpio") {
            if prop.value.len() >= 12 {
                if config.hp_invert {
                    // Flip polarity: [phandle, pin, flags] → toggle flags (0↔1)
                    let mut data = prop.value.to_vec();
                    let flags = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
                    let new_flags = if flags == 0 { 1u32 } else { 0u32 };
                    data[8..12].copy_from_slice(&new_flags.to_be_bytes());
                    builder.prop_bytes("simple-audio-card,hp-det-gpio", &data);
                } else {
                    // Keep original polarity
                    builder.prop_bytes("simple-audio-card,hp-det-gpio", prop.value);
                }
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
/// Uses a minimal FAT32 reader that tolerates BPB cluster-count ambiguity
/// (the `fatfs` crate rejects valid FAT32 partitions with <65525 clusters).
pub fn read_dtbo_from_image(image_path: &Path, panel_dtbo: &str) -> Result<Vec<u8>, String> {
    use std::io::{Read as _, Seek, SeekFrom};

    let mut file = std::fs::File::open(image_path)
        .map_err(|e| format!("Cannot open image: {}", e))?;
    let file_len = file.metadata()
        .map_err(|e| format!("Metadata error: {}", e))?.len();

    const BOOT_OFFSET: u64 = 16 * 1024 * 1024;
    if file_len <= BOOT_OFFSET {
        return Err("Image too small — no BOOT partition".into());
    }

    // Read BPB (BIOS Parameter Block) from boot sector
    file.seek(SeekFrom::Start(BOOT_OFFSET))
        .map_err(|e| format!("Seek error: {}", e))?;
    let mut bpb = [0u8; 512];
    file.read_exact(&mut bpb)
        .map_err(|e| format!("Read BPB error: {}", e))?;

    // Check boot sector signature
    if bpb[510] != 0x55 || bpb[511] != 0xAA {
        return Err("Invalid boot sector signature".into());
    }

    let bytes_per_sector = u16::from_le_bytes([bpb[11], bpb[12]]) as u64;
    let sectors_per_cluster = bpb[13] as u64;
    let reserved_sectors = u16::from_le_bytes([bpb[14], bpb[15]]) as u64;
    let num_fats = bpb[16] as u64;
    let sectors_per_fat_32 = u32::from_le_bytes([bpb[36], bpb[37], bpb[38], bpb[39]]) as u64;
    let root_cluster = u32::from_le_bytes([bpb[44], bpb[45], bpb[46], bpb[47]]);

    if bytes_per_sector == 0 || sectors_per_cluster == 0 || sectors_per_fat_32 == 0 {
        return Err("Invalid FAT32 BPB parameters".into());
    }

    let cluster_size = bytes_per_sector * sectors_per_cluster;
    let fat_start = BOOT_OFFSET + reserved_sectors * bytes_per_sector;
    let data_start = fat_start + num_fats * sectors_per_fat_32 * bytes_per_sector;

    let cluster_to_offset = |cluster: u32| -> u64 {
        data_start + (cluster as u64 - 2) * cluster_size
    };

    // Read FAT entry for a given cluster
    let read_fat_entry = |f: &mut std::fs::File, cluster: u32| -> Result<u32, String> {
        let offset = fat_start + cluster as u64 * 4;
        f.seek(SeekFrom::Start(offset)).map_err(|e| format!("FAT seek: {}", e))?;
        let mut buf = [0u8; 4];
        f.read_exact(&mut buf).map_err(|e| format!("FAT read: {}", e))?;
        Ok(u32::from_le_bytes(buf) & 0x0FFFFFFF)
    };

    // Read all data for a cluster chain
    let read_chain = |f: &mut std::fs::File, start_cluster: u32| -> Result<Vec<u8>, String> {
        let mut data = Vec::new();
        let mut cluster = start_cluster;
        let mut buf = vec![0u8; cluster_size as usize];
        loop {
            if cluster < 2 || cluster >= 0x0FFFFFF8 { break; }
            let offset = cluster_to_offset(cluster);
            f.seek(SeekFrom::Start(offset)).map_err(|e| format!("Data seek: {}", e))?;
            f.read_exact(&mut buf).map_err(|e| format!("Data read: {}", e))?;
            data.extend_from_slice(&buf);
            cluster = read_fat_entry(f, cluster)?;
        }
        Ok(data)
    };

    // Parse directory entries to find a named entry (case-insensitive 8.3 + LFN)
    fn find_entry(dir_data: &[u8], name: &str) -> Option<(u32, u32, bool)> {
        let name_upper = name.to_uppercase();
        let mut lfn_buf = String::new();
        let mut i = 0;
        while i + 32 <= dir_data.len() {
            let entry = &dir_data[i..i + 32];
            if entry[0] == 0x00 { break; } // end of directory
            if entry[0] == 0xE5 { i += 32; continue; } // deleted

            // LFN entry (attr == 0x0F)
            if entry[11] == 0x0F {
                // Extract UCS-2 chars from LFN entry
                let mut chars = Vec::new();
                for &off in &[1,3,5,7,9, 14,16,18,20,22,24, 28,30] {
                    if off + 1 < 32 {
                        let c = u16::from_le_bytes([entry[off], entry[off + 1]]);
                        if c == 0 || c == 0xFFFF { break; }
                        if let Some(ch) = char::from_u32(c as u32) {
                            chars.push(ch);
                        }
                    }
                }
                let seq = entry[0] & 0x1F;
                if entry[0] & 0x40 != 0 {
                    lfn_buf.clear();
                }
                // LFN entries are in reverse order; prepend
                let part: String = chars.into_iter().collect();
                lfn_buf = format!("{}{}", part, lfn_buf);
                i += 32;
                continue;
            }

            // Short name entry
            let attr = entry[11];
            let is_dir = attr & 0x10 != 0;
            let cluster_hi = u16::from_le_bytes([entry[20], entry[21]]) as u32;
            let cluster_lo = u16::from_le_bytes([entry[26], entry[27]]) as u32;
            let cluster = (cluster_hi << 16) | cluster_lo;
            let size = u32::from_le_bytes([entry[28], entry[29], entry[30], entry[31]]);

            // Check LFN match first
            if !lfn_buf.is_empty() && lfn_buf.eq_ignore_ascii_case(&name_upper) {
                return Some((cluster, size, is_dir));
            }

            // Check 8.3 short name (8 bytes name + 3 bytes ext, space-padded)
            let base: String = entry[0..8].iter()
                .map(|&b| b as char)
                .collect::<String>();
            let ext: String = entry[8..11].iter()
                .map(|&b| b as char)
                .collect::<String>();
            let base = base.trim();
            let ext = ext.trim();
            let short_name = if ext.is_empty() {
                base.to_uppercase()
            } else {
                format!("{}.{}", base, ext).to_uppercase()
            };
            if short_name.eq_ignore_ascii_case(&name_upper) {
                return Some((cluster, size, is_dir));
            }

            lfn_buf.clear();
            i += 32;
        }
        None
    }

    // 1. Read root directory
    let root_data = read_chain(&mut file, root_cluster)?;

    // 2. Find "overlays" directory
    let (overlays_cluster, _, is_dir) = find_entry(&root_data, "overlays")
        .ok_or("overlays/ directory not found in boot partition")?;
    if !is_dir {
        return Err("overlays is not a directory".into());
    }

    // 3. Read overlays directory and traverse path components
    //    panel_dtbo can be "panel0.dtbo" or "soysauce/ss_v04.dtbo"
    let parts: Vec<&str> = panel_dtbo.split('/').collect();
    let mut current_data = read_chain(&mut file, overlays_cluster)?;

    // Traverse subdirectories if any (all parts except the last)
    for &subdir in &parts[..parts.len() - 1] {
        let (sub_cluster, _, is_sub_dir) = find_entry(&current_data, subdir)
            .ok_or_else(|| format!("overlays/{} not found", subdir))?;
        if !is_sub_dir {
            return Err(format!("{} is not a directory", subdir));
        }
        current_data = read_chain(&mut file, sub_cluster)?;
    }

    // 4. Find the DTBO file in the final directory
    let dtbo_name = parts.last().unwrap();
    let (dtbo_cluster, dtbo_size, _) = find_entry(&current_data, dtbo_name)
        .ok_or_else(|| format!("overlays/{} not found", panel_dtbo))?;

    // 5. Read DTBO data
    let dtbo_data = read_chain(&mut file, dtbo_cluster)?;

    // Trim to actual file size
    Ok(dtbo_data[..dtbo_size as usize].to_vec())
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
