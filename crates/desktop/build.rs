// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Build script: stamp the git commit into the binary (so a running server can identify the exact
//! build it came from) and, on Windows, embed the app icon into newfoundsync.exe so it shows in
//! Explorer, the taskbar, and the Start menu.

/// Inputs that actually end up in the binary. Used for BOTH the rerun triggers and the dirty check, so
/// the two can never disagree — a README or CI edit makes the git tree dirty without changing a single
/// compiled byte, and calling that build "-dirty" would be noise.
///
/// These paths are relative to this package (cargo runs a build script with cwd = the package root).
/// Cargo scans directories recursively, so naming a directory covers every file under it.
const BINARY_INPUTS: &[&str] = &[
    "src",              // this crate
    "web",              // the client, embedded via include_str!
    "Cargo.toml",       // features / deps
    "../core",          // the workspace's other crate
    "../../Cargo.lock", // dependency versions
];

/// Emit `NFS_GIT_SHA` for the binary to report. Hand-copied builds on several machines are otherwise
/// indistinguishable, which makes "is that box running the build with the fix?" unanswerable.
fn emit_git_sha() {
    // Cargo re-runs a build script ONLY when a declared rerun-if-changed path changes. Watching just
    // .git/HEAD was not enough: editing a source file recompiled the crate WITHOUT re-running this
    // script, so the stamp came from cargo's cache and a dirty build reported itself as the clean
    // commit it was based on (and, after reverting, a clean build kept reporting "-dirty").
    for p in ["../../.git/HEAD", "../../.git/refs/heads"] {
        if std::path::Path::new(p).exists() {
            println!("cargo:rerun-if-changed={p}"); // a bare `git commit` re-stamps too
        }
    }
    for p in BINARY_INPUTS {
        println!("cargo:rerun-if-changed={p}");
    }

    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    // Mark a build made on top of uncommitted changes, so a local build can't masquerade as the clean
    // commit it was based on. Scoped with `-- <paths>` to the SAME inputs we watch above: that keeps
    // the flag honest (it means "the compiled sources differ from HEAD") and consistent with when
    // cargo actually re-runs us.
    let mut args: Vec<&str> = vec!["status", "--porcelain", "--untracked-files=no", "--"];
    args.extend_from_slice(BINARY_INPUTS);
    let dirty = std::process::Command::new("git")
        .args(&args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    println!("cargo:rustc-env=NFS_GIT_SHA={sha}{}", if dirty { "-dirty" } else { "" });
}

fn main() {
    emit_git_sha();

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
