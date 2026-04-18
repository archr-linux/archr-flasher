fn main() {
    #[allow(unused_mut)]
    let mut attrs = tauri_build::Attributes::new();

    // On Windows: embed admin manifest so the app requests UAC elevation at
    // startup (like Rufus). This eliminates the need for runtime PowerShell
    // elevation and prevents visible console windows during flash operations.
    #[cfg(windows)]
    {
        let windows = tauri_build::WindowsAttributes::new()
            .app_manifest(include_str!("admin.manifest"));
        attrs = attrs.windows_attributes(windows);

        // Windows 7 compatibility: compile bcryptprimitives.dll shim.
        // On Win7, ProcessPrng doesn't exist in the system DLL.
        // Our shim DLL intercepts the import (via DLL search order) and
        // falls back to RtlGenRandom from advapi32.dll.
        //
        // The shim DLL must be placed next to the .exe in the final package.
        // For NSIS/MSI installers, add it to the bundle resources.
        // For portable builds, copy it manually next to the .exe.
        //
        // Build the shim as a separate DLL using the cc crate's MSVC compiler:
        let out_dir = std::env::var("OUT_DIR").unwrap();
        let shim_src = std::path::Path::new("src/win7_shim.c");

        if shim_src.exists() {
            let status = std::process::Command::new("cl")
                .args(&[
                    "/LD", "/nologo",
                    "/Fe:", &format!("{}\\bcryptprimitives.dll", out_dir),
                    "src\\win7_shim.c",
                    "advapi32.lib",
                ])
                .status();

            match status {
                Ok(s) if s.success() => {
                    println!("cargo:warning=Built bcryptprimitives.dll shim for Win7 compat");
                    // Copy to target dir so it's next to the exe
                    let target_dir = std::path::Path::new(&out_dir)
                        .ancestors().nth(3).unwrap().to_path_buf();
                    let _ = std::fs::copy(
                        format!("{}\\bcryptprimitives.dll", out_dir),
                        target_dir.join("bcryptprimitives.dll"),
                    );
                }
                _ => {
                    // Fallback: try MinGW cross-compiler
                    let _ = std::process::Command::new("x86_64-w64-mingw32-gcc")
                        .args(&[
                            "-shared", "-o",
                            &format!("{}/bcryptprimitives.dll", out_dir),
                            "src/win7_shim.c",
                            "-ladvapi32",
                        ])
                        .status();
                    println!("cargo:warning=Win7 shim: tried MinGW fallback");
                }
            }
        }
    }

    tauri_build::try_build(attrs).expect("failed to run build");
}
