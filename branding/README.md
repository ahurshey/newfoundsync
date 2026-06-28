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
- `icon-16/32/48/64/128/256.png` — downscaled size variants.
- `icon.ico` — multi-size (16–256), PNG-encoded entries (Windows 10/11 reads
  these natively).

At icon sizes the curved wordmark is invisible, so the small icons read as the
round mark (blue disc + Newfoundland); the larger sizes show the full emblem.

## Where they're used

- **Windows .exe icon** (Explorer / Start menu): `icon.ico`, embedded into
  `newfoundsync.exe` by `crates/desktop/build.rs` via `winresource`.
- **Window + taskbar icon**: `icon-256.png`, decoded at runtime in
  `crates/desktop/src/ui.rs` (`icon_rgba`) and set on the egui viewport.
- **Tray icon**: same `icon-256.png` (`crates/desktop/src/tray.rs`), with a
  procedural teal-square fallback if it ever fails to load.
- **In-app branding**: the emblem texture in the egui header (26 px) and on the
  idle landing screen (112 px).
