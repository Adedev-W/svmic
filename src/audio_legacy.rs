/*
Legacy WAV-to-VB-CABLE prototype. Intentionally disabled while the MC agent
layer is developed; it will be reconnected to the TTS pipeline later.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, TrySendError};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, SupportedBufferSize, SupportedStreamConfig};

const TARGET_DEVICE: &str = "CABLE Input";
const CPAL_TARGET_DEVICE: &str = "CABLE Input (VB-Audio Virtual Cable)";
const OUTPUT_CHANNELS: u16 = 2;

#[derive(Debug)]
struct AudioData {
    samples: Vec<i16>,
    sample_rate: u32,
    channels: u16,
}

#[derive(Debug)]
enum PlaybackEvent {
    Finished,
    StreamError(String),
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let path = parse_audio_path(std::env::args_os().skip(1))?;
    let audio = load_wav(&path)?;
    let frame_count = audio.samples.len() / usize::from(audio.channels);
    let duration = frame_count as f64 / f64::from(audio.sample_rate);

    println!("File     : {}", path.display());
    println!(
        "Format   : PCM 16-bit, {} Hz, {} channel(s), {:.2} seconds",
        audio.sample_rate, audio.channels, duration
    );

    let host = cpal::default_host();
    let device = find_output_device(&host)?;
    let description = device
        .description()
        .map(|description| description.to_string())
        .unwrap_or_else(|_| TARGET_DEVICE.to_owned());
    let supported_config = select_output_config(&device, audio.sample_rate)?;
    let drain_time = drain_time(&supported_config);

    println!("Device   : {description}");
    println!(
        "Output   : {} Hz, {} channels, {:?}",
        supported_config.sample_rate(),
        supported_config.channels(),
        supported_config.sample_format()
    );

    let samples = into_stereo(audio.samples, audio.channels)?;
    play_samples(&device, supported_config, samples, drain_time)?;

    println!("Finished : audio sent to {TARGET_DEVICE}");
    Ok(())
}

fn parse_audio_path<I>(args: I) -> Result<PathBuf, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let path = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| "Usage: svmic <path-wav>".to_owned())?;

    if args.next().is_some() {
        return Err("Usage: svmic <path-wav> (only one audio file is accepted)".to_owned());
    }

    Ok(path)
}

fn load_wav(path: &Path) -> Result<AudioData, String> {
    let mut reader = hound::WavReader::open(path)
        .map_err(|error| format!("failed to open WAV '{}': {error}", path.display()))?;
    let spec = reader.spec();
    validate_wav_spec(spec)?;

    let samples = reader
        .samples::<i16>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to decode WAV '{}': {error}", path.display()))?;

    if samples.is_empty() {
        return Err(format!(
            "WAV '{}' contains no audio samples",
            path.display()
        ));
    }

    if samples.len() % usize::from(spec.channels) != 0 {
        return Err(format!(
            "WAV '{}' contains an incomplete audio frame",
            path.display()
        ));
    }

    Ok(AudioData {
        samples,
        sample_rate: spec.sample_rate,
        channels: spec.channels,
    })
}

fn validate_wav_spec(spec: hound::WavSpec) -> Result<(), String> {
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        return Err(format!(
            "unsupported WAV format: expected 16-bit PCM, got {:?} with {} bits per sample",
            spec.sample_format, spec.bits_per_sample
        ));
    }

    if !matches!(spec.channels, 1 | 2) {
        return Err(format!(
            "unsupported WAV channel count: expected mono or stereo, got {}",
            spec.channels
        ));
    }

    if spec.sample_rate == 0 {
        return Err("invalid WAV sample rate: 0 Hz".to_owned());
    }

    Ok(())
}

fn into_stereo(samples: Vec<i16>, channels: u16) -> Result<Vec<i16>, String> {
    match channels {
        1 => {
            let mut stereo = Vec::with_capacity(samples.len() * usize::from(OUTPUT_CHANNELS));
            for sample in samples {
                stereo.extend_from_slice(&[sample, sample]);
            }
            Ok(stereo)
        }
        2 => Ok(samples),
        _ => Err(format!(
            "cannot convert {channels} input channels to stereo output"
        )),
    }
}

fn find_output_device(host: &cpal::Host) -> Result<Device, String> {
    let devices = host
        .output_devices()
        .map_err(|error| format!("failed to enumerate output devices: {error}"))?;
    let mut available = Vec::new();

    for device in devices {
        match device.description() {
            Ok(description) => {
                available.push(description.name().to_owned());
                if is_target_device_name(description.name()) {
                    return Ok(device);
                }
            }
            Err(error) => available.push(format!("<unreadable device: {error}>")),
        }
    }

    available.sort();
    available.dedup();
    let listed_devices = if available.is_empty() {
        "<none>".to_owned()
    } else {
        available.join(", ")
    };

    Err(format!(
        "output device '{TARGET_DEVICE}' was not found; available output devices: {listed_devices}"
    ))
}

fn is_target_device_name(name: &str) -> bool {
    matches!(name, TARGET_DEVICE | CPAL_TARGET_DEVICE)
}

fn select_output_config(
    device: &Device,
    sample_rate: u32,
) -> Result<SupportedStreamConfig, String> {
    let configs = device
        .supported_output_configs()
        .map_err(|error| format!("failed to query '{TARGET_DEVICE}' configurations: {error}"))?;

    configs
        .filter(|config| {
            config.channels() == OUTPUT_CHANNELS
                && config.sample_format() == SampleFormat::I16
                && config.contains_rate(sample_rate)
        })
        .find_map(|config| config.try_with_sample_rate(sample_rate))
        .ok_or_else(|| {
            format!("'{TARGET_DEVICE}' does not support stereo 16-bit output at {sample_rate} Hz")
        })
}

fn drain_time(config: &SupportedStreamConfig) -> Duration {
    let seconds = match config.buffer_size() {
        SupportedBufferSize::Range { max, .. } => {
            (f64::from(*max) / f64::from(config.sample_rate())) * 2.0
        }
        SupportedBufferSize::Unknown => 0.1,
    };

    Duration::from_secs_f64(seconds.max(0.05))
}

fn play_samples(
    device: &Device,
    supported_config: SupportedStreamConfig,
    samples: Vec<i16>,
    drain_time: Duration,
) -> Result<(), String> {
    let stream_config = supported_config.config();
    let (event_tx, event_rx) = mpsc::sync_channel(1);
    let error_tx = event_tx.clone();
    let mut cursor = 0;
    let mut completion_sent = false;

    let stream = device
        .build_output_stream(
            stream_config,
            move |output: &mut [i16], _| {
                let remaining = samples.len().saturating_sub(cursor);
                let count = remaining.min(output.len());

                output[..count].copy_from_slice(&samples[cursor..cursor + count]);
                output[count..].fill(0);
                cursor += count;

                if cursor == samples.len() && !completion_sent {
                    completion_sent = true;
                    let _ = event_tx.try_send(PlaybackEvent::Finished);
                }
            },
            move |error| match error_tx.try_send(PlaybackEvent::StreamError(error.to_string())) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => {}
            },
            None,
        )
        .map_err(|error| format!("failed to create output stream: {error}"))?;

    stream
        .play()
        .map_err(|error| format!("failed to start output stream: {error}"))?;

    match event_rx
        .recv()
        .map_err(|_| "audio stream stopped before playback completed".to_owned())?
    {
        PlaybackEvent::Finished => {
            std::thread::sleep(drain_time);
            Ok(())
        }
        PlaybackEvent::StreamError(error) => Err(format!("audio stream failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(0);

    fn temporary_path(label: &str) -> PathBuf {
        let id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("svmic-{label}-{}-{id}.wav", std::process::id()))
    }

    #[test]
    fn accepts_exactly_one_cli_path() {
        let parsed = parse_audio_path([OsString::from("voice.wav")]).unwrap();
        assert_eq!(parsed, PathBuf::from("voice.wav"));
        assert!(parse_audio_path(Vec::<OsString>::new()).is_err());
        assert!(parse_audio_path([OsString::from("one.wav"), OsString::from("two.wav")]).is_err());
    }

    #[test]
    fn duplicates_mono_samples_into_stereo_frames() {
        let stereo = into_stereo(vec![100, -200, 300], 1).unwrap();
        assert_eq!(stereo, vec![100, 100, -200, -200, 300, 300]);
    }

    #[test]
    fn preserves_existing_stereo_samples() {
        let input = vec![100, -100, 200, -200];
        assert_eq!(into_stereo(input.clone(), 2).unwrap(), input);
    }

    #[test]
    fn matches_only_the_standard_vb_cable_input_name() {
        assert!(is_target_device_name("CABLE Input"));
        assert!(is_target_device_name(
            "CABLE Input (VB-Audio Virtual Cable)"
        ));
        assert!(!is_target_device_name(
            "CABLE In 16ch (VB-Audio Virtual Cable)"
        ));
        assert!(!is_target_device_name("CABLE Input 2"));
    }

    #[test]
    fn rejects_unsupported_wav_specs() {
        let float_spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        assert!(validate_wav_spec(float_spec).is_err());

        let eight_bit_spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 8,
            sample_format: hound::SampleFormat::Int,
        };
        assert!(validate_wav_spec(eight_bit_spec).is_err());

        let surround_spec = hound::WavSpec {
            channels: 6,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        assert!(validate_wav_spec(surround_spec).is_err());
    }

    #[test]
    fn loads_valid_pcm_wav() {
        let path = temporary_path("valid");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        writer.write_sample::<i16>(123).unwrap();
        writer.write_sample::<i16>(-456).unwrap();
        writer.finalize().unwrap();

        let audio = load_wav(&path).unwrap();
        fs::remove_file(&path).unwrap();

        assert_eq!(audio.sample_rate, 16_000);
        assert_eq!(audio.channels, 1);
        assert_eq!(audio.samples, vec![123, -456]);
    }

    #[test]
    fn rejects_missing_and_corrupt_files() {
        let missing = temporary_path("missing");
        assert!(load_wav(&missing).is_err());

        let corrupt = temporary_path("corrupt");
        fs::write(&corrupt, b"not a wave file").unwrap();
        assert!(load_wav(&corrupt).is_err());
        fs::remove_file(&corrupt).unwrap();
    }
}
*/
