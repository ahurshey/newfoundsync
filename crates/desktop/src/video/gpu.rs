// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! GPU zero-copy BGRA→NV12 color conversion via the D3D11 VideoProcessor.
//!
//! windows-capture creates its D3D11 device with BGRA_SUPPORT but NOT VIDEO_SUPPORT, so a
//! hardware VideoProcessor cannot run on it. We therefore stand up our OWN VIDEO_SUPPORT,
//! multithread-protected device, bridge each captured WGC BGRA texture onto it via a
//! keyed-mutex SHARED texture, and let the VideoProcessor convert+scale BGRA → NV12 on the
//! GPU. The resulting NV12 texture is fed straight to the Media Foundation HEVC encoder as a
//! DXGI surface — eliminating the CPU readback AND the CPU BGRA→NV12 conversion.
//!
//! ALL of this runs on the capture thread, inside `on_frame_arrived` (the only place the WGC
//! texture + its immediate context are valid). Any init failure → the caller falls back to the
//! system-memory CPU path, so video never breaks. Every fallible init step is logged so a
//! fallback on a given GPU is diagnosable.

use anyhow::{bail, Context, Result};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{IDXGIKeyedMutex, IDXGIResource1, DXGI_SHARED_RESOURCE_READ};

const NV12_RING: usize = 3; // round-robin so the encoder's in-flight sample never aliases the next Blt

/// Owns a VIDEO_SUPPORT D3D11 device + a VideoProcessor configured to scale+convert
/// `capture_dims` BGRA → `enc_dims` NV12. Produces GPU NV12 textures ready to encode.
pub struct GpuConverter {
    pub device: ID3D11Device,
    context: ID3D11DeviceContext,
    vctx: ID3D11VideoContext1,
    vproc: ID3D11VideoProcessor,
    venum: ID3D11VideoProcessorEnumerator,
    enc_w: u32,
    enc_h: u32,
    // Lazily-built once the first WGC frame's exact desc is known.
    bridge: Option<Bridge>,
    nv12: Vec<ID3D11Texture2D>,
    nv12_next: usize,
}

/// Cross-device ingest: a keyed-mutex shared BGRA texture owned by OUR device and opened on
/// the WGC device, so we can CopyResource the WGC frame onto it then VideoProcessorBlt it.
struct Bridge {
    cap_w: u32,
    cap_h: u32,
    ours: ID3D11Texture2D,    // shared texture on OUR device
    ours_km: IDXGIKeyedMutex, // its keyed mutex (our side)
    wgc: ID3D11Texture2D,     // same texture opened on the WGC device
    wgc_km: IDXGIKeyedMutex,  // its keyed mutex (WGC side)
    in_view: ID3D11VideoProcessorInputView,
}

impl GpuConverter {
    /// Build the converter. Returns Err on ANY unsupported step → caller uses the CPU path.
    pub fn try_new(enc_w: u32, enc_h: u32) -> Result<GpuConverter> {
        if enc_w % 2 != 0 || enc_h % 2 != 0 {
            bail!("NV12 needs even encode dims ({enc_w}x{enc_h})");
        }
        unsafe {
            // Our own device: BGRA (for the shared BGRA texture) + VIDEO (for the processor).
            let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
            let levels = [D3D_FEATURE_LEVEL_11_0];
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                flags,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice(VIDEO_SUPPORT)")?;
            let device = device.context("no D3D11 device")?;
            let context = context.context("no D3D11 context")?;

            // Hardware MFTs require the device to be multithread-protected.
            let mt: ID3D11Multithread = context.cast().context("ID3D11Multithread")?;
            mt.SetMultithreadProtected(true);

            let vdev: ID3D11VideoDevice = device.cast().context("ID3D11VideoDevice (no VIDEO_SUPPORT?)")?;
            let vctx: ID3D11VideoContext1 = context.cast().context("ID3D11VideoContext1")?;

            // VideoProcessor: input dims are set per-frame via the input view; the content desc
            // declares input==output==enc dims here (the Blt's dest rect scales to enc dims).
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                InputWidth: enc_w,
                InputHeight: enc_h,
                OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                OutputWidth: enc_w,
                OutputHeight: enc_h,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let venum = vdev
                .CreateVideoProcessorEnumerator(&content)
                .context("CreateVideoProcessorEnumerator")?;

            // Confirm BGRA-in / NV12-out support.
            let bgra_in = venum.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM).unwrap_or(0);
            let nv12_out = venum.CheckVideoProcessorFormat(DXGI_FORMAT_NV12).unwrap_or(0);
            if bgra_in & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0 {
                bail!("VideoProcessor does not support BGRA input");
            }
            if nv12_out & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0 {
                bail!("VideoProcessor does not support NV12 output");
            }

            let vproc = vdev.CreateVideoProcessor(&venum, 0).context("CreateVideoProcessor")?;

            // Color: BGRA full-range sRGB in → NV12 studio/limited BT.709 out (standard for encode).
            vctx.VideoProcessorSetStreamFrameFormat(&vproc, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamColorSpace1(&vproc, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            vctx.VideoProcessorSetOutputColorSpace1(&vproc, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);

            // NV12 output texture ring (bind RENDER_TARGET for the VP output view; add
            // VIDEO_ENCODER for the MFT — retry without it if a driver rejects that combo).
            let mut nv12 = Vec::with_capacity(NV12_RING);
            for _ in 0..NV12_RING {
                let tex = create_nv12(&device, enc_w, enc_h)?;
                nv12.push(tex);
            }

            tracing::info!(enc_w, enc_h, "GPU VideoProcessor ready (BGRA→NV12)");
            Ok(GpuConverter {
                device,
                context,
                vctx,
                vproc,
                venum,
                enc_w,
                enc_h,
                bridge: None,
                nv12,
                nv12_next: 0,
            })
        }
    }

    /// Convert one WGC BGRA frame to an NV12 GPU texture on our device. `wgc_ctx` is the
    /// capture frame's IMMEDIATE context (valid only inside on_frame_arrived).
    pub fn convert(
        &mut self,
        wgc_tex: &ID3D11Texture2D,
        wgc_ctx: &ID3D11DeviceContext,
        wgc_device: &ID3D11Device,
    ) -> Result<ID3D11Texture2D> {
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            wgc_tex.GetDesc(&mut desc);
            if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM || desc.ArraySize != 1 {
                bail!("unexpected WGC texture format {:?} array {}", desc.Format, desc.ArraySize);
            }
            self.ensure_bridge(desc.Width, desc.Height, wgc_device)?;
            let b = self.bridge.as_ref().unwrap();

            // WGC side: lock the shared texture, copy the frame onto it, hand to our side.
            b.wgc_km.AcquireSync(0, u32::MAX).context("wgc AcquireSync")?;
            wgc_ctx.CopyResource(&b.wgc, wgc_tex);
            b.wgc_km.ReleaseSync(1).context("wgc ReleaseSync")?;

            // Our side: lock, Blt BGRA→NV12 (scaled to enc dims) into the next ring texture.
            b.ours_km.AcquireSync(1, u32::MAX).context("our AcquireSync")?;
            let out_tex = self.nv12[self.nv12_next].clone();
            self.nv12_next = (self.nv12_next + 1) % NV12_RING;
            let out_view = self.make_output_view(&out_tex)?;

            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM::default();
            stream.Enable = true.into();
            stream.OutputIndex = 0;
            stream.InputFrameOrField = 0;
            // ManuallyDrop: clone the input view in; we drop it explicitly after the Blt so the
            // ref placed in the struct is released and we don't leak a surface every frame.
            stream.pInputSurface = core::mem::ManuallyDrop::new(Some(b.in_view.clone()));

            let blt = self.vctx.VideoProcessorBlt(&self.vproc, &out_view, 0, &[stream.clone()]);
            core::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
            blt.context("VideoProcessorBlt")?;

            b.ours_km.ReleaseSync(0).context("our ReleaseSync")?;
            Ok(out_tex)
        }
    }

    /// (Re)create the cross-device shared BGRA ingest texture + its input view for the given
    /// capture dims. Cached until the capture size changes.
    unsafe fn ensure_bridge(
        &mut self,
        cap_w: u32,
        cap_h: u32,
        wgc_device: &ID3D11Device,
    ) -> Result<()> {
        if let Some(b) = &self.bridge {
            if b.cap_w == cap_w && b.cap_h == cap_h {
                return Ok(());
            }
        }
        self.bridge = None;

        // Shared BGRA texture on OUR device.
        let desc = D3D11_TEXTURE2D_DESC {
            Width: cap_w,
            Height: cap_h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32,
        };
        let mut ours: Option<ID3D11Texture2D> = None;
        self.device
            .CreateTexture2D(&desc, None, Some(&mut ours))
            .context("create shared BGRA ingest texture")?;
        let ours = ours.context("no ingest texture")?;
        let ours_km: IDXGIKeyedMutex = ours.cast().context("ingest keyed mutex")?;

        // Share it onto the WGC device.
        let res1: IDXGIResource1 = ours.cast().context("IDXGIResource1")?;
        let handle = res1
            .CreateSharedHandle(None, DXGI_SHARED_RESOURCE_READ.0, None)
            .context("CreateSharedHandle")?;
        // Open OUR shared texture on the WGC device so its context can CopyResource into it.
        let wgc_dev1: ID3D11Device1 = wgc_device.cast().context("ID3D11Device1 (WGC)")?;
        let wgc: ID3D11Texture2D = wgc_dev1
            .OpenSharedResource1(handle)
            .context("OpenSharedResource1")?;
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        let wgc_km: IDXGIKeyedMutex = wgc.cast().context("wgc keyed mutex")?;

        // Input view for the VideoProcessor (reads OUR shared BGRA texture).
        let mut in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC::default();
        in_desc.FourCC = 0;
        in_desc.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D;
        in_desc.Anonymous.Texture2D = D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 };
        let res: ID3D11Resource = ours.cast()?;
        let mut in_view: Option<ID3D11VideoProcessorInputView> = None;
        let vdev: ID3D11VideoDevice = self.device.cast()?;
        vdev.CreateVideoProcessorInputView(&res, &self.venum, &in_desc, Some(&mut in_view))
            .context("CreateVideoProcessorInputView")?;
        let in_view = in_view.context("no input view")?;

        self.bridge = Some(Bridge {
            cap_w,
            cap_h,
            ours,
            ours_km,
            wgc,
            wgc_km,
            in_view,
        });
        tracing::info!(cap_w, cap_h, "GPU cross-device bridge ready");
        Ok(())
    }

    unsafe fn make_output_view(
        &self,
        nv12: &ID3D11Texture2D,
    ) -> Result<ID3D11VideoProcessorOutputView> {
        let mut out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC::default();
        out_desc.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2D;
        out_desc.Anonymous.Texture2D = D3D11_TEX2D_VPOV { MipSlice: 0 };
        let res: ID3D11Resource = nv12.cast()?;
        let mut view: Option<ID3D11VideoProcessorOutputView> = None;
        let vdev: ID3D11VideoDevice = self.device.cast()?;
        vdev.CreateVideoProcessorOutputView(&res, &self.venum, &out_desc, Some(&mut view))
            .context("CreateVideoProcessorOutputView")?;
        view.context("no output view")
    }

    pub fn enc_dims(&self) -> (u32, u32) {
        (self.enc_w, self.enc_h)
    }
}

/// Create an NV12 texture for VideoProcessor output + MF encoder input. Tries
/// RENDER_TARGET|VIDEO_ENCODER first (what a HW encoder wants), retries RENDER_TARGET-only.
unsafe fn create_nv12(device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D> {
    let mk = |bind: u32| -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex))?;
        tex.context("no NV12 texture")
    };
    let with_enc = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32;
    match mk(with_enc) {
        Ok(t) => Ok(t),
        Err(_) => mk(D3D11_BIND_RENDER_TARGET.0 as u32).context("create NV12 texture"),
    }
}
