# Branding assets

The source logo lives here; every icon form is generated from it.

## Source (hand-provided)

- **`newfoundsync-logo.png`** — the full circular emblem (Newfoundland porthole
  with the "NEWFOUNDSYNC" wordmark curved in the ring), 1254×1254 on a cream
  canvas. Replace this and re-run the generator to re-skin the app.

## Generate

```powershell
./gen-icons.ps1
```

`gen-icons.ps1` color-keys the emblem off the cream canvas, crops it square,
masks the corners transparent (feathered circle), and emits:

- `icon-source.png` — 512 master, transparent corners.
- `icon-16/32/48/64/128/256/512.png` — downscaled size variants.
- `icon-512-maskable.png` — the PWA "maskable" companion to `icon-512.png` (extra padding so
  Android's shape mask can't clip the emblem). Both are served by the web client's manifest.
- `icon.ico` — multi-size (16–256), PNG-encoded entries (Windows 10/11 reads
  these natively).

At icon sizes the curved wordmark is invisible, so the small icons read as the
round mark (blue disc + Newfoundland); the larger sizes show the full emblem.

## Where they're used

- **Windows .exe icon** (Explorer / Start menu): `icon.ico`, embedded into
  `newfoundsync.exe` by `crates/desktop/build.rs` via `winresource`.
- **Window + taskbar icon**: `icon-256.png`, decoded at startup via
  `eframe::icon_data::from_png_bytes` in `crates/desktop/src/gui.rs` and set on the
  egui viewport.
- **Web client**: `icon-32` / `-128` / `-256` / `-512` / `-512-maskable` are compiled
  into the server (`crates/desktop/src/webserver.rs`) and served as the favicon and
  the PWA manifest icons — so a browser tab and an "add to home screen" shortcut
  both carry the mark.

There is **no tray icon**: the app has no system-tray integration. (This section
previously described a `tray.rs` and a `ui.rs` that do not exist.)
