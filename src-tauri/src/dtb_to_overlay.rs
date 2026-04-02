//! Convert a stock firmware DTB into an ArchR panel overlay DTBO.
//! Pure Rust implementation — no Python, no external tools.
//!
//! Reads the stock DTB, extracts MIPI panel configuration (timings, init
//! sequence, format, lanes, GPIOs), and generates a DTBO overlay compatible
//! with the ArchR generic-dsi panel driver.

use crate::dtbo_builder::FdtBuilder;

/// Generate a DTBO overlay from a stock firmware DTB.
pub fn generate_overlay(dtb_data: &[u8]) -> Result<Vec<u8>, String> {
    let fdt = fdt::Fdt::new(dtb_data)
        .map_err(|e| format!("Invalid DTB: {:?}", e))?;

    // Find DSI panel node via __symbols__ or by scanning
    let panel_node = find_panel_node(&fdt)?;

    // Extract panel description lines
    let panel_desc = extract_panel_description(&fdt, &panel_node)?;

    // Build the DTBO
    let dtbo = build_overlay(&panel_desc);

    Ok(dtbo)
}

/// Find the DSI panel node in the DTB.
fn find_panel_node<'a>(fdt: &'a fdt::Fdt<'a>) -> Result<fdt::node::FdtNode<'a, 'a>, String> {
    // Try common paths
    let paths = [
        "/dsi@ff450000/panel@0",  // RK3326 (PX30)
        "/dsi@fe060000/panel@0",  // RK3566
    ];

    for path in &paths {
        if let Some(node) = fdt.find_node(path) {
            return Ok(node);
        }
    }

    // Scan for any node with panel-init-sequence property
    fn find_panel_recursive<'a>(
        node: fdt::node::FdtNode<'a, 'a>,
    ) -> Option<fdt::node::FdtNode<'a, 'a>> {
        if node.property("panel-init-sequence").is_some() {
            return Some(node);
        }
        for child in node.children() {
            if let Some(found) = find_panel_recursive(child) {
                return Some(found);
            }
        }
        None
    }

    let root = fdt.find_node("/").ok_or("No root node")?;
    find_panel_recursive(root).ok_or_else(|| {
        "No MIPI panel node found. The DTB must contain a panel-init-sequence property.".into()
    })
}

/// Read a u32 property with a default value.
fn prop_u32_or(node: &fdt::node::FdtNode, name: &str, default: u32) -> u32 {
    node.property(name)
        .and_then(|p| {
            if p.value.len() >= 4 {
                Some(u32::from_be_bytes([p.value[0], p.value[1], p.value[2], p.value[3]]))
            } else {
                None
            }
        })
        .unwrap_or(default)
}

/// Read a u32 property (required).
fn prop_u32(node: &fdt::node::FdtNode, name: &str) -> Result<u32, String> {
    node.property(name)
        .and_then(|p| {
            if p.value.len() >= 4 {
                Some(u32::from_be_bytes([p.value[0], p.value[1], p.value[2], p.value[3]]))
            } else {
                None
            }
        })
        .ok_or_else(|| format!("Missing property: {}", name))
}

struct Mode {
    clock: u32,
    hor: [u32; 4],
    ver: [u32; 4],
    #[allow(dead_code)]
    is_default: bool,
}

/// Extract all panel description lines from the DTB panel node.
fn extract_panel_description(
    _fdt: &fdt::Fdt,
    panel: &fdt::node::FdtNode,
) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();

    // Delays
    let delays = [
        prop_u32_or(panel, "prepare-delay-ms", 50),
        prop_u32_or(panel, "reset-delay-ms", 50),
        prop_u32_or(panel, "init-delay-ms", 50),
        prop_u32_or(panel, "enable-delay-ms", 50),
        20, // ready
    ];
    let delays_str: Vec<String> = delays.iter().map(|d| d.to_string()).collect();

    // Format
    let fmt_val = prop_u32_or(panel, "dsi,format", 0);
    let fmt = match fmt_val {
        0 => "rgb888",
        1 => "rgb666",
        2 => "rgb666_packed",
        3 => "rgb565",
        _ => "rgb888",
    };

    // Lanes and flags
    let lanes = prop_u32(panel, "dsi,lanes")?;
    let flags = prop_u32(panel, "dsi,flags")? | 0x0400;

    // Physical size (try width-mm/height-mm, fallback to 3.5" diagonal)
    let (w, h) = get_panel_dimensions(panel);

    // G line: global panel config
    lines.push(format!(
        "G size={},{} delays={} format={} lanes={} flags=0x{:x}",
        w, h, delays_str.join(","), fmt, lanes, flags
    ));

    // Display timings
    let timings_node = panel.children()
        .find(|c| c.name.starts_with("display-timings"))
        .ok_or("No display-timings node found")?;

    let native_phandle = timings_node.property("native-mode")
        .and_then(|p| {
            if p.value.len() >= 4 {
                Some(u32::from_be_bytes([p.value[0], p.value[1], p.value[2], p.value[3]]))
            } else {
                None
            }
        });

    // Collect vendor modes
    let mut modes: Vec<(f64, Mode)> = Vec::new();
    let mut default_fps: Option<f64> = None;

    for timing in timings_node.children() {
        let clock_hz = prop_u32(&timing, "clock-frequency")?;
        let clock = (clock_hz + 500) / 1000; // to kHz

        let hor = [
            prop_u32(&timing, "hactive")?,
            prop_u32(&timing, "hfront-porch")?,
            prop_u32(&timing, "hsync-len")?,
            prop_u32(&timing, "hback-porch")?,
        ];
        let ver = [
            prop_u32(&timing, "vactive")?,
            prop_u32(&timing, "vfront-porch")?,
            prop_u32(&timing, "vsync-len")?,
            prop_u32(&timing, "vback-porch")?,
        ];

        let htotal: u32 = hor.iter().sum();
        let vtotal: u32 = ver.iter().sum();
        let fps = (clock as f64) * 1000.0 / (htotal as f64 * vtotal as f64);

        let phandle = prop_u32_or(&timing, "phandle", 0);
        let is_default = native_phandle.map_or(false, |n| n == phandle);

        if is_default {
            default_fps = Some(fps);
        }

        modes.push((fps, Mode { clock, hor, ver, is_default }));
    }

    let def_fps = default_fps.unwrap_or(60.0);

    // Generate modes for common refresh rates
    let common_fps: &[f64] = &[
        50.0 / 1.001, 50.0, 50.0070, 57.5, 59.7275,
        60.0 / 1.001, 60.0, 60.0988, 75.47, 90.0, 120.0,
    ];

    // First emit the default mode, then common modes
    let mut target_list: Vec<f64> = Vec::new();
    if let Some(dfps) = default_fps {
        target_list.push(dfps);
    }
    for &fps in common_fps {
        if default_fps.map_or(true, |d| (fps - d).abs() > 0.001) {
            target_list.push(fps);
        }
    }

    for target_fps in &target_list {
        if let Some(mode_line) = generate_mode(&modes, *target_fps, def_fps) {
            lines.push(mode_line);
        }
    }

    // Init sequence
    let init_prop = panel.property("panel-init-sequence")
        .ok_or("No panel-init-sequence property")?;

    let init_bytes = init_prop.value;
    let mut pos = 0;
    while pos + 3 <= init_bytes.len() {
        let _cmd = init_bytes[pos];
        let wait = init_bytes[pos + 1];
        let datalen = init_bytes[pos + 2] as usize;
        pos += 3;

        if pos + datalen > init_bytes.len() {
            break;
        }

        let data = &init_bytes[pos..pos + datalen];
        pos += datalen;

        let hex: String = data.iter().map(|b| format!("{:02x}", b)).collect();
        let wait_str = if wait > 0 {
            format!(" wait={}", wait)
        } else {
            String::new()
        };
        lines.push(format!("I seq={}{}", hex, wait_str));
    }

    Ok(lines)
}

/// Get panel physical dimensions (mm). Falls back to 3.5" diagonal estimate.
fn get_panel_dimensions(panel: &fdt::node::FdtNode) -> (u32, u32) {
    let w = prop_u32_or(panel, "width-mm", 0);
    let h = prop_u32_or(panel, "height-mm", 0);

    if w > 0 && h > 0 {
        return (w, h);
    }

    // Estimate from 3.5" diagonal
    let diag_mm = 3.5 * 25.4;
    // Try to get resolution from first timing mode
    let (hactive, vactive) = panel.children()
        .find(|c| c.name.starts_with("display-timings"))
        .and_then(|timings| timings.children().next())
        .map(|timing| {
            (
                prop_u32_or(&timing, "hactive", 640),
                prop_u32_or(&timing, "vactive", 480),
            )
        })
        .unwrap_or((640, 480));

    let px_diag = ((hactive as f64).powi(2) + (vactive as f64).powi(2)).sqrt();
    let est_w = (diag_mm * hactive as f64 / px_diag).round() as u32;
    let est_h = (diag_mm * vactive as f64 / px_diag).round() as u32;
    (est_w, est_h)
}

/// Generate a mode line for a target FPS by adjusting timings from the nearest vendor mode.
fn generate_mode(
    modes: &[(f64, Mode)],
    target_fps: f64,
    def_fps: f64,
) -> Option<String> {
    // Find nearest vendor mode >= target_fps
    let base = modes.iter()
        .filter(|(fps, _)| *fps >= target_fps)
        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let (_, base_mode) = base.or_else(|| {
        modes.iter().max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
    })?;

    let hor = base_mode.hor;
    let ver = base_mode.ver;
    let htotal: u32 = hor.iter().sum();
    let vtotal: u32 = ver.iter().sum();

    let perfect_clock = target_fps * htotal as f64 * vtotal as f64 / 1000.0;

    let clock = if base.is_some() {
        base_mode.clock
    } else {
        ((perfect_clock / 10.0).ceil() as u32) * 10
    };

    let max_vtotal = (vtotal as f64 * 1.25) as u32;

    // Bruteforce to find best htotal/vtotal for target fps
    let mut best: Option<(f64, u32, u32)> = None;

    for vt in vtotal..=max_vtotal {
        let c_start = clock;
        let c_end = (1.25 * perfect_clock) as u32;
        let mut c = c_start;
        while c <= c_end {
            let ht_f = c as f64 * 1000.0 / target_fps / vt as f64;
            if ht_f >= htotal as f64 && ht_f < htotal as f64 * 1.05 {
                let dev = (ht_f - ht_f.round()).abs();
                if best.map_or(true, |(best_dev, _, _)| dev < best_dev) {
                    best = Some((dev, c, vt));
                }
            }
            c += 10;
        }
    }

    let (_, new_clock, new_vtotal) = best?;
    let new_htotal = (new_clock as f64 * 1000.0 / target_fps / new_vtotal as f64).round() as u32;

    let add_h = new_htotal.saturating_sub(htotal);
    let add_v = new_vtotal.saturating_sub(vtotal);

    let mut new_hor = hor;
    let mut new_ver = ver;
    new_hor[2] += add_h; // add to hsync-len
    new_ver[2] += add_v; // add to vsync-len

    let is_default = (target_fps - def_fps).abs() < 0.001;
    let default_str = if is_default { " default=1" } else { "" };

    Some(format!(
        "M clock={} horizontal={},{},{},{} vertical={},{},{},{}{}",
        new_clock,
        new_hor[0], new_hor[1], new_hor[2], new_hor[3],
        new_ver[0], new_ver[1], new_ver[2], new_ver[3],
        default_str
    ))
}

/// Build the final DTBO overlay from panel description lines.
fn build_overlay(panel_desc: &[String]) -> Vec<u8> {
    let mut fdt = FdtBuilder::new();

    // Root node
    fdt.begin_node("");

    // fragment@0: target-path = "/"
    fdt.begin_node("fragment@0");
    fdt.prop_str("target-path", "/");

    // __overlay__
    fdt.begin_node("__overlay__");

    // DSI panel node (use generic path that works with __fixups__ or target-path)
    fdt.begin_node("dsi@ff450000");
    fdt.begin_node("panel@0");

    fdt.prop_str("compatible", "archr,generic-dsi");

    // panel_description as stringlist
    let desc_strs: Vec<&str> = panel_desc.iter().map(|s| s.as_str()).collect();
    fdt.prop_str_list("panel_description", &desc_strs);

    fdt.end_node(); // panel@0
    fdt.end_node(); // dsi@ff450000
    fdt.end_node(); // __overlay__
    fdt.end_node(); // fragment@0

    fdt.end_node(); // root

    fdt.finish()
}

