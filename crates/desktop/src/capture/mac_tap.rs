// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! macOS capture through a CoreAudio **process tap** we build ourselves (macOS 14.2+).
//!
//! The plain "All apps" source goes through cpal, which builds a tap too (see `system.rs`), but
//! cpal's tap is fixed: every process, never muted. This module exists for the two things cpal
//! cannot express:
//!
//! * **One app** — a tap over a chosen set of process objects (`initStereoMixdownOfProcesses`).
//! * **Silent on this Mac** — the same tap with `CATapMuteBehavior::Muted`, so the tapped audio
//!   stops reaching the speakers while it is captured. That is the macOS counterpart of Linux's
//!   "Newfoundsync output device": the Mac would otherwise play the sound a whole buffer ahead of
//!   every synced client. The mute belongs to the tap, so CoreAudio lifts it when the tap is
//!   destroyed — including when this process dies without running a destructor.
//!
//! Plumbing, the same shape as Apple's sample and the AudioCap project: tap → private aggregate
//! device (system output as the clock, the tap as its input) → an IOProc that reads the tap's
//! buffers. Samples go through `system.rs`'s resampler into canonical 48 kHz stereo frames.
//!
//! Permission: the System Audio Recording prompt, the same one the cpal tap triggers. When it is
//! denied, the tap delivers silence rather than an error.

use std::ffi::{c_void, CStr};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{bail, Context, Result};
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceMainSubDeviceKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceSubDeviceListKey,
    kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyNominalSampleRate, kAudioHardwarePropertyDefaultSystemOutputDevice,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    kAudioSubDeviceUIDKey, kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey,
    kAudioTapPropertyFormat, AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID,
    AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
    AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
    AudioHardwareDestroyProcessTap, AudioObjectGetPropertyData, AudioObjectID,
    AudioObjectPropertyAddress, CATapDescription, CATapMuteBehavior,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, AudioBuffer, AudioBufferList, AudioStreamBasicDescription,
    AudioTimeStamp,
};
use objc2_core_foundation::{
    kCFAllocatorDefault, kCFTypeArrayCallBacks, kCFTypeDictionaryKeyCallBacks,
    kCFTypeDictionaryValueCallBacks, CFArray, CFDictionary, CFMutableDictionary, CFRetained,
    CFString,
};
use objc2_foundation::{NSArray, NSNumber, NSString};

use newfoundsync_core::codec::FRAME_SAMPLE_COUNT;

use super::system::Resampler;

static INSTANCE: AtomicU32 = AtomicU32::new(0);

/// What a tap listens to.
pub enum TapTarget {
    /// Only these CoreAudio process objects (one app and its helper processes).
    Processes(Vec<AudioObjectID>),
    /// Every process except these (all apps; the list is normally just us, if we have an object).
    AllExcept(Vec<AudioObjectID>),
}

/// State the IOProc owns. Boxed and handed to CoreAudio as client data; freed only after the
/// IOProc is stopped and destroyed.
struct Ctx {
    rs: Resampler,
    acc: Vec<i16>,
    on_frame: Box<dyn FnMut(&[i16]) + Send>,
}

/// A running tap capture. Dropping it stops the IOProc and destroys the aggregate device and the
/// tap, which also lifts the tap's mute.
pub struct TapCapture {
    tap: AudioObjectID,
    aggregate: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    ctx: *mut Ctx,
    pub device_name: String,
}

// The raw pointer is only touched by CoreAudio's IO thread while running, and by Drop after the
// IOProc has been stopped and destroyed.
unsafe impl Send for TapCapture {}

impl TapCapture {
    pub fn start<F>(target: TapTarget, silent: bool, label: &str, on_frame: F) -> Result<TapCapture>
    where
        F: FnMut(&[i16]) + Send + 'static,
    {
        let pid = std::process::id();
        let n = INSTANCE.fetch_add(1, Ordering::Relaxed);

        // 1. The tap.
        let desc = unsafe {
            match &target {
                TapTarget::Processes(ids) => CATapDescription::initStereoMixdownOfProcesses(
                    CATapDescription::alloc(),
                    &id_array(ids),
                ),
                TapTarget::AllExcept(ids) => CATapDescription::initStereoGlobalTapButExcludeProcesses(
                    CATapDescription::alloc(),
                    &id_array(ids),
                ),
            }
        };
        unsafe {
            desc.setName(&NSString::from_str(&format!("Newfoundsync tap {pid}.{n}")));
            desc.setPrivate(true);
            desc.setMuteBehavior(if silent { CATapMuteBehavior::Muted } else { CATapMuteBehavior::Unmuted });
        }
        let mut tap: AudioObjectID = 0;
        let st = unsafe { AudioHardwareCreateProcessTap(Some(&desc), &mut tap) };
        if st != 0 || tap == 0 {
            bail!(
                "macOS refused to create the audio tap (OSStatus {st}). Process taps need macOS 14.2 \
                 or newer."
            );
        }
        let tap_uid = unsafe { desc.UUID().UUIDString() };

        // Everything after this point must release the tap on failure.
        let cleanup_tap = |e: anyhow::Error| {
            unsafe { AudioHardwareDestroyProcessTap(tap) };
            e
        };

        let fmt: AudioStreamBasicDescription =
            get_prop(tap, kAudioTapPropertyFormat).map_err(cleanup_tap)?;
        if fmt.mFormatFlags & kAudioFormatFlagIsFloat == 0 || fmt.mBitsPerChannel != 32 {
            unsafe { AudioHardwareDestroyProcessTap(tap) };
            bail!(
                "the tap delivers an unexpected sample format (flags {:#x}, {} bits)",
                fmt.mFormatFlags,
                fmt.mBitsPerChannel
            );
        }

        // 2. The aggregate device: the system output is its clock, the tap is its input.
        let out_uid = match default_output_uid() {
            Ok(u) => u,
            Err(e) => return Err(cleanup_tap(e)),
        };
        let agg_uid = format!("ca.newfoundsync.tap.{pid}.{n}");
        let props = aggregate_properties(&tap_uid, &out_uid, &agg_uid, &format!("Newfoundsync {label}"));
        let mut aggregate: AudioObjectID = 0;
        let st = unsafe { AudioHardwareCreateAggregateDevice(&props, NonNull::from(&mut aggregate)) };
        if st != 0 || aggregate == 0 {
            unsafe { AudioHardwareDestroyProcessTap(tap) };
            bail!("could not create the capture device for the tap (OSStatus {st})");
        }
        let destroy_both = || unsafe {
            AudioHardwareDestroyAggregateDevice(aggregate);
            AudioHardwareDestroyProcessTap(tap);
        };

        // The IOProc runs on the aggregate's clock, so that is the rate the samples arrive at.
        let rate = get_prop::<f64>(aggregate, kAudioDevicePropertyNominalSampleRate)
            .ok()
            .filter(|r| *r > 1000.0)
            .unwrap_or(fmt.mSampleRate);
        let source_rate = rate.round() as u32;

        // 3. The IOProc.
        let ctx = Box::into_raw(Box::new(Ctx {
            rs: Resampler::new(source_rate),
            acc: Vec::with_capacity(FRAME_SAMPLE_COUNT + 2),
            on_frame: Box::new(on_frame),
        }));
        let mut proc_id: AudioDeviceIOProcID = None;
        let st = unsafe {
            AudioDeviceCreateIOProcID(aggregate, Some(io_proc), ctx.cast(), NonNull::from(&mut proc_id))
        };
        if st != 0 || proc_id.is_none() {
            destroy_both();
            drop(unsafe { Box::from_raw(ctx) });
            bail!("could not attach to the tap's capture device (OSStatus {st})");
        }
        let st = unsafe { AudioDeviceStart(aggregate, proc_id) };
        if st != 0 {
            unsafe { AudioDeviceDestroyIOProcID(aggregate, proc_id) };
            destroy_both();
            drop(unsafe { Box::from_raw(ctx) });
            bail!("could not start the tap's capture device (OSStatus {st})");
        }

        tracing::info!(
            label,
            silent,
            source_rate,
            tap_rate = fmt.mSampleRate,
            tap_channels = fmt.mChannelsPerFrame,
            "macOS process-tap capture started"
        );
        Ok(TapCapture {
            tap,
            aggregate,
            proc_id,
            ctx,
            device_name: if silent { format!("{label} (silent on this Mac)") } else { label.to_string() },
        })
    }
}

impl Drop for TapCapture {
    fn drop(&mut self) {
        unsafe {
            AudioDeviceStop(self.aggregate, self.proc_id);
            AudioDeviceDestroyIOProcID(self.aggregate, self.proc_id);
            AudioHardwareDestroyAggregateDevice(self.aggregate);
            AudioHardwareDestroyProcessTap(self.tap);
            drop(Box::from_raw(self.ctx));
        }
        tracing::info!("macOS process-tap capture stopped");
    }
}

/// Reads the tap's input buffers. Handles both layouts CoreAudio may hand over: one interleaved
/// buffer carrying every channel, or one mono buffer per channel.
unsafe extern "C-unwind" fn io_proc(
    _device: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    _output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> i32 {
    let Some(ctx) = (client as *mut Ctx).as_mut() else { return 0 };
    let list = input.as_ptr();
    let count = (*list).mNumberBuffers as usize;
    if count == 0 {
        return 0;
    }
    let bufs: &[AudioBuffer] = std::slice::from_raw_parts((*list).mBuffers.as_ptr(), count);
    let as_f32 = |b: &AudioBuffer| -> &[f32] {
        if b.mData.is_null() {
            &[]
        } else {
            std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4)
        }
    };
    let Ctx { rs, acc, on_frame } = ctx;
    let first = &bufs[0];
    if count == 1 || first.mNumberChannels >= 2 {
        let ch = first.mNumberChannels.max(1) as usize;
        for frame in as_f32(first).chunks_exact(ch) {
            let l = frame[0];
            let r = if ch >= 2 { frame[1] } else { l };
            rs.push(l, r, acc, on_frame);
        }
    } else {
        let left = as_f32(first);
        let right = as_f32(&bufs[1]);
        for (i, &l) in left.iter().enumerate() {
            rs.push(l, right.get(i).copied().unwrap_or(l), acc, on_frame);
        }
    }
    0
}

fn id_array(ids: &[AudioObjectID]) -> Retained<NSArray<NSNumber>> {
    let nums: Vec<Retained<NSNumber>> = ids.iter().map(|&i| NSNumber::new_u32(i)).collect();
    NSArray::from_retained_slice(&nums)
}

fn addr(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Read a fixed-size property.
pub(super) fn get_prop<T: Copy>(object: AudioObjectID, selector: u32) -> Result<T> {
    let a = addr(selector);
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    let mut size = std::mem::size_of::<T>() as u32;
    let st = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&a),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(value.as_mut_ptr().cast()),
        )
    };
    if st != 0 {
        bail!("CoreAudio property {selector:#x} on object {object}: OSStatus {st}");
    }
    Ok(unsafe { value.assume_init() })
}

/// Read a CFString property (bundle IDs, device UIDs).
pub(super) fn get_string_prop(object: AudioObjectID, selector: u32) -> Option<String> {
    let s: *const CFString = get_prop(object, selector).ok()?;
    if s.is_null() {
        return None;
    }
    // The getter hands over a +1 reference.
    let owned: CFRetained<CFString> = unsafe { CFRetained::from_raw(NonNull::new_unchecked(s as *mut CFString)) };
    Some(owned.to_string())
}

fn default_output_uid() -> Result<String> {
    let dev: AudioObjectID = get_prop(kAudioObjectSystemObject as AudioObjectID, kAudioHardwarePropertyDefaultSystemOutputDevice)
        .context("find the Mac's output device")?;
    get_string_prop(dev, kAudioDevicePropertyDeviceUID)
        .filter(|s| !s.is_empty())
        .context("read the output device's UID")
}

fn cfstr(c: &CStr) -> CFRetained<CFString> {
    CFString::from_str(c.to_str().unwrap_or_default())
}

fn new_dict() -> CFRetained<CFMutableDictionary> {
    unsafe {
        CFMutableDictionary::new(
            kCFAllocatorDefault,
            0,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }
    .expect("CFMutableDictionary")
}

fn set(dict: &CFMutableDictionary, key: &CStr, value: *const c_void) {
    let k = cfstr(key);
    unsafe { CFMutableDictionary::set_value(Some(dict), (&*k as *const CFString).cast(), value) };
}

fn array_of(items: &[*const c_void]) -> CFRetained<CFArray> {
    unsafe {
        CFArray::new(kCFAllocatorDefault, items.as_ptr() as *mut *const c_void, items.len() as _, &kCFTypeArrayCallBacks)
    }
    .expect("CFArray")
}

/// The aggregate device's description, as in Apple's process-tap sample:
/// ```text
/// { name, uid, main: <output uid>, private: YES, tapautostart: YES,
///   subdevices: [{ uid: <output uid> }],
///   taps: [{ uid: <tap uuid>, drift: YES }] }
/// ```
fn aggregate_properties(
    tap_uid: &NSString,
    out_uid: &str,
    agg_uid: &str,
    agg_name: &str,
) -> CFRetained<CFDictionary> {
    let yes = NSNumber::new_bool(true);
    let yes_ptr = (&*yes as *const NSNumber).cast::<c_void>();
    let out_uid_cf = CFString::from_str(out_uid);

    let sub = new_dict();
    set(&sub, kAudioSubDeviceUIDKey, (&*out_uid_cf as *const CFString).cast());
    let subs = array_of(&[(&*sub as *const CFMutableDictionary).cast()]);

    let tap = new_dict();
    set(&tap, kAudioSubTapUIDKey, (tap_uid as *const NSString).cast());
    set(&tap, kAudioSubTapDriftCompensationKey, yes_ptr);
    let taps = array_of(&[(&*tap as *const CFMutableDictionary).cast()]);

    let name = CFString::from_str(agg_name);
    let uid = CFString::from_str(agg_uid);
    let d = new_dict();
    set(&d, kAudioAggregateDeviceNameKey, (&*name as *const CFString).cast());
    set(&d, kAudioAggregateDeviceUIDKey, (&*uid as *const CFString).cast());
    set(&d, kAudioAggregateDeviceMainSubDeviceKey, (&*out_uid_cf as *const CFString).cast());
    set(&d, kAudioAggregateDeviceIsPrivateKey, yes_ptr);
    set(&d, kAudioAggregateDeviceTapAutoStartKey, yes_ptr);
    set(&d, kAudioAggregateDeviceSubDeviceListKey, (&*subs as *const CFArray).cast());
    set(&d, kAudioAggregateDeviceTapListKey, (&*taps as *const CFArray).cast());
    unsafe { CFRetained::cast_unchecked::<CFDictionary>(d) }
}
