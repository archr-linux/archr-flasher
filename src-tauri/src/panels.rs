use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Panel {
    pub id: String,
    pub name: String,
    pub dtbo: String,
}

/// Original R36S panels (12 panels).
/// DTBO names derived from DTS filenames in generate-panel-dtbos.sh.
const PANELS_ORIGINAL: &[(&str, &str, &str)] = &[
    ("0",          "Panel 0",              "panel0.dtbo"),
    ("1",          "Panel 1",              "panel1.dtbo"),
    ("2",          "Panel 2",              "panel2.dtbo"),
    ("3",          "Panel 3",              "panel3.dtbo"),
    ("4",          "Panel 4",              "panel4.dtbo"),
    ("4v22",       "Panel 4 V22",          "panel4-v22.dtbo"),
    ("5",          "Panel 5",              "panel5.dtbo"),
    ("6",          "Panel 6",              "panel6.dtbo"),
    ("r35s",       "R35S Rumble",          "r35s-rumble.dtbo"),
    ("r36sp",      "R36S Plus",            "r36s-plus.dtbo"),
    ("r46h",       "R46H (1024x768)",      "r46h.dtbo"),
    ("rgb20s",     "RGB20S",               "rgb20s.dtbo"),
];

/// Clone R36S panels (12 panels).
/// DTBO names derived from CLONE_ORDER in generate-panel-dtbos.sh.
const PANELS_CLONE: &[(&str, &str, &str)] = &[
    ("C1",     "Clone 1 (ST7703)",         "clone_panel_1.dtbo"),
    ("C2",     "Clone 2 (ST7703)",         "clone_panel_2.dtbo"),
    ("C3",     "Clone 3 (NV3051D)",        "clone_panel_3.dtbo"),
    ("C4",     "Clone 4 (NV3051D)",        "clone_panel_4.dtbo"),
    ("C5",     "Clone 5 (ST7703)",         "clone_panel_5.dtbo"),
    ("C6",     "Clone 6 (NV3051D)",        "clone_panel_6.dtbo"),
    ("C7",     "Clone 7 (JD9365DA)",       "clone_panel_7.dtbo"),
    ("C8",     "Clone 8 G80CA (ST7703)",   "clone_panel_8.dtbo"),
    ("C9",     "Clone 9 (NV3051D)",        "clone_panel_9.dtbo"),
    ("C10",    "Clone 10 (ST7703)",        "clone_panel_10.dtbo"),
    ("R36Max", "R36 Max (ST7703 720x720)", "r36_max.dtbo"),
    ("RX6S",   "RX6S (NV3051D)",           "rx6s.dtbo"),
];

pub fn get_panels(console: &str) -> Vec<Panel> {
    let source = match console {
        "original" => PANELS_ORIGINAL,
        "clone" => PANELS_CLONE,
        _ => return vec![],
    };
    source.iter().map(|(id, name, dtbo)| Panel {
        id: id.to_string(),
        name: name.to_string(),
        dtbo: dtbo.to_string(),
    }).collect()
}
