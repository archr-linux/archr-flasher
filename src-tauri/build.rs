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

        // Windows 7 compatibility: delay-load bcryptprimitives.dll so the
        // binary doesn't fail to start when ProcessPrng is missing.
        // The failure hook in win7_shim.c falls back to RtlGenRandom.
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:bcryptprimitives.dll");
        println!("cargo:rustc-link-lib=delayimp");

        cc::Build::new()
            .file("src/win7_shim.c")
            .compile("win7_shim");
    }

    tauri_build::try_build(attrs).expect("failed to run build");
}
