// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! The Linux source picker — the counterpart to `sessions.rs` on Windows.
//!
//! It deliberately exposes the same two symbols (`AudioApp`, `list_sources`) so `gui.rs` can drive
//! one picker on both platforms. What it CANNOT do is promise the same semantics, and the
//! difference is worth stating plainly because it shapes the UI:
//!
//! * **Windows** enumerates every titled window plus every audio session. An app is listed whether
//!   or not it is making a sound, and its PID is stable for the life of the process.
//! * **Linux** can only enumerate live playback streams (PulseAudio "sink inputs"). An application
//!   that is not playing *does not exist* to this API. Entries therefore appear when playback
//!   starts and vanish when it stops or pauses.
//!
//! The tempting fix — pad the list from `/proc` so it looks like the Windows one — is a trap. It
//! produces a stable, familiar-looking picker in which most entries cannot actually be captured,
//! and the failure only shows up after the operator has picked one and gone live.
//!
//! `hwnd` is always `None`: X11/Wayland windows have no relationship to audio streams, so these
//! entries are never candidates for per-window VIDEO capture.

use super::pulse::{self, SinkInput};

/// A selectable capture source. Field-for-field identical to the Windows `AudioApp` so the GUI's
/// picker code is shared; see the module docs for where the *meaning* diverges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioApp {
    pub pid: u32,
    pub name: String,
    pub exe: String,
    /// Always `None` on Linux — there is no window↔audio mapping to report.
    pub hwnd: Option<isize>,
}

/// Every application currently producing audio, minus our own process.
///
/// Returns an empty list rather than an error when nothing is playing: to the picker, "nothing is
/// making a sound" and "I could not ask" should not look the same, but neither is a failure worth
/// tearing the GUI down over — the caller renders the empty state and the operator presses play.
pub fn list_sources(exclude_pid: u32) -> Vec<AudioApp> {
    let inputs = match pulse::list_sink_inputs() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("could not list audio applications: {e:#}");
            return Vec::new();
        }
    };
    map_sources(inputs, exclude_pid)
}

/// The pure half of [`list_sources`], split out so the filtering rules can be tested without a
/// running sound server.
fn map_sources(inputs: Vec<SinkInput>, exclude_pid: u32) -> Vec<AudioApp> {
    let mut out: Vec<AudioApp> = Vec::new();
    let mut skipped_anonymous = 0usize;
    for si in inputs {
        let Some(pid) = si.pid else {
            // No `application.process.id` — common for native-PipeWire and JACK clients. The
            // picker addresses apps by PID, so such a stream cannot be selected. Counted and
            // logged rather than silently dropped, so "my app isn't in the list" is diagnosable.
            skipped_anonymous += 1;
            continue;
        };
        if pid == exclude_pid {
            continue; // our own playback, if we ever have any
        }
        // One process can hold several streams (a browser with two noisy tabs). The picker is
        // keyed by PID and capture attaches to that process's first stream, so collapse them
        // rather than listing the same app twice with no way to tell the entries apart.
        if out.iter().any(|a| a.pid == pid) {
            continue;
        }
        out.push(AudioApp { pid, name: si.label(), exe: si.binary.clone(), hwnd: None });
    }
    if skipped_anonymous > 0 {
        tracing::debug!(
            "{skipped_anonymous} audio stream(s) hidden from the picker: no application.process.id"
        );
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

#[cfg(test)]
mod tests {
    use super::{map_sources, SinkInput};

    fn si(index: u32, pid: Option<u32>, name: &str, binary: &str) -> SinkInput {
        SinkInput { index, sink: 0, pid, name: name.into(), binary: binary.into() }
    }

    #[test]
    fn lists_one_entry_per_playing_application() {
        let got = map_sources(
            vec![si(1, Some(10), "Firefox", "firefox"), si(2, Some(11), "mpv", "mpv")],
            999,
        );
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "Firefox"); // sorted case-insensitively
        assert_eq!(got[1].name, "mpv");
        // Linux entries are never candidates for per-window video capture.
        assert!(got.iter().all(|a| a.hwnd.is_none()));
    }

    #[test]
    fn collapses_several_streams_from_one_process() {
        // A browser playing two tabs is two sink inputs but one selectable app — listing it twice
        // would give the operator two identical rows with no way to tell them apart.
        let got = map_sources(
            vec![si(1, Some(10), "Firefox", "firefox"), si(2, Some(10), "Firefox", "firefox")],
            999,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].pid, 10);
    }

    #[test]
    fn hides_streams_that_cannot_be_addressed() {
        // No application.process.id (native-PipeWire / JACK clients): the picker selects by PID, so
        // such a stream could be shown but never actually captured. Better absent than broken.
        let got = map_sources(vec![si(1, None, "some jack client", "")], 999);
        assert!(got.is_empty());
    }

    #[test]
    fn never_offers_our_own_playback() {
        let got = map_sources(vec![si(1, Some(999), "Newfoundsync", "newfoundsync")], 999);
        assert!(got.is_empty());
    }

    #[test]
    fn falls_back_through_the_identity_properties() {
        // Measured on real hardware: `paplay` reports application.name="paplay" but
        // application.process.binary="pacat" (it is a symlink), so the two disagree even in the
        // trivial case. Prefer the friendly name, fall back to the binary, then to the index —
        // an unlabelled row is still better than a missing app.
        let got = map_sources(
            vec![
                si(1, Some(10), "", "pacat"),
                si(2, Some(11), "", ""),
                si(3, Some(12), "  ", "spotify"),
            ],
            999,
        );
        let names: Vec<&str> = got.iter().map(|a| a.name.as_str()).collect();
        assert!(names.contains(&"pacat"), "{names:?}");
        assert!(names.contains(&"spotify"), "{names:?}");
        assert!(names.iter().any(|n| n.contains("audio stream #2")), "{names:?}");
    }
}
