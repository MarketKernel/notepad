//! On Windows, embeds the app icon, version info and the application manifest
//! into the executable.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/windows.manifest");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico")
            .set_manifest_file("assets/windows.manifest")
            .set("FileDescription", "Notepad")
            .set("ProductName", "Notepad")
            .set("OriginalFilename", "notepad.exe")
            .set("LegalCopyright", "Copyright (c) 2026 MarketKernel");
        res.compile().expect("compile Windows resources");
    }
}
