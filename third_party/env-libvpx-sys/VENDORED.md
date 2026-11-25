# Vendored copy of env-libvpx-sys 5.1.3

This directory is the `env-libvpx-sys` crate, version 5.1.3, as published on crates.io
(upstream commit `25fefef3760b98fc9920d4bb81db7582d4b23f72` of
<https://github.com/astraw/env-libvpx-sys>), with ONE change: the optional `bindgen`
build-dependency in `Cargo.toml` is `0.72.1` instead of `0.68.1`. No other file is modified.
The workspace selects it through `[patch.crates-io]` in the root `Cargo.toml`.

## Why it is here

The `vp9-bindgen` feature makes this crate generate its libvpx bindings with bindgen. bindgen
releases before 0.72.1 misread C headers under libclang 21 and newer: the structs that another
declaration names before they are defined — in libvpx, `vpx_codec_enc_cfg` and `vpx_codec_dec_cfg`,
both used as `const struct ... *` in `vpx_codec.h` — are emitted as empty opaque types
(`pub _address: u8`). bindgen reports no error; the build fails
later, in `crates/desktop/src/video/vp9.rs`, with `no field g_w on type vpx_codec_enc_cfg`.

Measured with these exact headers (libvpx 1.14.1), same builder options as `build.rs`:

| bindgen | libclang 19 | libclang 22 |
| ------- | ----------- | ----------- |
| 0.68.1  | correct     | opaque      |
| 0.69.5  |             | opaque      |
| 0.71.1  |             | opaque      |
| 0.72.0  |             | opaque      |
| 0.72.1  | correct     | correct     |

GitHub's `ubuntu-latest` runner (Ubuntu 26.04) ships libclang 21 and showed the same opaque
struct with 0.68.1.

Upstream was archived in November 2023, with 5.1.3 as its last release, so there is no newer
version to depend on and a Cargo dependency cannot have its own build-dependency overridden
across an incompatible version. Hence the copy.

The pre-generated bindings under `generated/` (what the plain `vp9` feature uses, on Windows and
for the macOS package) are byte-identical to upstream and are not affected by any of this.
`.gitattributes` turns off line-ending conversion for `third_party/` to keep it that way.

## Removing it

Delete this directory, the `[patch.crates-io]` entry and the `exclude` line in the root
`Cargo.toml` once the VP9 bindings come from somewhere maintained.

## License

MPL-2.0, as upstream (see `Cargo.toml`); the text is at <https://mozilla.org/MPL/2.0/>.
