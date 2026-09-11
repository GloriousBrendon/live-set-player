//! Audio device layer (`docs/SPEC.md` §1): explicit device selection by name,
//! platform-correct sample-rate policy, stream creation, and surfacing device loss.
//!
//! Hard rules from §1, enforced here:
//!
//! - **Never fall back to a default device.** [`find_output_device`] either finds
//!   the named device or fails with the list of what *is* available.
//! - **One engine rate for the stream's lifetime**, fixed at open:
//!   - **Linux (ALSA/PipeWire):** open at exactly the project rate; if the device
//!     doesn't support it, fail loudly listing the rates it does support.
//!   - **Windows (WASAPI shared mode):** the device advertises only the mix format
//!     from the Sound control panel; forcing another rate is impossible (Windows
//!     would resample silently underneath). Read the device's actual rate, make it
//!     the engine rate, and let the loader resample the project to it. The mismatch
//!     is a `notice` (informational), not an error.
//! - Buffer sizes prefer 1024 frames, never below 512 by choice — xrun immunity
//!   over latency. If the device rejects a fixed size, its default is used.
//! - Device loss mid-session must not panic the process: the cpal error callback
//!   sets an atomic flag the UI polls ([`OpenedStream::device_error`]); the
//!   transport keeps its state and the caller re-opens when the user asks.
//!
//! The rate policy itself ([`choose_engine_rate`]) is a pure function over plain
//! data so both platform behaviours are unit-tested on any host.

use crate::error::DeviceError;
use crate::rt::{self, EngineHandle, GarbageDrain, RtEngine};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, SupportedBufferSize};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

pub const PREFERRED_BUFFER_FRAMES: u32 = 1024;
pub const MIN_BUFFER_FRAMES: u32 = 512;

/// Enumerate output device names on the default host (WASAPI on Windows,
/// ALSA/PipeWire on Linux). The name is the §1 persistence key.
///
/// **De-duplicated, in first-seen order.** ALSA routinely enumerates the same card
/// many times over (one entry per sub-device/plugin configuration) — a typical
/// desktop reports ~49 devices under ~23 distinct names, with a single analog output
/// appearing 15 times. Since the persistence key is the *name* and
/// [`find_output_device`] resolves a name to the **first** device bearing it, showing
/// the duplicates would offer the user choices that are not distinguishable by the
/// thing we actually store, and all but the first of which are unreachable. Listing
/// each name once keeps the picker honest: every entry it offers is an entry
/// `find_output_device` can actually return.
pub fn list_output_devices() -> Result<Vec<String>, DeviceError> {
    let host = cpal::default_host();
    let devices = host
        .output_devices()
        .map_err(|e| DeviceError::Backend(e.to_string()))?;
    let mut seen = std::collections::HashSet::new();
    Ok(devices
        .filter_map(|d| d.description().ok().map(|desc| desc.name().to_string()))
        .filter(|name| seen.insert(name.clone()))
        .collect())
}

/// Find a device by its persisted name. No fallback of any kind.
pub fn find_output_device(name: &str) -> Result<cpal::Device, DeviceError> {
    let host = cpal::default_host();
    let devices = host
        .output_devices()
        .map_err(|e| DeviceError::Backend(e.to_string()))?;
    let mut available = Vec::new();
    for device in devices {
        match device.description() {
            Ok(desc) if desc.name() == name => return Ok(device),
            Ok(desc) => available.push(desc.name().to_string()),
            Err(_) => {}
        }
    }
    Err(DeviceError::DeviceNotFound {
        name: name.to_string(),
        available,
    })
}

/// Which §1 sample-rate rule applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RatePolicy {
    /// Use the device's shared-mode mix rate as the engine rate; resample the
    /// project to it at load time; report a mismatch as a notice.
    WindowsSharedMix,
    /// Open at exactly the project rate or fail listing supported rates.
    LinuxExact,
}

pub fn platform_rate_policy() -> RatePolicy {
    if cfg!(target_os = "windows") {
        RatePolicy::WindowsSharedMix
    } else {
        RatePolicy::LinuxExact
    }
}

/// A supported sample-rate range advertised by the device (plain data so the
/// policy is testable without a device).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupportedRateRange {
    pub min_rate: u32,
    pub max_rate: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateChoice {
    pub engine_rate: u32,
    /// §1: on Windows a project/device rate mismatch is informational, not an error.
    pub notice: Option<String>,
}

/// Decide the engine rate per §1. Pure function; see [`RatePolicy`].
pub fn choose_engine_rate(
    policy: RatePolicy,
    project_rate: u32,
    device_default_rate: u32,
    supported: &[SupportedRateRange],
) -> Result<RateChoice, DeviceError> {
    match policy {
        RatePolicy::WindowsSharedMix => {
            let notice = (device_default_rate != project_rate).then(|| {
                format!(
                    "Device mix format is {device_default_rate} Hz but the project is \
                     {project_rate} Hz. WASAPI shared mode cannot be forced to the project \
                     rate; all audio is resampled to {device_default_rate} Hz at load time."
                )
            });
            Ok(RateChoice {
                engine_rate: device_default_rate,
                notice,
            })
        }
        RatePolicy::LinuxExact => {
            let ok = supported
                .iter()
                .any(|r| r.min_rate <= project_rate && project_rate <= r.max_rate);
            if ok {
                Ok(RateChoice {
                    engine_rate: project_rate,
                    notice: None,
                })
            } else {
                Err(DeviceError::UnsupportedRate {
                    requested: project_rate,
                    supported: supported.iter().map(|r| (r.min_rate, r.max_rate)).collect(),
                })
            }
        }
    }
}

/// A running output stream plus everything the caller needs to drive it. Note that
/// `cpal::Stream` is not `Send`: keep this on the thread that opened it (dropping
/// it stops and closes the stream).
pub struct OpenedStream {
    pub stream: cpal::Stream,
    pub engine_rate: u32,
    pub channels: u16,
    /// The §1 informational notice (Windows rate mismatch), if any.
    pub notice: Option<String>,
    /// Set nonzero by the cpal error callback: 1 = device no longer available,
    /// 2 = other stream error. The UI polls this and offers a re-open.
    error_flag: Arc<AtomicU32>,
}

impl OpenedStream {
    pub fn device_error(&self) -> Option<u32> {
        match self.error_flag.load(Ordering::Acquire) {
            0 => None,
            e => Some(e),
        }
    }
}

/// Open `device_name` for output per the §1 rules, build the real-time engine at
/// the resulting engine rate, and start the stream. Returns the stream together
/// with the UI-side [`EngineHandle`] and the worker-side [`GarbageDrain`].
///
/// The engine registers the callback thread with MMCSS as "Pro Audio" on first
/// callback (Windows; see [`crate::mmcss`] for what cpal does and doesn't do).
pub fn open_output(
    device_name: &str,
    project_rate: u32,
    requested_buffer_frames: Option<u32>,
) -> Result<(OpenedStream, EngineHandle, GarbageDrain), DeviceError> {
    let device = find_output_device(device_name)?;
    let default_cfg = device
        .default_output_config()
        .map_err(|e| DeviceError::Backend(e.to_string()))?;

    let policy = platform_rate_policy();
    let supported_cfgs: Vec<cpal::SupportedStreamConfigRange> = device
        .supported_output_configs()
        .map_err(|e| DeviceError::Backend(e.to_string()))?
        .collect();
    let supported_rates: Vec<SupportedRateRange> = supported_cfgs
        .iter()
        .map(|c| SupportedRateRange {
            min_rate: c.min_sample_rate(),
            max_rate: c.max_sample_rate(),
        })
        .collect();

    let choice = choose_engine_rate(
        policy,
        project_rate,
        default_cfg.sample_rate(),
        &supported_rates,
    )?;

    // Pick the concrete stream config for the chosen rate.
    let chosen = match policy {
        RatePolicy::WindowsSharedMix => default_cfg,
        RatePolicy::LinuxExact => {
            // Among configs supporting the project rate, prefer f32, then i16, then
            // u16; prefer at least 2 channels.
            let mut candidates: Vec<_> = supported_cfgs
                .iter()
                .filter(|c| {
                    c.min_sample_rate() <= choice.engine_rate
                        && choice.engine_rate <= c.max_sample_rate()
                })
                .collect();
            candidates.sort_by_key(|c| {
                let fmt_rank = match c.sample_format() {
                    SampleFormat::F32 => 0,
                    SampleFormat::I16 => 1,
                    SampleFormat::U16 => 2,
                    _ => 3,
                };
                let ch_rank = if c.channels() >= 2 { 0 } else { 1 };
                (fmt_rank, ch_rank)
            });
            candidates
                .first()
                .ok_or(DeviceError::NoUsableConfig)?
                .with_sample_rate(choice.engine_rate)
        }
    };

    let sample_format = chosen.sample_format();
    let supported_buffer = *chosen.buffer_size();
    let mut config: cpal::StreamConfig = chosen.into();

    // Buffer size: prefer 1024 (or the caller's request), clamped to the device's
    // supported range, floored at 512 where the range allows — never optimised for
    // latency. If the device won't take a fixed size we retry with its default.
    let requested = requested_buffer_frames
        .unwrap_or(PREFERRED_BUFFER_FRAMES)
        .max(MIN_BUFFER_FRAMES);
    let fixed = match supported_buffer {
        SupportedBufferSize::Range { min, max } => Some(requested.clamp(min, max)),
        SupportedBufferSize::Unknown => Some(requested),
    };

    let engine_rate = choice.engine_rate;
    let channels = config.channels as usize;
    let error_flag = Arc::new(AtomicU32::new(0));

    let mut attempt = |buffer: BufferSize| -> Result<
        (cpal::Stream, EngineHandle, GarbageDrain),
        cpal::BuildStreamError,
    > {
        config.buffer_size = buffer;
        let (engine, handle, garbage) = rt::new_engine(engine_rate, true);
        let flag = error_flag.clone();
        let error_cb = move |err: cpal::StreamError| {
            let code = match err {
                cpal::StreamError::DeviceNotAvailable => 1,
                _ => 2,
            };
            flag.store(code, Ordering::Release);
        };
        let stream = build_stream(&device, &config, sample_format, engine, channels, error_cb)?;
        Ok((stream, handle, garbage))
    };

    let (stream, handle, garbage) = match fixed {
        Some(frames) => match attempt(BufferSize::Fixed(frames)) {
            Ok(ok) => ok,
            // Some backends reject fixed sizes outright; the device default is the
            // stable choice, not an error.
            Err(_) => {
                attempt(BufferSize::Default).map_err(|e| DeviceError::Backend(e.to_string()))?
            }
        },
        None => attempt(BufferSize::Default).map_err(|e| DeviceError::Backend(e.to_string()))?,
    };

    stream
        .play()
        .map_err(|e| DeviceError::Backend(e.to_string()))?;

    Ok((
        OpenedStream {
            stream,
            engine_rate,
            channels: config.channels,
            notice: choice.notice,
            error_flag,
        },
        handle,
        garbage,
    ))
}

/// Build the typed cpal stream. f32 renders directly into the device buffer; i16
/// and u16 devices render into a preallocated f32 scratch buffer and convert —
/// still allocation-free per callback.
fn build_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: SampleFormat,
    mut engine: RtEngine,
    channels: usize,
    error_cb: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    match sample_format {
        SampleFormat::F32 => device.build_output_stream(
            config,
            move |data: &mut [f32], _| engine.process(data, channels),
            error_cb,
            None,
        ),
        SampleFormat::I16 => {
            build_converting_stream::<i16>(device, config, engine, channels, error_cb)
        }
        SampleFormat::U16 => {
            build_converting_stream::<u16>(device, config, engine, channels, error_cb)
        }
        _ => Err(cpal::BuildStreamError::StreamConfigNotSupported),
    }
}

fn build_converting_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut engine: RtEngine,
    channels: usize,
    error_cb: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    // Preallocated once here (worker thread); the callback only slices into it.
    // Length is a multiple of `channels`, and device buffers are too, so chunking
    // below always lands on frame boundaries.
    let mut scratch = vec![0.0f32; crate::core::MAX_BLOCK_FRAMES * channels.max(1)];
    device.build_output_stream(
        config,
        move |data: &mut [T], _| {
            let mut offset = 0usize;
            while offset < data.len() {
                let n = (data.len() - offset).min(scratch.len());
                let part = &mut scratch[..n];
                engine.process(part, channels);
                for (dst, &src) in data[offset..offset + n].iter_mut().zip(part.iter()) {
                    *dst = T::from_sample(src);
                }
                offset += n;
            }
        },
        error_cb,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const RANGES: &[SupportedRateRange] = &[
        SupportedRateRange {
            min_rate: 44100,
            max_rate: 44100,
        },
        SupportedRateRange {
            min_rate: 48000,
            max_rate: 96000,
        },
    ];

    #[test]
    fn windows_policy_uses_device_mix_rate_and_notices_mismatch() {
        let c = choose_engine_rate(RatePolicy::WindowsSharedMix, 44100, 48000, RANGES).unwrap();
        assert_eq!(c.engine_rate, 48000);
        assert!(c.notice.is_some(), "rate mismatch must produce a notice");

        let c = choose_engine_rate(RatePolicy::WindowsSharedMix, 48000, 48000, RANGES).unwrap();
        assert_eq!(c.engine_rate, 48000);
        assert!(c.notice.is_none(), "matching rates need no notice");
    }

    #[test]
    fn linux_policy_opens_at_project_rate_when_supported() {
        for rate in [44100u32, 48000] {
            let c = choose_engine_rate(RatePolicy::LinuxExact, rate, 96000, RANGES).unwrap();
            assert_eq!(c.engine_rate, rate, "engine rate must be the project rate");
            assert!(c.notice.is_none());
        }
    }

    #[test]
    fn linux_policy_fails_loudly_listing_supported_rates() {
        let only_48k = [SupportedRateRange {
            min_rate: 48000,
            max_rate: 48000,
        }];
        let err = choose_engine_rate(RatePolicy::LinuxExact, 44100, 48000, &only_48k).unwrap_err();
        match err {
            DeviceError::UnsupportedRate {
                requested,
                supported,
            } => {
                assert_eq!(requested, 44100);
                assert_eq!(supported, vec![(48000, 48000)]);
            }
            other => panic!("expected UnsupportedRate, got {other:?}"),
        }
    }

    #[test]
    fn missing_device_error_lists_available_devices() {
        // Whatever hardware the test host has, a device with this name won't exist;
        // the error must carry the enumeration rather than falling back.
        // (`cpal::Device` has no `Debug`, so no `unwrap_err` here.)
        let err = match find_output_device("::definitely-not-a-real-device::") {
            Err(e) => e,
            Ok(_) => panic!("nonexistent device was somehow found"),
        };
        match err {
            DeviceError::DeviceNotFound { name, .. } => {
                assert_eq!(name, "::definitely-not-a-real-device::");
            }
            DeviceError::Backend(_) => {} // acceptable on hosts with no audio backend at all
            other => panic!("expected DeviceNotFound, got {other:?}"),
        }
    }
}
