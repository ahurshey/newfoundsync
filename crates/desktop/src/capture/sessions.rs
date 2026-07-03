// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Enumerate selectable audio sources for the server GUI: apps currently making
//! sound (WASAPI audio sessions) AND visible windows (by title). Picking a window
//! captures that window's process tree — the practical way to grab "the Chrome that
//! has YouTube Music", since a browser routes all its tabs through one mixed audio
//! session that's otherwise just labelled "chrome".
//!
//! COM work (audio sessions) runs on a short-lived MTA thread so it never fights the
//! GUI thread's apartment. Window enumeration needs no COM and runs inline.

use std::collections::HashSet;
use std::path::Path;

use windows::core::{Interface, BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, TRUE};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Media::Audio::{
    eMultimedia, eRender, AudioSessionStateExpired, IAudioSessionControl2,
    IAudioSessionEnumerator, IAudioSessionManager2, IMMDeviceEnumerator, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    IsWindowVisible, GW_OWNER,
};

/// A selectable capture source — an app (audio session) or a window. `pid` is what
/// we feed to process-loopback capture (its whole process tree); `name` is the label.
/// `hwnd` is the raw window handle for titled-window entries (used for per-window VIDEO
/// capture); it's `None` for windowless audio sessions (background players).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioApp {
    pub pid: u32,
    pub name: String,
    pub exe: String,
    pub hwnd: Option<isize>,
}

/// The combined picker list: every titled window (recognizable, e.g. "YouTube Music
/// — Google Chrome") plus any windowless app that's making sound (background players).
/// Runs on a dedicated worker thread so the GUI never blocks on EnumWindows /
/// OpenProcess / COM (and so the COM session enum gets its own MTA apartment).
pub fn list_sources(exclude_pid: u32) -> Vec<AudioApp> {
    std::thread::Builder::new()
        .name("source-enum".into())
        .spawn(move || build_sources(exclude_pid))
        .ok()
        .and_then(|h| h.join().ok())
        .unwrap_or_default()
}

/// Resolve a picked PID (usually a top-level WINDOW process) to the process that actually owns an
/// audio RENDER session — the one whose audio process-loopback can isolate. Browsers render audio
/// in a hidden child audio-service process, and UWP apps run under ApplicationFrameHost, so the
/// window PID is frequently NOT the renderer; INCLUDE on it then grabs the wrong tree (or, if the
/// OS will not honor the per-PID filter, the whole mix). Strategy: if `target` already owns a
/// session keep it; else pick a session whose exe matches `target`'s exe (the app's own audio
/// child, e.g. the browser's audio service); else return `target` unchanged. Runs on a short-lived
/// MTA thread (COM), like `list_sources`.
pub fn resolve_render_pid(target: u32) -> u32 {
    std::thread::Builder::new()
        .name("render-pid-resolve".into())
        .spawn(move || {
            let target_exe = process_name(target).unwrap_or_default().to_lowercase();
            let sessions = enumerate_sessions(0); // 0 = exclude nothing (PID 0 is dropped inside)
            if sessions.iter().any(|s| s.pid == target) {
                return target; // already an audio renderer
            }
            if !target_exe.is_empty() {
                if let Some(s) = sessions.iter().find(|s| s.exe.to_lowercase() == target_exe) {
                    return s.pid; // same-exe audio child (e.g. the browser's audio service)
                }
            }
            target // fallback: keep the picked PID and INCLUDE its process tree
        })
        .ok()
        .and_then(|h| h.join().ok())
        .unwrap_or(target)
}

fn build_sources(exclude_pid: u32) -> Vec<AudioApp> {
    let mut out = enumerate_windows(exclude_pid); // EnumWindows (no COM needed)
    let win_exes: HashSet<String> = out.iter().map(|w| w.exe.to_lowercase()).collect();
    // Add audio sessions whose exe doesn't already have a window (so we don't list
    // both "chrome" the audio-service session AND its windows).
    for s in enumerate_sessions(exclude_pid) {
        if !win_exes.contains(&s.exe.to_lowercase()) {
            out.push(s);
        }
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

fn enumerate_sessions(exclude_pid: u32) -> Vec<AudioApp> {
    let mut apps: Vec<AudioApp> = Vec::new();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let result = (|| -> windows::core::Result<()> {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
            let mgr: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
            let sessions: IAudioSessionEnumerator = mgr.GetSessionEnumerator()?;
            let count = sessions.GetCount()?;
            for i in 0..count {
                let ctrl = match sessions.GetSession(i) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let ctrl2: IAudioSessionControl2 = match ctrl.cast() {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                if let Ok(state) = ctrl2.GetState() {
                    if state == AudioSessionStateExpired {
                        continue;
                    }
                }
                let pid = match ctrl2.GetProcessId() {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if pid == 0 || pid == exclude_pid {
                    continue; // PID 0 = system sounds; also drop our own process
                }
                if apps.iter().any(|a| a.pid == pid) {
                    continue;
                }
                let exe = process_name(pid).unwrap_or_else(|| format!("PID {pid}"));
                apps.push(AudioApp {
                    pid,
                    name: exe.clone(),
                    exe,
                    hwnd: None, // audio-only session: no window to capture video from
                });
            }
            Ok(())
        })();
        if let Err(e) = result {
            tracing::debug!("audio session enumeration failed: {e}");
        }
        // No explicit CoUninitialize: this runs on the short-lived `source-enum` thread,
        // which exits immediately after returning, and Windows tears down its COM apartment
        // on thread exit. An explicit CoUninitialize here did a synchronous teardown that
        // could DEADLOCK whenever the caller blocked the GUI/STA thread on our join().
    }
    apps
}

/// List visible top-level windows with a title (for "share this window/app").
fn enumerate_windows(exclude_pid: u32) -> Vec<AudioApp> {
    let mut ctx = EnumCtx {
        exclude_pid,
        out: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut ctx as *mut EnumCtx as isize));
    }
    ctx.out
}

struct EnumCtx {
    exclude_pid: u32,
    out: Vec<AudioApp>,
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // Never let a panic unwind across the EnumWindows (C) callback boundary.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = &mut *(lparam.0 as *mut EnumCtx);

        if !IsWindowVisible(hwnd).as_bool() {
            return;
        }
        // Skip owned windows (dialogs, tooltips, palettes) — top-level only.
        if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
            if !owner.0.is_null() {
                return;
            }
        }
        // Skip cloaked windows (UWP background, other virtual desktops).
        let mut cloaked: u32 = 0;
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        );
        if cloaked != 0 {
            return;
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return;
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        // Guard the slice: n is chars copied; never let it exceed the buffer (defends
        // against a title that grew between the length query and this read).
        if n <= 0 || n as usize >= buf.len() {
            return;
        }
        let title = String::from_utf16_lossy(&buf[..n as usize]);
        let title = title.trim();
        if title.is_empty() || title == "Program Manager" {
            return;
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32));
        if pid == 0 || pid == ctx.exclude_pid {
            return;
        }
        let exe = process_name(pid).unwrap_or_default();
        // Trim very long titles so the combo box stays tidy.
        let mut label: String = if title.chars().count() > 56 {
            let mut s: String = title.chars().take(55).collect();
            s.push('…');
            s
        } else {
            title.to_string()
        };
        if !exe.is_empty() {
            label = format!("{label} — {exe}");
        }
        ctx.out.push(AudioApp {
            pid,
            name: label,
            exe,
            hwnd: Some(hwnd.0 as isize), // captured here so SCREEN VIDEO can grab just this window
        });
    }));
    TRUE
}

/// Friendly name for a PID = its executable's file stem (e.g. "Spotify", "chrome").
fn process_name(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut size = buf.len() as u32;
        let res = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        res.ok()?;
        let full = String::from_utf16_lossy(&buf[..size as usize]);
        Path::new(&full)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
    }
}
