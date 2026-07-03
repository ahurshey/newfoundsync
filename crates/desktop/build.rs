// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Build script: on Windows, embed the app icon into newfoundsync.exe so it
//! shows in Explorer, the taskbar, and the Start menu. No-op elsewhere.

fn main() {
    #[cfg(target_os = "windows")]
    {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let ico = std::path::Path::new(&manifest)
            .join("..")
            .join("..")
            .join("branding")
            .join("icon.ico");
        println!("cargo:rerun-if-changed={}", ico.display());
        if ico.exists() {
            let mut res = winresource::WindowsResource::new();
            res.set_icon(ico.to_str().unwrap());
            if let Err(e) = res.compile() {
                // Don't fail the build if the resource compiler is unavailable;
                // the runtime window/tray icons still apply.
                println!("cargo:warning=icon embed skipped: {e}");
            }
        } else {
            println!("cargo:warning=branding/icon.ico not found; run branding/gen-icons.ps1");
        }
    }
}
