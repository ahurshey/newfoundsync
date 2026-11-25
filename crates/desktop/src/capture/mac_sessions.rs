// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! The macOS source picker — the counterpart to `sessions.rs` (Windows) and `linux_sessions.rs`.
//!
//! Same two symbols (`AudioApp`, `list_sources`) so `gui.rs` drives one picker everywhere. The
//! list comes from CoreAudio's process objects (`kAudioHardwarePropertyProcessObjectList`): every
//! process that has opened audio, playing or not.
//!
//! Apps rarely play from their own process. Chrome and Electron apps play from a helper, Safari
//! and other WebKit apps from a `com.apple.WebKit.GPU` process. Tapping the main app's PID alone
//! would capture silence, so entries are grouped by the process macOS holds *responsible* for each
//! one (the app the user launched), and a capture taps every process object in the group.

use std::ffi::c_void;
use std::path::Path;
use std::ptr::NonNull;

use anyhow::{bail, Result};
use objc2_core_audio::{
    kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioProcessPropertyBundleID,
    kAudioProcessPropertyIsRunningOutput, kAudioProcessPropertyPID, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress,
};

use super::mac_tap::{get_prop, get_string_prop};

/// A selectable capture source. Field-for-field the Windows/Linux `AudioApp`. `pid` is the app the
/// user launched (the responsible process); `hwnd` is always `None`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioApp {
    pub pid: u32,
    pub name: String,
    pub exe: String,
    pub hwnd: Option<isize>,
}

extern "C" {
    fn proc_pidpath(pid: i32, buffer: *mut c_void, buffersize: u32) -> i32;
    /// Private but long-stable libSystem export (Chromium and the audio-tap samples use it): the
    /// process macOS attributes another one to, e.g. Chrome for "Google Chrome Helper".
    fn responsibility_get_pid_responsible_for_pid(pid: i32) -> i32;
}

/// One CoreAudio process object.
struct Proc {
    object: AudioObjectID,
    pid: i32,
    owner: i32,
    playing: bool,
}

fn exe_path(pid: i32) -> Option<String> {
    let mut buf = vec![0u8; 4096];
    let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    String::from_utf8(buf).ok()
}

/// The outermost `.app` bundle in a path ("Google Chrome" for Chrome's nested helper apps).
fn app_name(path: &str) -> Option<String> {
    path.split('/').find_map(|c| c.strip_suffix(".app")).map(str::to_string)
}

fn owner_of(pid: i32) -> i32 {
    let r = unsafe { responsibility_get_pid_responsible_for_pid(pid) };
    if r > 0 {
        r
    } else {
        pid
    }
}

fn process_objects() -> Vec<Proc> {
    let a = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyProcessObjectList,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let sys = kAudioObjectSystemObject as AudioObjectID;
    let mut size: u32 = 0;
    let st = unsafe { AudioObjectGetPropertyDataSize(sys, NonNull::from(&a), 0, std::ptr::null(), NonNull::from(&mut size)) };
    if st != 0 || size == 0 {
        if st != 0 {
            tracing::warn!("could not list audio processes (OSStatus {st}); process taps need macOS 14.2+");
        }
        return Vec::new();
    }
    let mut ids = vec![0 as AudioObjectID; size as usize / std::mem::size_of::<AudioObjectID>()];
    let st = unsafe {
        AudioObjectGetPropertyData(
            sys,
            NonNull::from(&a),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(ids.as_mut_ptr().cast()),
        )
    };
    if st != 0 {
        tracing::warn!("could not list audio processes (OSStatus {st})");
        return Vec::new();
    }
    ids.truncate(size as usize / std::mem::size_of::<AudioObjectID>());
    ids.into_iter()
        .filter_map(|object| {
            let pid: i32 = get_prop(object, kAudioProcessPropertyPID).ok()?;
            if pid <= 0 {
                return None;
            }
            let playing = get_prop::<u32>(object, kAudioProcessPropertyIsRunningOutput).unwrap_or(0) != 0;
            Some(Proc { object, pid, owner: owner_of(pid), playing })
        })
        .collect()
}

/// Apps that have opened audio, grouped by the app the user launched, minus our own process.
/// Playing ones first. Background daemons with no `.app` are listed only while they play.
pub fn list_sources(exclude_pid: u32) -> Vec<AudioApp> {
    use std::collections::BTreeMap;
    // owner pid -> (any process playing)
    let mut groups: BTreeMap<i32, bool> = BTreeMap::new();
    for p in process_objects() {
        if p.owner as u32 == exclude_pid || p.pid as u32 == exclude_pid {
            continue;
        }
        *groups.entry(p.owner).or_default() |= p.playing;
    }
    let mut out: Vec<(bool, AudioApp)> = Vec::new();
    for (owner, playing) in groups {
        let path = exe_path(owner).unwrap_or_default();
        let name = match app_name(&path) {
            Some(n) => n,
            None if playing => Path::new(&path)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("PID {owner}")),
            None => continue, // a daemon that is not making a sound: noise in the list
        };
        out.push((playing, AudioApp { pid: owner as u32, name, exe: path, hwnd: None }));
    }
    out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase())));
    out.into_iter()
        .map(|(playing, mut a)| {
            if playing {
                a.name.push_str(" (playing)");
            }
            a
        })
        .collect()
}

/// Every CoreAudio process object belonging to the app `pid` (itself and its helpers), for a tap.
/// Errors when the app has not opened audio yet: there is nothing to tap, and quietly tapping
/// something broader would stream more than the operator picked.
pub fn tap_objects_for(pid: u32) -> Result<(Vec<AudioObjectID>, String)> {
    let procs = process_objects();
    let ids: Vec<AudioObjectID> = procs
        .iter()
        .filter(|p| p.owner as u32 == pid || p.pid as u32 == pid)
        .map(|p| p.object)
        .collect();
    let name = exe_path(pid as i32)
        .and_then(|p| app_name(&p).or_else(|| Path::new(&p).file_name().map(|f| f.to_string_lossy().into_owned())))
        .unwrap_or_else(|| format!("PID {pid}"));
    if ids.is_empty() {
        bail!("{name} has not opened audio yet. Start playback in it, then pick it again.");
    }
    for p in procs.iter().filter(|p| p.owner as u32 == pid || p.pid as u32 == pid) {
        tracing::info!(
            object = p.object,
            pid = p.pid,
            bundle = get_string_prop(p.object, kAudioProcessPropertyBundleID).unwrap_or_default(),
            playing = p.playing,
            "[capture] tapping process"
        );
    }
    Ok((ids, name))
}

/// Our own process object, if we have one, so an all-apps tap can leave us out.
pub fn own_objects() -> Vec<AudioObjectID> {
    let me = std::process::id() as i32;
    process_objects().into_iter().filter(|p| p.pid == me).map(|p| p.object).collect()
}

#[cfg(test)]
mod tests {
    use super::app_name;

    #[test]
    fn outermost_app_bundle_names_the_app() {
        assert_eq!(
            app_name("/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Helpers/Google Chrome Helper.app/Contents/MacOS/Google Chrome Helper").as_deref(),
            Some("Google Chrome")
        );
        assert_eq!(app_name("/System/Applications/Music.app/Contents/MacOS/Music").as_deref(), Some("Music"));
        assert_eq!(app_name("/usr/libexec/coreaudiod"), None);
    }
}
