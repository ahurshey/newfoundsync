# Code Review Fixes — commit `7fcf4ff` (web-client cast, codec picker, video pacing, port)

This is a worklist for a Claude Code session. Each item is independently
verified against the **current** source (line numbers are live as of this
writing, not diff line numbers). Nothing here blocks the feature for a
trusted-LAN tool — but the first item ships a control the commit message
already advertises as present.

**How to use this file:** work top-to-bottom (severity order). Each item has a
precise anchor, what's wrong, the concrete fix, and an acceptance check. When a
fix is done, tick its checkbox. See **Build & verify** at the bottom before you
compile — there's a Windows `.exe` file-lock gotcha and `app.js` is embedded at
build time.

---

## Priority 1 — Medium

### [x] FIX-1 · Wire the operator "Stop cast" control (F1 + F2 are one job)

The commit message claims *"one caster … operator stops it via the clients
panel."* That operator stop **does not exist**. `cast_stop_msg()` is defined,
`pub`, and documented "Exposed for the GUI" — but it has **zero call sites**,
and the CONNECTED CLIENTS panel has no stop button. Today a caster can only be
stopped by (a) the caster tapping *Stop cast* themselves, (b) the caster
disconnecting, or (c) the operator switching the source away entirely (which
force-reconnects everyone). There's no per-caster kick.

**F2 is the trap baked into this work:** when you wire the button, freeing the
slot is a *two-part* action. The server is the sole authority on the slot, and
the caster's client-side `stopCast(false)` does **not** send `MSG_CAST_STOP`
back to the server. So pushing `MSG_CAST_STOP` to the caster alone leaves the
server slot occupied forever (until that client disconnects) — no one else can
claim it.

**Anchors:**
- `crates/desktop/src/webserver.rs:190` — `cast_stop_msg()` (currently unused)
- `crates/desktop/src/webserver.rs:453-455` — `ClientGuard::Drop` frees the slot on disconnect
- `crates/desktop/src/webserver.rs:622-624` — `MSG_CAST_STOP` handler frees the slot when the *caster* sends it
- `crates/desktop/src/gui.rs:1031-1136` — CONNECTED CLIENTS panel (no cast control today)
- `crates/desktop/src/gui.rs:95` — the shared `cast: webserver::CastState` (`Arc<Mutex<Option<u64>>>`) the GUI already holds

**Fix:**
1. In the CONNECTED CLIENTS panel (per-client row, ~after the Sync slider,
   `gui.rs:~1124`), render a **"⏹ Stop cast"** button **only** for the client
   whose `conn_id == *cast.lock()` (i.e. the active caster). Mirror the existing
   "Stop calibration" button pattern.
2. On click, do **both**, synchronously:
   - **(a)** push `webserver::cast_stop_msg()` into that caster's control
     channel (the same `ctrl_tx`/`UnboundedSender` path the GUI uses for
     `SET_VOLUME`), so the caster's browser tears down its uplink; **and**
   - **(b)** `*self.cast.lock().unwrap() = None;` (or `if let Ok(mut s) =
     self.cast.lock() { *s = None; }`) to free the server slot immediately.
3. Do **not** wait for the caster to acknowledge — free the slot in the same
   handler.

**Acceptance:** with a caster active, clicking the operator's Stop cast (i) ends
the caster's uplink in the browser, (ii) frees the slot so a *different* client
can immediately claim it, and (iii) leaves no `cast_stop_msg` dead-code warning.

> If you decide *not* to build this now: at minimum, delete `cast_stop_msg()`
> and soften the commit message / any changelog so the operator-stop control
> isn't advertised as present. But wiring it is the better close — it's also the
> only per-caster kick.

---

## Priority 2 — Low

### [x] FIX-2 · Server trusts the caster-supplied keyframe flag (no re-scan)

The relay forwards the caster's keyframe bit verbatim instead of deriving it
from the bitstream — unlike the local capture path, which re-scans via
`enc.is_keyframe()`. A buggy or hostile caster can mislabel keyframes and strand
other receivers on a black frame. Low impact (the caster already fully controls
the shared stream on a trusted LAN), cheap to harden.

**Anchors:**
- `crates/desktop/src/webserver.rs:612-619` — `MSG_UP_VIDEO` handler → `relay.push_video(b[1] != 0, &b[2..])`
- `crates/desktop/src/media.rs:135-143` — `CastRelay::push_video` (encodes the flag verbatim)
- `crates/desktop/src/media.rs:472` — local path re-scans with `enc.is_keyframe(&bits)`
- `crates/desktop/src/video/codec.rs:164` — `fn annexb_has_h264_idr(...)` (currently **private**)

**Fix (web casts are always `avc1`/H.264 — see `media.rs:273-274`):**
1. Make the helper reusable — `codec.rs:164`: `fn annexb_has_h264_idr` → `pub fn annexb_has_h264_idr`.
2. In `CastRelay::push_video` (`media.rs:135`), recompute the flag from the bytes
   and drop reliance on the passed-in `key`:
   ```rust
   pub fn push_video(&self, h264: &[u8]) {
       let key = crate::video::codec::annexb_has_h264_idr(h264);
       let pts = mono_now() + self.lead_ns;
       // …unchanged: build [MSG_VIDEO][pts BE][key u8][bytes]…
   }
   ```
3. Update the call site (`webserver.rs:616`) to `relay.push_video(&b[2..]);`
   (the `b[1]` wire byte from the client is now ignored — leave the wire format
   as-is for client compatibility).

**Acceptance:** a caster that lies in `b[1]` no longer affects receivers'
keyframe gating; keyframe detection matches the local path. Build is clean.

---

### [x] FIX-3 · VideoEncoder error path doesn't release the capture reader

On a `VideoEncoder` error the callback nulls `castVidEnc`, so the read loop
(`while (casting && castVidEnc)`) exits on its *next* iteration — but
`castVidReader` is never cancelled, so the `MediaStreamTrackProcessor` keeps
pulling the video track until `stopCast()` runs. Bounded (cleaned up on stop),
not a growing leak, but the track is held needlessly during audio-only fallback.

**Anchor:** `crates/desktop/web/app.js:2828-2832` — the `VideoEncoder` `error:` callback inside `startVideoCast()`.

**Fix — cancel + null the reader inside the error callback:**
```javascript
error: (e) => {
  try { if (castVidEnc && castVidEnc.state !== "closed") castVidEnc.close(); } catch (x) {}
  castVidEnc = null;
  try { if (castVidReader) castVidReader.cancel(); } catch (x) {}
  castVidReader = null;
  setCastStatus("📡 Casting audio (video encode failed: " + e.message + ") — tap Stop cast to end.");
},
```

**Acceptance:** after a simulated video-encode error, the video track is
released promptly (not held until Stop cast); audio keeps flowing.
> Reminder: `app.js` is embedded via `include_str!` — rebuild to take effect.

---

### [x] FIX-4 · `settings.rs` save is a non-atomic in-place write

`save_key` does `std::fs::write()` straight onto `settings.txt`. A crash
mid-write can truncate/corrupt it. Tiny, single-writer, non-critical — but the
atomic pattern is nearly free.

**Anchor:** `crates/desktop/src/settings.rs:42-51` — `save_key()`, specifically the final `std::fs::write(&path, body)`.

**Fix — write to a sibling temp file, then rename over the target:**
```rust
let body: String = map.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
let tmp_path = path.with_extension("txt.tmp");
std::fs::write(&tmp_path, body).map_err(|e| e.to_string())?;
std::fs::rename(&tmp_path, &path).map_err(|e| e.to_string())
```

**Acceptance:** saving the port still works; an interrupted write can never leave
a partial `settings.txt` (either the old file or the complete new one is present).

---

### [x] FIX-5 · Codec picker silently drops encoder index 1 ("hardware")

The Codec ComboBox hard-codes only `enc_idx` 0 (HEVC/GPU) and 2 (H.264/CPU).
`ENC_LABELS[1]` is `("GPU only", "hardware")` and `--encoder hardware` maps to
`enc_idx == 1`. A config that lands on 1 is (a) unreachable from the GUI and (b)
silently shown as HEVC, because `is_h264 = self.enc_idx == 2` treats 1 as HEVC.
Harmless today, but the mapping is implicit and can mislead.

**Anchor:** `crates/desktop/src/gui.rs:1368-1377` — the `is_h264` computation + the `"codec"` ComboBox.

**Fix (pick one; option A preferred):**
- **A — normalize on load.** After the `enc_idx` initialization (~`gui.rs:180`),
  fold the unexposed index into Auto:
  ```rust
  let enc_idx = if enc_idx == 1 { 0 } else { enc_idx }; // "GPU only" (1) isn't exposed; fold into Auto(0)
  ```
- **B — document intent.** Expand the comment at `gui.rs:1368-1370` to state that
  index 1 (`hardware`, GPU-only) is intentionally not surfaced because this build
  exposes only Auto (0, GPU with CPU fallback) and CPU (2).

**Acceptance:** an `--encoder hardware` launch no longer produces a
GUI/encoder-state mismatch (option A), or the omission is clearly documented
(option B).

---

## Priority 3 — Nit / cosmetic

### [x] FIX-6 · Remove stray double-blank-lines introduced by this commit

**Anchors:**
- `crates/desktop/web/app.js:349` — redundant blank line between the service-worker block and the `// ---- audio visualizer` comment.
- `crates/desktop/web/index.html:159` — redundant blank line between the `#hint` style block and the `/* ---- buffering loading bar` comment.

**Fix:** delete the extra blank line in each (leave a single blank line).

---

## Build & verify (Windows — read before compiling)

- **Stop the running server first.** A release build fails with `exit 101 /
  "Access is denied"` at the link step if `newfoundsync.exe` is running (the
  linker can't overwrite the locked binary):
  ```powershell
  Stop-Process -Name newfoundsync -Force -ErrorAction SilentlyContinue
  cargo build --release -p newfoundsync
  ```
- **`app.js` / `index.html` are embedded via `include_str!`** — client-side
  changes (FIX-3, FIX-6) only take effect after a **rebuild**. Returning browser
  clients also cache via a service worker; hard-reload (or bump the SW cache
  version) when testing client changes.
- **Suggested branch/commit split:** FIX-1 as its own commit (it's a real
  feature); FIX-2…FIX-5 batched as a "review hardening" commit; FIX-6 folded in
  or trivially separate.
- **Regression sanity after building:** with source = *Web client cast*, one
  browser casts (audio, then screen+audio); confirm all clients receive it,
  a second browser is denied the slot while the first holds it, and — once
  FIX-1 lands — the operator can stop the caster and a different client can then
  claim the slot.
