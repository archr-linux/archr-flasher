# Arch R Flasher

> Flash. Select panel. Play.

Cross-platform desktop app for flashing [Arch R](https://github.com/archr-linux/Arch-R) onto R36S and clone gaming consoles. Handles image download, SD card writing, and display panel configuration — all in one step.

## Features

- **Two tabs:** Flash (full image write) and Overlay (change panel on existing SD)
- **20 display panels:** 8 original R36S + 12 clone variants, data-driven selection
- **Customizations:** display rotation, analog stick inversion, headphone detect polarity
- **Image download:** fetches latest release from GitHub with SHA256 verification and caching
- **Cross-platform:** Windows, Linux, macOS with native privilege escalation
- **In-app updates:** automatic update checking and installation
- **4 languages:** English, Portuguese (BR), Spanish, Chinese

## Download

Get the latest release for your platform from [Releases](../../releases).

| Platform | File |
|----------|------|
| Windows | `Arch.R.Flasher_1.0.0_x64-setup.exe` |
| Linux (deb) | `arch-r-flasher_1.0.0_amd64.deb` |
| Linux (AppImage) | `arch-r-flasher_1.0.0_amd64.AppImage` |
| macOS | `Arch.R.Flasher_1.0.0_aarch64.dmg` |

## Usage

### Flash Tab

1. Select console type — **R36S Original** (8 panels) or **R36S Clone** (12 panels)
2. Select image — download latest from GitHub or pick a local `.img` / `.img.xz` file
3. Select your display panel
4. Optionally adjust customizations (rotation, stick inversion, HP detect)
5. Select target SD card
6. Click **FLASH**

The app decompresses XZ images, writes to SD, and injects the correct panel overlay (DTBO) into the BOOT partition — no manual overlay copying needed.

### Overlay Tab

Change the display panel on an already-flashed Arch R SD card without reflashing:

1. Insert an Arch R SD card
2. App auto-detects the BOOT partition and shows current panel + settings
3. Select a new panel and/or adjust customizations
4. Click **APPLY**

## Building from Source

### Requirements

- [Rust](https://rustup.rs/) (stable)
- Tauri CLI: `cargo install tauri-cli`

#### Linux

```bash
sudo apt install -y libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev
```

#### macOS

Xcode Command Line Tools.

#### Windows

[WebView2](https://developer.microsoft.com/en-us/microsoft-edge/webview2/) (pre-installed on Windows 11).

### Build

```bash
# Development
cargo tauri dev

# Release (generates installer for current platform)
cargo tauri build
```

## Architecture

```
archr-flasher/
├── src-tauri/
│   ├── src/
│   │   ├── main.rs            # Tauri entry point + 11 IPC commands
│   │   ├── panels.rs          # Panel definitions (20 panels, data-driven)
│   │   ├── disk.rs            # Removable disk detection (Linux/macOS/Windows)
│   │   ├── flash.rs           # Image writing + privilege escalation
│   │   ├── github.rs          # GitHub Releases API + image download
│   │   ├── overlay.rs         # SD card panel overlay read/write
│   │   ├── panel_config.rs    # DTBO customization injection
│   │   └── dtbo_builder.rs    # FDT binary builder (no external tools)
│   ├── Cargo.toml
│   └── tauri.conf.json
├── src/
│   ├── index.html             # UI (two tabs: Flash + Overlay)
│   ├── style.css              # Dark theme
│   ├── main.js                # Frontend logic (vanilla JS)
│   └── i18n/                  # Translations (en, pt-BR, es, zh)
└── .github/
    └── workflows/             # CI/CD
```

### How It Works

**Flash flow:**
1. Download or select `.img.xz` image
2. Decompress XZ (streaming, 4MB chunks)
3. Read source panel DTBO from image's FAT32 BOOT partition
4. If customizations are set, build a modified DTBO with injected properties
5. Write image to SD card via platform-specific privileged script
6. Inject final DTBO as `overlays/mipi-panel.dtbo` on BOOT partition

**Overlay flow:**
1. Detect mounted Arch R BOOT partition
2. Read current `mipi-panel.dtbo` — identify panel via `panel_description` hash
3. User selects new panel + customizations
4. Build DTBO and write to `overlays/mipi-panel.dtbo`

### Privilege Escalation

| Platform | Method | Notes |
|----------|--------|-------|
| Linux | `pkexec` | No terminal window needed |
| macOS | `osascript` (AppleScript) | Native admin prompt |
| Windows | Admin manifest at startup | No runtime UAC prompt |

### Panel DTBO System

The app includes a built-in FDT binary builder — no `dtc` or device-tree-compiler dependency needed. Customizations (rotation, stick inversion, HP detect polarity) are injected as DT properties into the panel overlay, preserving all original hardware nodes (reset-gpios, pinctrl, power supply).

## Supported Panels

### Original R36S (8 panels)

| Panel | Overlay | Controller |
|-------|---------|------------|
| Panel 0 | panel0.dtbo | ST7703 |
| Panel 1 | panel1.dtbo | ST7703 |
| Panel 2 | panel2.dtbo | ST7703 |
| Panel 3 | panel3.dtbo | ST7703 |
| Panel 4 | panel4.dtbo | ST7703 |
| Panel 4 V22 | panel4-v22.dtbo | ST7703 |
| Panel 5 | panel5.dtbo | ST7703 |
| R46H | r46h.dtbo | ST7703 (1024x768) |

### Clone R36S (12 panels)

| Panel | Overlay | Controller |
|-------|---------|------------|
| Clone 1 | clone_panel_1.dtbo | ST7703 |
| Clone 2 | clone_panel_2.dtbo | ST7703 |
| Clone 3 | clone_panel_3.dtbo | NV3051D |
| Clone 4 | clone_panel_4.dtbo | NV3051D |
| Clone 5 | clone_panel_5.dtbo | ST7703 |
| Clone 6 | clone_panel_6.dtbo | NV3051D |
| Clone 7 | clone_panel_7.dtbo | JD9365DA |
| Clone 8 G80CA | clone_panel_8.dtbo | ST7703 |
| Clone 9 | clone_panel_9.dtbo | NV3051D |
| Clone 10 | clone_panel_10.dtbo | ST7703 |
| R36 Max | r36_max.dtbo | ST7703 (720x720) |
| RX6S | rx6s.dtbo | NV3051D |

## License

GPL v3
