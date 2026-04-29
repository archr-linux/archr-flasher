<p align="center">
  <strong>Arch R Flasher</strong><br>
  Flash. Select panel. Play.
</p>

<p align="center">
  <a href="https://github.com/archr-linux/archr-flasher/releases/latest"><img src="https://img.shields.io/github/release/archr-linux/archr-flasher.svg?color=0080FF&label=latest%20version&style=flat-square" alt="Latest Version"></a>
</p>

---

Cross-platform desktop app for flashing [Arch R](https://github.com/archr-linux/Arch-R) onto R36S Original, R36S Clone, and Soysauce gaming consoles. Handles image download, SD card writing, and per-motherboard panel configuration in one step.

## Features

- **Two tabs:** Flash (full image write) and Overlay (change panel on existing SD)
- **3 console families, 43 panels:** 15 R36S Original + 18 R36S Clone + 10 Soysauce, named after the exact motherboard revision (e.g. `R36S-V21_2024-12-18_2551.dtbo`)
- **Custom panel from stock DTB:** import a vendor `.dtb` file and the flasher auto-generates the matching MIPI overlay (pure Rust, no external tools)
- **Customizations:** display rotation, analog stick inversion (left/right), headphone-detect polarity, joypad variant (auto/oga/ogs), forced simple-audio mode, skip-vendor-mode toggle
- **Image download:** fetches latest release from GitHub with SHA256 verification (compares against the `.sha256` asset published with the release) and on-disk caching
- **Compression support:** `.img`, `.img.gz`, and `.img.xz` (streaming decompress, 4 MiB chunks)
- **Cross-platform:** Windows, Linux, macOS with native privilege escalation
- **In-app updates:** automatic update checking and one-click install
- **5 languages:** English, Portuguese (BR), Spanish, Chinese, Russian
- **Retry logic:** automatic retry on transient SD card I/O errors

## System Requirements

| Platform | Minimum |
|----------|---------|
| Windows  | **Windows 10 (1809) or later, x86_64.** WebView2 Runtime (pre-installed on Windows 11; on Windows 10 the installer pulls it via the Evergreen Bootstrapper) |
| Linux    | glibc 2.31+, `webkit2gtk-4.1`, `gtk-3`, `libayatana-appindicator3` |
| macOS    | macOS 10.15+, Apple Silicon |

> **Windows 7 is not supported.** Both the GUI runtime (Tauri 2 / Microsoft Edge WebView2) and the privileged flash script (PowerShell `Storage` module — `Clear-Disk`, `Get-Partition`, `Update-Disk`) require Windows 8+. Even with the bundled `bcryptprimitives.dll` shim that fixes Rust's `ProcessPrng` import, the WebView and the flash script still fail on Windows 7. Use Linux, macOS, or upgrade to Windows 10/11.

## Download

Grab the latest release for your platform from [Releases](https://github.com/archr-linux/archr-flasher/releases).

| Platform | File |
|----------|------|
| Windows | `Arch.R.Flasher_x64-setup.exe` |
| Linux (deb) | `arch-r-flasher_amd64.deb` |
| Linux (AppImage) | `arch-r-flasher_amd64.AppImage` |
| macOS | `Arch.R.Flasher_aarch64.dmg` |

## Usage

### Flash Tab

1. **Console** — pick **R36S Original**, **R36S Clone**, or **Soysauce**
2. **Image** — download the latest from GitHub, or pick a local `.img` / `.img.xz` / `.img.gz`
3. **Panel** — select your motherboard revision from the curated list, or import a stock vendor `.dtb` to auto-generate a custom overlay
4. **Customize** (optional) — rotation, stick inversion, HP invert, joypad variant, simple audio, vendor mode skip
5. **SD Card** — pick the target removable disk
6. **Flash**

The app decompresses the image, writes it to the SD card with retries, then mounts the BOOT partition and injects the chosen DTBO as `overlays/mipi-panel.dtbo`.

### Overlay Tab

Change the display panel on an already-flashed Arch R SD card without reflashing:

1. Insert an Arch R SD card
2. App auto-detects the BOOT partition and shows the current panel + settings
3. Select a new panel and/or adjust customizations
4. **Apply**

### Custom panel from a stock DTB

If your motherboard revision isn't in the curated list, drop in any working stock `r36s.dtb` (extracted from the vendor's image or pulled from `/sys/firmware/fdt`):

1. In the Panel step, choose **Import DTB → generate overlay**
2. Pick the `.dtb` file
3. The flasher's pure-Rust DTB→overlay generator extracts the MIPI panel node and produces a minimal DTBO at `<cache>/com.archr.flasher/custom-overlay.dtbo`
4. That overlay is applied like any built-in panel

No `dtc`, `mtools`, or device-tree-compiler needed at any step.

## Building from Source

### Requirements

- [Rust](https://rustup.rs/) (stable, edition 2024 — Rust 1.85+)
- Tauri CLI: `cargo install tauri-cli --version "^2"`

#### Linux

```bash
sudo apt install -y libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev
```

#### macOS

Xcode Command Line Tools.

#### Windows

[WebView2 Runtime](https://developer.microsoft.com/en-us/microsoft-edge/webview2/) (pre-installed on Windows 11; the installer auto-pulls it on Windows 10). MSVC toolchain via Visual Studio Build Tools.

### Build

```bash
# Development
cargo tauri dev

# Release (generates installer for current platform)
cargo tauri build
```

The Windows build also compiles `src/win7_shim.c` into a fallback `bcryptprimitives.dll` next to the `.exe`. This shim is harmless on Windows 8+ (forwards calls to the real system DLL) and would be required on Windows 7 — but the Tauri 2 WebView and the PS `Storage` module make Win7 unfeasible regardless. The shim ships only because the build pipeline keeps it for diagnostic builds.

## Architecture

```
archr-flasher/
 src-tauri/
   src/
     main.rs            # Tauri entry point + IPC commands
     panels.rs          # Panel definitions (43 panels, data-driven)
     disk.rs            # Removable disk detection (Linux/macOS/Windows)
     flash.rs           # Image writing + privilege escalation + retry
     github.rs          # GitHub Releases API + image download + SHA256 verify
     overlay.rs         # SD card panel overlay read/write
     panel_config.rs    # DTBO read/customization (built-in FAT32 reader)
     dtbo_builder.rs    # FDT binary builder (no external tools)
     dtb_to_overlay.rs  # Stock DTB → MIPI overlay generator (pure Rust)
     win7_shim.c        # Win7 ProcessPrng fallback DLL (compiled by build.rs)
   build.rs
   Cargo.toml
   tauri.conf.json
   admin.manifest       # Windows UAC elevation manifest
 src/
   index.html           # UI (two tabs: Flash + Overlay)
   style.css            # Dark theme
   main.js              # Frontend logic (vanilla JS)
   i18n/                # Translations (en, pt-BR, es, zh, ru)
 .github/
   workflows/           # CI/CD
```

### How It Works

**Flash flow:**
1. Download (with SHA256 verify) or select `.img.xz` / `.img.gz` / `.img`
2. Stream-decompress to app cache directory (4 MiB chunks)
3. Detect panel selection: built-in DTBO from the image's FAT32 BOOT partition, or a custom DTBO from disk (when the user imported a stock DTB)
4. If customizations are set, build a modified DTBO with injected DT properties (preserving original hardware nodes: reset-gpios, pinctrl, power supply, `__fixups__`)
5. Write image to SD card via platform-specific privileged script (with retries)
6. Mount BOOT partition (with retry) and inject DTBO as `overlays/mipi-panel.dtbo`

**Overlay flow:**
1. Detect a mounted Arch R BOOT partition
2. Read current `mipi-panel.dtbo` — identify panel via `panel_description` hash
3. User picks a new panel (built-in or custom DTB) + customizations
4. Build DTBO and write it back to `overlays/mipi-panel.dtbo`

### Privilege Escalation

| Platform | Method | Notes |
|----------|--------|-------|
| Linux | `pkexec` | No terminal window needed |
| macOS | `osascript` (AppleScript) | Native admin prompt |
| Windows | Admin manifest at startup (Rufus-style) | App is launched elevated; no runtime UAC popup, no visible PowerShell console |

### Panel DTBO System

The app ships a built-in FDT binary builder, a minimal FAT32 reader, and a DTB→overlay generator — no `dtc`, `mtools`, or `device-tree-compiler` dependency. Customizations (rotation, stick inversion, HP polarity, joypad variant, simple audio, skip vendor mode) are injected as DT properties into the panel overlay, preserving every original hardware node so the device boots identically to the vendor image.

## Licenses

Copyright (C) 2026-present [Arch R](https://github.com/archr-linux/Arch-R)

Licensed under the terms of the [GNU GPL Version 2](https://choosealicense.com/licenses/gpl-2.0/).

## Credits

Part of the [Arch R](https://github.com/archr-linux/Arch-R) project, built on top of [ROCKNIX](https://github.com/ROCKNIX/distribution).
