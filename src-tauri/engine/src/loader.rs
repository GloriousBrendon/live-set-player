//! Load-time audio pipeline (`docs/SPEC.md` §1): decode WAV → downmix to mono →
//! resample to the **engine rate** → preload as `Arc<[f32]>`.
//!
//! Everything here is worker-thread code. Nothing in this module is called from the
//! audio callback; nothing at playback time ever resamples (CLAUDE.md invariant 3).
//! The engine rate is fixed at device-open time (`crate::device`): on Linux it is
//! the project rate or the open fails; on Windows/WASAPI shared mode it is the mix
//! format's rate, and the whole project is resampled to it here, offline, with
//! `rubato`'s sinc resampler.
//!
//! Downmix per §1: stereo sums as `(L + R) * 0.5` (−6 dB), configurable per track
//! as `sum` / `left` / `right`. Files with more than two channels are rejected —
//! backtracks and stems are mono or stereo by construction.

use crate::error::LoadError;
use crate::project::{DownmixMode, Project, Song};
use crate::render::AudioBank;
use crate::rt::{self, Loaded};
use std::path::Path;
use std::sync::Arc;

/// A decoded, mono, source-rate track.
pub struct DecodedMono {
    pub samples: Vec<f32>,
    pub source_rate: u32,
}

/// Decode a WAV file and downmix it to mono. Supports 16/24/32-bit integer and
/// 32-bit float PCM, mono or stereo.
pub fn decode_wav_mono(path: &Path, downmix: DownmixMode) -> Result<DecodedMono, LoadError> {
    let display = path.display().to_string();
    let mut reader = hound::WavReader::open(path).map_err(|e| match e {
        hound::Error::IoError(io) => LoadError::Io {
            path: display.clone(),
            source: io,
        },
        other => LoadError::Wav {
            path: display.clone(),
            source: other,
        },
    })?;
    let spec = reader.spec();
    if spec.channels == 0 || spec.channels > 2 {
        return Err(LoadError::UnsupportedFormat {
            path: display,
            detail: format!(
                "{} channels (only mono and stereo are supported)",
                spec.channels
            ),
        });
    }

    let interleaved: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, 32) => reader
            .samples::<f32>()
            .collect::<Result<_, _>>()
            .map_err(|e| LoadError::Wav {
                path: display.clone(),
                source: e,
            })?,
        (hound::SampleFormat::Int, bits @ (16 | 24 | 32)) => {
            let scale = 1.0f64 / (1i64 << (bits - 1)) as f64;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| (v as f64 * scale) as f32))
                .collect::<Result<_, _>>()
                .map_err(|e| LoadError::Wav {
                    path: display.clone(),
                    source: e,
                })?
        }
        (fmt, bits) => {
            return Err(LoadError::UnsupportedFormat {
                path: display,
                detail: format!("{fmt:?} {bits}-bit"),
            })
        }
    };

    let samples = if spec.channels == 1 {
        interleaved
    } else {
        downmix_stereo(&interleaved, downmix)
    };
    Ok(DecodedMono {
        samples,
        source_rate: spec.sample_rate,
    })
}

/// Fold interleaved stereo to mono per §1: `sum` is `(L + R) * 0.5` (−6 dB).
pub fn downmix_stereo(interleaved: &[f32], mode: DownmixMode) -> Vec<f32> {
    let frames = interleaved.len() / 2;
    let mut out = Vec::with_capacity(frames);
    for i in 0..frames {
        let l = interleaved[i * 2];
        let r = interleaved[i * 2 + 1];
        out.push(match mode {
            DownmixMode::Sum => (l + r) * 0.5,
            DownmixMode::Left => l,
            DownmixMode::Right => r,
        });
    }
    out
}

/// Resample mono audio from `from_rate` to `to_rate` with rubato's sinc resampler
/// (high-quality offline settings). Identity — the input vector returned untouched,
/// bit-exact — when the rates already match, which is the common case on Linux and
/// on Windows when the mix format matches the project.
pub fn resample_mono(input: Vec<f32>, from_rate: u32, to_rate: u32) -> Result<Vec<f32>, LoadError> {
    use rubato::{
        Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
    };

    if from_rate == to_rate {
        return Ok(input);
    }
    let ratio = to_rate as f64 / from_rate as f64;
    let expected_len = (input.len() as f64 * ratio).round() as usize;
    if input.is_empty() {
        return Ok(Vec::new());
    }

    let params = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 256,
        window: WindowFunction::BlackmanHarris2,
    };
    const CHUNK: usize = 1024;
    let mut resampler = SincFixedIn::<f64>::new(ratio, 2.0, params, CHUNK, 1)
        .map_err(|e| LoadError::Resample(e.to_string()))?;

    // The sinc filter introduces a fixed latency in output samples; skip it so
    // sample 0 of the result corresponds to sample 0 of the input (offsets and
    // section positions must not shift by the filter delay).
    let delay = resampler.output_delay();
    let needed = expected_len + delay;

    let input_f64: Vec<f64> = input.iter().map(|&s| s as f64).collect();
    let mut collected: Vec<f64> = Vec::with_capacity(needed + CHUNK * 2);
    let mut pos = 0usize;
    while collected.len() < needed {
        let want = resampler.input_frames_next();
        let out = if pos + want <= input_f64.len() {
            let chunk = &input_f64[pos..pos + want];
            pos += want;
            resampler
                .process(&[chunk], None)
                .map_err(|e| LoadError::Resample(e.to_string()))?
        } else if pos < input_f64.len() {
            let chunk = &input_f64[pos..];
            pos = input_f64.len();
            resampler
                .process_partial(Some(&[chunk]), None)
                .map_err(|e| LoadError::Resample(e.to_string()))?
        } else {
            // Flush: feed nothing until the filter has drained what we need.
            resampler
                .process_partial::<&[f64]>(None, None)
                .map_err(|e| LoadError::Resample(e.to_string()))?
        };
        let ch = &out[0];
        if ch.is_empty() && pos >= input_f64.len() {
            break; // resampler fully drained; avoid a pathological infinite loop
        }
        collected.extend_from_slice(ch);
    }

    Ok(collected
        .into_iter()
        .skip(delay)
        .take(expected_len)
        .map(|s| s as f32)
        .collect())
}

/// Full per-file pipeline: decode → downmix → resample to `engine_rate` → `Arc<[f32]>`.
pub fn load_track_audio(
    path: &Path,
    downmix: DownmixMode,
    engine_rate: u32,
) -> Result<Arc<[f32]>, LoadError> {
    let decoded = decode_wav_mono(path, downmix)?;
    let resampled = resample_mono(decoded.samples, decoded.source_rate, engine_rate)?;
    Ok(resampled.into())
}

/// Load every track of `song` (paths resolved against `project_dir`) into an
/// [`AudioBank`] at `engine_rate`.
pub fn load_song_bank(
    project_dir: &Path,
    song: &Song,
    engine_rate: u32,
) -> Result<AudioBank, LoadError> {
    let mut bank = AudioBank::new();
    for track in &song.tracks {
        let path = track.file.path.to_platform(project_dir);
        let audio = load_track_audio(&path, track.downmix, engine_rate)?;
        bank.insert(track.id.clone(), audio);
    }
    Ok(bank)
}

/// Convenience for the worker thread: load a song's files and build the
/// ready-to-swap [`Loaded`] in one call. All allocation happens here; the audio
/// thread receives the finished `Box` through `Command::LoadSong`.
pub fn load_and_prepare(
    project_dir: &Path,
    project: &Project,
    song: &Song,
    engine_rate: u32,
) -> Result<Box<Loaded>, LoadError> {
    let bank = load_song_bank(project_dir, song, engine_rate)?;
    rt::prepare_loaded(project, song, &bank, engine_rate)
        .map_err(|e| LoadError::Resample(format!("prepare failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_test_wav(
        path: &Path,
        spec: hound::WavSpec,
        write: impl FnOnce(&mut hound::WavWriter<std::io::BufWriter<std::fs::File>>),
    ) {
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        write(&mut w);
        w.finalize().unwrap();
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("lsp_loader_{}_{name}", std::process::id()))
    }

    #[test]
    fn decodes_16_bit_stereo_and_downmixes_sum_at_minus_6_db() {
        let path = temp_path("s16.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        write_test_wav(&path, spec, |w| {
            // Frame 0: L=+16384, R=+16384 -> mono 0.5*(0.5+0.5) = 0.5
            // Frame 1: L=-32768, R=0      -> mono 0.5*(-1.0+0.0) = -0.5
            for s in [16384i16, 16384, -32768, 0] {
                w.write_sample(s).unwrap();
            }
        });
        let d = decode_wav_mono(&path, DownmixMode::Sum).unwrap();
        assert_eq!(d.source_rate, 48000);
        assert_eq!(d.samples.len(), 2);
        assert!((d.samples[0] - 0.5).abs() < 1e-4);
        assert!((d.samples[1] + 0.5).abs() < 1e-4);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn left_and_right_downmix_pick_one_channel() {
        let interleaved = [0.25f32, -0.75, 0.5, -0.5];
        assert_eq!(
            downmix_stereo(&interleaved, DownmixMode::Left),
            vec![0.25, 0.5]
        );
        assert_eq!(
            downmix_stereo(&interleaved, DownmixMode::Right),
            vec![-0.75, -0.5]
        );
    }

    #[test]
    fn float_wav_decodes_bit_exact_mono() {
        let path = temp_path("f32.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44100,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let values = [0.0f32, 0.123456, -0.999, 1.0];
        write_test_wav(&path, spec, |w| {
            for &s in &values {
                w.write_sample(s).unwrap();
            }
        });
        let d = decode_wav_mono(&path, DownmixMode::Sum).unwrap();
        assert_eq!(d.samples, values);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn more_than_two_channels_is_rejected() {
        let path = temp_path("4ch.wav");
        let spec = hound::WavSpec {
            channels: 4,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        write_test_wav(&path, spec, |w| {
            for _ in 0..8 {
                w.write_sample(0i16).unwrap();
            }
        });
        assert!(matches!(
            decode_wav_mono(&path, DownmixMode::Sum),
            Err(LoadError::UnsupportedFormat { .. })
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn equal_rates_resample_is_bit_exact_identity() {
        let input: Vec<f32> = (0..10_000)
            .map(|i| ((i * 7919) % 1000) as f32 / 1000.0)
            .collect();
        let out = resample_mono(input.clone(), 48000, 48000).unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn resample_44100_to_48000_preserves_length_and_content() {
        let from = 44100u32;
        let to = 48000u32;
        let seconds = 2.0f64;
        let freq = 440.0f64;
        let n = (from as f64 * seconds) as usize;
        let input: Vec<f32> = (0..n)
            .map(|i| {
                ((2.0 * std::f64::consts::PI * freq * i as f64 / from as f64).sin() * 0.5) as f32
            })
            .collect();

        let out = resample_mono(input, from, to).unwrap();
        let expected_len = (n as f64 * to as f64 / from as f64).round() as usize;
        assert_eq!(out.len(), expected_len);

        // RMS of a 0.5-amplitude sine is 0.5/sqrt(2); check the interior (away from
        // filter edge effects at the very ends).
        let interior = &out[4800..out.len() - 4800];
        let rms = (interior
            .iter()
            .map(|&s| (s as f64) * (s as f64))
            .sum::<f64>()
            / interior.len() as f64)
            .sqrt();
        let expected_rms = 0.5 / 2.0f64.sqrt();
        assert!(
            (rms - expected_rms).abs() / expected_rms < 0.02,
            "RMS after resample: {rms}, expected ~{expected_rms}"
        );

        // Frequency survives: count zero crossings in the interior.
        let crossings = interior
            .windows(2)
            .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
            .count();
        let measured_freq = crossings as f64 / 2.0 / (interior.len() as f64 / to as f64);
        assert!(
            (measured_freq - freq).abs() < 2.0,
            "frequency after resample: {measured_freq} Hz, expected ~{freq} Hz"
        );
    }
}
