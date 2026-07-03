# Code Review Fixes — AV1 + VP9 video-codec reshape (`f0cac4c` + working tree)

Scope of this review: the **AV1+VP9 reshape** — the 5 commits ahead of
`origin/main` (`9bf7f11..f0cac4c`) **plus** the uncommitted working-tree changes.
That work removed HEVC + H.264 (openh264) + the GPU zero-copy (D3D11
VideoProcessor) path and made the server encode **AV1-only** (GPU AV1 via Media
Foundation where the GPU supports it, else CPU SVT-AV1) with a **VP9** (libvpx,
CPU) fallback selectable via `--encoder`. The browser web-cast **H.264 relay**
was intentionally kept.

This is a worklist for another Claude Code session. Every item below was
**adversarially verified against the live source** (a review agent found it, a
second agent tried to refute it against the current file). Line numbers are
**live** as of `f0cac4c` + the working tree, not diff line numbers.

**Headline:** the reshape is clean. **Nothing here breaks the build and nothing
here breaks video on the mainstream WebCodecs browsers (Chrome/Edge/Firefox/
Safari).** There are **no Critical or High** findings — every item is Medium or
Low polish/robustness/docs. Ship-blocking? No. Worth fixing before you call the
project "royalty-free AV1/VP9, correct"? Yes.

**How to use this file:** work top-to-bottom (severity order). Each item has a
precise anchor, what's wrong, the concrete fix, and an acceptance check. Tick the
checkbox when done. See **Build & verify** at the bottom before you compile —
`app.js` is embedded into the binary at build time, and the VP9 build needs
vcpkg/libvpx wired up.

> **Update — all 8 items applied & independently verified.** Every FIX below is
> implemented; the tree **compiles and passes tests** (release build `exit 0` in ~3m11s;
> 30 tests pass incl. a new `codec_strings_cover_every_preset`). FIX-1 now derives the
> AV1/VP9 level from the negotiated resolution+fps via `av1_codec_string` /
> `vp9_codec_string` in `crates/core/src/video.rs`, and `MediaConfig.video_codec` became a
> `String`. A follow-up multi-agent pass re-derived both level tables against the AV1/VP9
> spec Annex A and confirmed **no preset understates** the stream. The **GPU-only**
> acceptance checks for FIX-3 and the GPU half of FIX-4 can't be exercised on this Vega box
> (it uses the CPU SVT-AV1 path); the code is in and compiles — confirm on a machine with a
> hardware AV1 encoder.

---

## Verified clean (do NOT re-investigate — these were checked and are fine)

So the next session doesn't burn time re-deriving what the review already ruled
out:

- **Build integrity after the `Cargo.toml` feature removal.** The working tree
  drops the `windows` features `Win32_Graphics_Direct3D`, `Direct3D11`, `Dxgi`,
  `Dxgi_Common`, and `Win32_Media_KernelStreaming`. A full grep of
  `crates/desktop/src` for `Direct3D | Dxgi | DXGI | ID3D11 | D3D11 |
  VideoProcessor | KernelStreaming | Texture2D | ColorSpace1 | IDXGI` returns
  **no matches** — no dangling symbol, the build does not break from the feature
  removal.
- **Bitstream keyframe detectors are correct for the shipped encoders.**
  `obu_has_av1_keyframe` (AV1 low-overhead OBU: `obu_type = (b>>3)&0xf`, LEB128
  size skip) and `vp9_frame_is_keyframe` (VP9 profile-0 uncompressed header:
  `frame_marker`, `show_existing_frame`, `frame_type` bits) both parse the bit
  layout correctly, and the unit tests assert the right cases. (See FIX-3 for the
  one *robustness* caveat on the GPU path — that is a heuristic-strength concern,
  not a bit-parse bug.)
- **libvpx / VP9 FFI memory management.** `Vp9Encoder` allocates the image via
  `vpx_img_alloc` and frees it (`vpx_img_free` + `vpx_codec_destroy`) in `Drop`;
  altref and lag are disabled so there is no superframe-index complication; the
  per-plane `copy_plane` stride math is correct.
- **libvpx version story.** `env-libvpx-sys` pre-gen 1.13.0 bindings vs
  `vcpkg.json` pinning libvpx 1.13.1 vs `VPX_VERSION=1.13.0` is **ABI-safe** —
  1.13.1 is a security-patch release over 1.13.0 with no encoder ABI change. The
  author reports it links (no `crt-static` needed). Leave the pin as-is.
- **`--encoder` / GUI codec picker / EncoderBackend** wiring is consistent
  (`av1` default, `vp9` fallback; aliases resolve; the GUI picker sets the
  backend that `VideoEncoder::new` consumes).

---

## Priority 1 — Medium

### [x] FIX-1 · Advertised AV1/VP9 codec-string **levels** understate the stream resolution

**Status: CONFIRMED.** This is the most substantive item and it re-introduces the
exact bug class the H.264 cast path was deliberately fixed for.

The server advertises a **hardcoded** codec string to clients, independent of the
negotiated resolution/fps:

- **AV1:** `av01.0.04M.08` — the `04` is `seq_level_idx = 4` = **AV1 level 3.0**
  (major `= 2 + (idx>>2)`, minor `= idx&3`). Level 3.0 `MaxPicSize` is 665,856
  luma samples. **Every** preset exceeds it: 720p = 921,600; **1080p (the
  default)** = 2,073,600; 1440p = 3,686,400; 2160p = 8,294,400.
- **VP9:** `vp09.00.10.08` — the `10` is VP9 level `1.0` (level ×10), a
  ~256×144-class level, for streams up to 4K60.

The real bitstream carries the correct, higher level (SVT-AV1 / the MF encoder /
libvpx auto-select `seq_level_idx` from the resolution), so the advertised string
**understates the stream by several levels across the whole supported range.**

Contrast the H.264 cast path, which deliberately computes a resolution-adequate
level in `avcCodecString()` ([app.js:601](crates/desktop/web/app.js:601)) —
its own comment says a hardcoded-low level *"would be rejected by
isConfigSupported and silently drop the cast to audio-only."* The AV1/VP9 strings
reintroduce that mistake.

**Why this is Medium, not High:** on the mainstream WebCodecs browsers this is
currently **benign** — `isConfigSupported` (app.js:1202) and `configure`
(app.js:1311-1315) are called **without** `codedWidth`/`codedHeight`, so no
browser validates the declared level against a resolution, and AV1/VP9 decode
reads the true parameters from the self-describing bitstream. It becomes a real
audio-only drop only on a stricter/future decoder that honors the level, or if
someone later adds coded dimensions to the probe. It is a genuine
spec-conformance / consistency defect regardless.

**Anchors (all live):**
- [crates/desktop/src/media.rs:281](crates/desktop/src/media.rs:281) — `"vp09.00.10.08"`
- [crates/desktop/src/media.rs:283](crates/desktop/src/media.rs:283) — `"av01.0.04M.08"`
- [crates/desktop/web/app.js:1202](crates/desktop/web/app.js:1202) — client probe fallback `"av01.0.04M.08"`
- [crates/desktop/web/app.js:1267](crates/desktop/web/app.js:1267) — client `onVideoChunk` fallback `"av01.0.04M.08"`
- `video_codec` is a `&'static str` in a struct literal (media.rs:278-284).

**Fix (pick one):**

- **Minimal (blanket-safe):** bump the two server constants to a level that
  covers the max preset (4K60). A receiver that honors the level only requires
  `declared >= real`, so a high blanket level is always safe:
  - AV1 → `"av01.0.13M.08"` (level 5.1)
  - VP9 → `"vp09.00.51.08"` (level 5.1)
  - Update the two client fallbacks (app.js:1202, app.js:1267) to match, or
    better, delete the fallbacks' reliance on a magic string by trusting
    `cfg.videoCodec` (the server always sends it).
- **Better (accurate):** compute the level from the negotiated resolution+fps the
  way `avcCodecString()` already does for H.264. Add an
  `av1_codec_string(res, fps)` / `vp9_codec_string(res, fps)` helper and change
  `MediaConfig.video_codec` from `&'static str` to `String`. Reference mapping:
  AV1 `seq_level_idx` 8 = L4.0 (≤1080p60/2K30), 12 = L5.0 (4K30), 13 = L5.1
  (4K60); VP9 `40` = L4.0, `51` = L5.1.

**Acceptance:** for every preset (720p…2160p, 30/60), the advertised `videoCodec`
level `>=` the level the encoder actually writes into the bitstream, and
`VideoDecoder.isConfigSupported({codec})` returns `supported: true` on
Chrome/Edge/Firefox with no spurious "can't decode the video codec" warning.

---

### [x] FIX-2 · README claims the **SVT-AV1** encoder "uses your GPU" — it's CPU-only

**Status: CONFIRMED.** Docs-only, but a clear factual error a maintainer/user
would be misled by.

[README.md:106-107](README.md:106) reads:

> "Video is **AV1** by default — the SVT-AV1 encoder ships as a prebuilt library
> (no extra setup) and uses your GPU's hardware AV1 encoder when it has one."

This conflates the two **distinct** AV1 backends. SVT-AV1
(`shiguredo_svt_av1`, `Av1Encoder` in
[codec.rs](crates/desktop/src/video/codec.rs:29)) is **strictly CPU/software**.
The GPU hardware-AV1 path is a *separate* backend — Media Foundation
(`MfEncoder::new_av1` in
[mf_encoder.rs](crates/desktop/src/video/mf_encoder.rs)). `VideoEncoder::new`
([codec.rs:181-197](crates/desktop/src/video/codec.rs:181)) tries the MF GPU
encoder **first** and only falls back to SVT-AV1 on the CPU when there is no GPU
AV1 encoder. SVT-AV1 never touches the GPU.

Everywhere else in the tree the split is stated correctly
([core/src/video.rs:111-113](crates/core/src/video.rs:111);
[index.html:329](index.html:329)) — only this README sentence contradicts the
code.

**Fix:** reword README:106-107 to separate the two backends, e.g.:

> "Video is **AV1** by default. Where your GPU has a hardware AV1 encoder, the
> server uses it via Media Foundation; otherwise it falls back to the CPU
> **SVT-AV1** encoder, which ships as a prebuilt static library (no extra setup)."

**Acceptance:** README no longer implies SVT-AV1 does GPU encoding; the
GPU=Media Foundation / CPU=SVT-AV1 split reads consistently with
`VideoEncoder::new`.

---

### [x] FIX-3 · AV1 keyframe detection is a **sequence-header proxy**, not a `frame_type` parse (GPU-path robustness)

**Status: PLAUSIBLE — needs hardware to confirm.** Do not treat this as a proven
bug; treat it as defensive hardening for the GPU AV1 path, which cannot be
exercised on this box (Vega iGPU → CPU path).

`obu_has_av1_keyframe`
([codec.rs:264](crates/desktop/src/video/codec.rs:264)) returns `true` the
instant it sees an OBU of type 1 (Sequence Header) and **never inspects the frame
header's `frame_type`**. This is the *sole* keyframe signal for the GPU path:
`MfEncoder::force_keyframe()` is a no-op
([mf_encoder.rs:187](crates/desktop/src/video/mf_encoder.rs:187)) and the server
derives the wire keyframe flag purely from the emitted bytes
(`enc.is_keyframe(&bits)`,
[media.rs:472](crates/desktop/src/media.rs:472)). The heuristic is wrong in
principle in *both* directions:

- **False negative (the risk that matters):** if a given hardware AV1 MFT ever
  emits the sequence header out-of-band / only once at stream start rather than
  inline with every keyframe TU, `is_keyframe` returns `false` for real
  keyframes, the client discards every chunk (it waits for a key to configure
  from — [app.js:1265-1266](crates/desktop/web/app.js:1265)), and video stays
  **permanently black** on that GPU. *(Mitigating reality: a raw low-overhead OBU
  stream fed to WebCodecs with no out-of-band description is only decodable if the
  seq header rides inline with each keyframe — so a non-repeating MFT would
  already be broken more visibly. This is why it's PLAUSIBLE, not CONFIRMED.)*
- **False positive:** a mid-stream sequence header on a non-key TU would be
  mislabeled key. Does not occur with the currently-wired SVT-AV1 (low-delay RTC,
  `scene_change_detection = false`) or MF AV1, which only emit the seq header at
  GOP keyframes.

**Fix (defensive):** don't rely on OBU-scanning alone for the GPU path. In
`MfEncoder`'s output drain, read the per-sample sync marker
`sample.GetUINT32(&MFSampleExtension_CleanPoint)` and thread that flag up
alongside the bytes (OR it with the OBU heuristic). Optionally make
`obu_has_av1_keyframe` spec-correct too: walk to the Frame/Frame-Header OBU
(type 6/3) and return true only when `show_existing_frame == 0 && frame_type ==
KEY_FRAME`, mirroring `vp9_frame_is_keyframe`. Keep the OBU detector as the
CPU-path primary.

**Acceptance:** on a machine with a hardware AV1 encoder (Intel Arc / NVIDIA
RTX40+ / AMD RX7000+), a browser joining >2 s after stream start shows video
within one GOP (~2 s), and recovers after an induced decode error instead of
staying black. If you harden the OBU parser, add unit tests: (seq-header + INTER
frame) → false; (KEY_FRAME, no seq header) → true.

---

## Priority 2 — Low

### [x] FIX-4 · `"video encoder ready (CPU path)"` is logged even when the **GPU** AV1 MFT is active

**Status: CONFIRMED.** Diagnostic-only.

[media.rs:438](crates/desktop/src/media.rs:438) logs
`tracing::info!(backend = e.backend_label(), "video encoder ready (CPU path)")`
with a hardcoded `(CPU path)` literal. After the reshape `backend_label()` can be
`"GPU AV1 (Media Foundation)"`, so on a GPU-AV1 box the line self-contradicts
(`backend="GPU AV1 (Media Foundation)" … "video encoder ready (CPU path)"`) and
misleads anyone diagnosing whether hardware AV1 engaged.

**Fix:** drop the `(CPU path)` literal — the structured `backend` field already
reports CPU vs GPU: `tracing::info!(backend = e.backend_label(), "video encoder ready");`

**Acceptance:** on a hardware-AV1 box the line no longer says "CPU path"; the
`backend` field reads "GPU AV1 (Media Foundation)".

---

### [x] FIX-5 · README `--capture` flag table omits the `web` (web-uplink cast) source

**Status: CONFIRMED.** Docs-completeness.

The `--capture` row ([README.md:157](README.md:157)) lists only `allapps ·
system · app`. But `main.rs` parses a fourth source — `web` (aliases
`uplink`/`cast`) → `CaptureSource::WebUplink`
([main.rs:100-110](crates/desktop/src/main.rs:100)) — and the flag's own help
string ([main.rs:72](crates/desktop/src/main.rs:72)) documents it. So
`--headless --capture web` is a real, supported mode the README table hides.

**Fix:** append `web` to the README:157 row, e.g. `· web (a web client casts
audio/video up to this server)`, matching the `main.rs` help text and parser.

**Acceptance:** the README `--capture` row lists all four sources `main.rs`
accepts (`allapps`, `system`, `app`, `web`).

---

### [x] FIX-6 · `Cargo.toml` rayon comment lists a phantom `BGRA→RGB` conversion

**Status: CONFIRMED.** Comment accuracy.

[crates/desktop/Cargo.toml:32](crates/desktop/Cargo.toml:32) reads:
`# Data-parallel pixel conversions (BGRA→NV12 / BGRA→RGB / scaling) across cores.`
There is **no** BGRA→RGB conversion anywhere in the desktop crate (grep for
`rgb` finds only egui theming). The real rayon-parallel conversions after the
reshape are **BGRA→NV12** (`mf_encoder.rs`, GPU path) and **BGRA→I420**
(`codec.rs::bgra_to_i420`, used by *both* SVT-AV1 and VP9) plus `scale_bgra`
(`media.rs`). The comment names one that doesn't exist and omits the I420 path
that is now the dominant CPU conversion.

**Fix:** `# Data-parallel pixel conversions (BGRA→I420 for SVT-AV1/VP9, BGRA→NV12
for the MF GPU encoder) and frame scaling across cores.`

**Acceptance:** the comment names only conversions present in the code.

---

### [x] FIX-7 · `IMFActivate` entries leak on the null-activate error path in `new_av1`

**Status: PLAUSIBLE (defensive; not reachable in practice).** COM-ref hygiene.

In `new_av1`, when `slice[0].as_ref()` is `None`
([mf_encoder.rs:73-76](crates/desktop/src/video/mf_encoder.rs:73)) the code
`CoTaskMemFree`s the array and bails **without** releasing the remaining
`Option<IMFActivate>` entries at `1..count`. The success path
([mf_encoder.rs:79-82](crates/desktop/src/video/mf_encoder.rs:79)) correctly
`ptr::read`s every entry to release it before freeing. The asymmetry would leak a
COM ref per trailing entry — but `MFTEnumEx` returns a densely packed array (no
null slot 0 followed by valid entries), so the branch is effectively dead, and
this runs once at init with a software fallback behind it. Fix it for symmetry,
not urgency.

**Fix:** before the `CoTaskMemFree` on the null-activate path, run the same
`for i in 0..count { let _ = std::ptr::read(activates.add(i)); }` release loop the
success path uses. Factor the release-then-free into a shared helper so the two
paths can't diverge again.

**Acceptance:** static review — both the success and null-activate paths release
all enumerated `IMFActivate` entries exactly once before freeing the array.

---

### [x] FIX-8 · Periodic-keyframe cadence counts **emitted** outputs, not captured frames / wall time

**Status: PLAUSIBLE (self-correcting; bounded by the encoders' own GOP).**

The producer computes `periodic = frame_count % key_interval == 0`
([media.rs:456](crates/desktop/src/media.rs:456), `key_interval = fps*2`) but
`frame_count` is incremented **only** inside the `Ok(bits) if !bits.is_empty()`
arm ([media.rs:479](crates/desktop/src/media.rs:479)). Encoders with pipeline
latency (the GPU MFT; SVT-AV1's fill phase) return empty `Vec`s for the first N
inputs and on skipped frames, so `frame_count` tracks *emitted access units*, not
captured frames or wall time. The intended "keyframe every ~2 s" therefore drifts
during ramp-up / under drops.

**Why Low:** both real encoders emit keyframes on their **own** ~2 s GOP anyway
(SVT-AV1 `intra_period_length = fps*2`; the GPU MFT is GOP-driven and treats
`force_keyframe` as a no-op), so the mislabeled cadence only affects the redundant
*force-request* timing, not actual keyframe spacing. Self-corrects at steady
state.

**Fix:** drive the periodic keyframe request off elapsed wall-clock (an `Instant`
since the last keyframe: `now - last_keyframe_at >= KEYFRAME_SECS`) or off a
per-loop-iteration counter, rather than off the successful-output count.

**Acceptance:** with an encoder that returns empty for the first few inputs, the
first periodic keyframe request lands ~2 s after start rather than after `2*fps`
successful outputs.

---

## Build & verify

Do this before you compile — the pipeline has two gotchas:

1. **`app.js` is embedded at build time.** The browser client
   (`crates/desktop/web/app.js`) is baked into the binary. Any FIX-1 change to the
   client fallbacks (app.js:1202/1267) requires a **rebuild** of the desktop
   crate to take effect — editing the file alone does nothing to a running server.
2. **VP9 build needs vcpkg/libvpx.** FIX-1's VP9 string change compiles without
   libvpx, but *running* the `vp9` backend and any VP9 test needs the vcpkg
   libvpx setup from the README build section (`vcpkg install --triplet
   x64-windows-static`, the `vpx.lib → libvpx.lib` alias, and the `VPX_*` env
   vars). AV1-only changes (FIX-2…FIX-8) do **not** need libvpx.

Then:

```powershell
# From the repo root:
cargo build -p newfoundsync            # confirms nothing broke (AV1 path)
cargo test  -p newfoundsync            # keyframe-detector unit tests (hardware-free)
cargo test  -p newfoundsync-core       # sync-core suite
```

- Windows `.exe` file-lock gotcha: stop any running `newfoundsync` before
  rebuilding or the link step fails.
- FIX-3 and the GPU-AV1 half of FIX-4 can only be *fully* verified on a machine
  with a hardware AV1 encoder; on a CPU-only box confirm the CPU-path behavior and
  leave the GPU acceptance checks for hardware.

---

*Generated by an adversarially-verified multi-agent code review (6 review
dimensions + completeness critic, each finding re-checked against the live source
by an independent verifier). 16 findings survived verification and were
deduplicated into the 8 items above; the build-integrity and bitstream-parse
dimensions came back clean (see "Verified clean").*
