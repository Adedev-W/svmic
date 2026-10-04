use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, SupportedBufferSize, SupportedStreamConfig};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapProd, HeapRb};

pub const PCM_SAMPLE_RATE: u32 = 24_000;
const TARGET_DEVICE: &str = "CABLE Input";
const CPAL_TARGET_DEVICE: &str = "CABLE Input (VB-Audio Virtual Cable)";
const OUTPUT_CHANNELS: u16 = 2;
const BUFFER_SECONDS: usize = 4;
const PREBUFFER_MILLISECONDS: usize = 250;
const BUFFER_WAIT: Duration = Duration::from_millis(2);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

pub struct VirtualMicPlayer {
    producer: HeapProd<i16>,
    stream: cpal::Stream,
    resampler: LinearResampler,
    prebuffer_samples: usize,
    started: bool,
    producer_finished: Arc<AtomicBool>,
    drained: Arc<AtomicBool>,
    underflow_frames: Arc<AtomicUsize>,
    stream_error: Arc<Mutex<Option<String>>>,
    drain_time: Duration,
}

impl VirtualMicPlayer {
    pub fn new() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = find_output_device(&host)?;
        let description = device
            .description()
            .map(|description| description.to_string())
            .unwrap_or_else(|_| TARGET_DEVICE.to_owned());
        let supported_config = select_output_config(&device)?;
        let output_sample_rate = supported_config.sample_rate();
        let output_sample_rate_usize = usize::try_from(output_sample_rate)
            .map_err(|_| "output sample rate does not fit this platform".to_owned())?;
        let buffer_capacity = output_sample_rate_usize * BUFFER_SECONDS;
        let prebuffer_samples = output_sample_rate_usize * PREBUFFER_MILLISECONDS / 1_000;
        let drain_time = drain_time(&supported_config);
        let stream_config = supported_config.config();

        let ring = HeapRb::<i16>::new(buffer_capacity);
        let (producer, mut consumer) = ring.split();
        let producer_finished = Arc::new(AtomicBool::new(false));
        let drained = Arc::new(AtomicBool::new(false));
        let underflow_frames = Arc::new(AtomicUsize::new(0));
        let stream_error = Arc::new(Mutex::new(None));

        let callback_finished = Arc::clone(&producer_finished);
        let callback_drained = Arc::clone(&drained);
        let callback_underflows = Arc::clone(&underflow_frames);
        let callback_error = Arc::clone(&stream_error);

        let stream = device
            .build_output_stream(
                stream_config,
                move |output: &mut [i16], _| {
                    let missing = fill_stereo_output(output, || consumer.try_pop());
                    let finished = callback_finished.load(Ordering::Acquire);
                    if missing > 0 && !finished {
                        callback_underflows.fetch_add(missing, Ordering::Relaxed);
                    }
                    if finished && consumer.is_empty() {
                        callback_drained.store(true, Ordering::Release);
                    }
                },
                move |error| {
                    if let Ok(mut slot) = callback_error.lock()
                        && slot.is_none()
                    {
                        *slot = Some(error.to_string());
                    }
                },
                None,
            )
            .map_err(|error| format!("failed to create output stream: {error}"))?;

        eprintln!("Virtual mic: {description}");
        eprintln!(
            "Audio output: {} Hz, {} channels, {:?}",
            output_sample_rate,
            supported_config.channels(),
            supported_config.sample_format()
        );

        Ok(Self {
            producer,
            stream,
            resampler: LinearResampler::new(PCM_SAMPLE_RATE, output_sample_rate),
            prebuffer_samples,
            started: false,
            producer_finished,
            drained,
            underflow_frames,
            stream_error,
            drain_time,
        })
    }

    pub async fn push_pcm_samples(&mut self, samples: &[i16]) -> Result<(), String> {
        self.check_stream_error()?;
        let output_samples = self.resampler.process(samples);
        self.push_output_samples(&output_samples).await
    }

    pub async fn finish(&mut self) -> Result<(), String> {
        let tail = self.resampler.finish();
        self.push_output_samples(&tail).await?;
        self.producer_finished.store(true, Ordering::Release);
        self.ensure_started()?;

        let deadline = Instant::now() + DRAIN_TIMEOUT;
        while !self.drained.load(Ordering::Acquire) {
            self.check_stream_error()?;
            if Instant::now() >= deadline {
                return Err("timed out while draining audio to CABLE Input".to_owned());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        tokio::time::sleep(self.drain_time).await;
        self.check_stream_error()?;
        let underflows = self.underflow_frames.load(Ordering::Relaxed);
        if underflows > 0 {
            eprintln!(
                "Warning: streaming audio underflowed by {underflows} frame(s); silence was inserted."
            );
        }
        Ok(())
    }

    async fn push_output_samples(&mut self, samples: &[i16]) -> Result<(), String> {
        let mut cursor = 0;
        while cursor < samples.len() {
            self.check_stream_error()?;
            let pushed = self.producer.push_slice(&samples[cursor..]);
            cursor += pushed;
            self.start_if_prebuffered()?;
            if pushed == 0 {
                tokio::time::sleep(BUFFER_WAIT).await;
            }
        }
        Ok(())
    }

    fn start_if_prebuffered(&mut self) -> Result<(), String> {
        if !self.started
            && should_start(self.producer.occupied_len(), self.prebuffer_samples, false)
        {
            self.ensure_started()?;
        }
        Ok(())
    }

    fn ensure_started(&mut self) -> Result<(), String> {
        if !self.started {
            self.stream
                .play()
                .map_err(|error| format!("failed to start output stream: {error}"))?;
            self.started = true;
            eprintln!("Playback started on CABLE Input.");
        }
        Ok(())
    }

    fn check_stream_error(&self) -> Result<(), String> {
        let error = self
            .stream_error
            .lock()
            .map_err(|_| "audio stream error state was poisoned".to_owned())?
            .clone();
        match error {
            Some(error) => Err(format!("audio stream failed: {error}")),
            None => Ok(()),
        }
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

fn select_output_config(device: &Device) -> Result<SupportedStreamConfig, String> {
    let ranges = device
        .supported_output_configs()
        .map_err(|error| format!("failed to query '{TARGET_DEVICE}' configurations: {error}"))?
        .filter(|config| {
            config.channels() == OUTPUT_CHANNELS && config.sample_format() == SampleFormat::I16
        })
        .collect::<Vec<_>>();

    if let Some(config) = ranges
        .iter()
        .find_map(|range| range.try_with_sample_rate(PCM_SAMPLE_RATE))
    {
        return Ok(config);
    }

    if let Ok(default) = device.default_output_config()
        && default.channels() == OUTPUT_CHANNELS
        && default.sample_format() == SampleFormat::I16
        && ranges
            .iter()
            .any(|range| range.try_with_sample_rate(default.sample_rate()).is_some())
    {
        return Ok(default);
    }

    ranges
        .iter()
        .filter_map(|range| {
            let rate = PCM_SAMPLE_RATE.clamp(range.min_sample_rate(), range.max_sample_rate());
            range
                .try_with_sample_rate(rate)
                .map(|config| (rate.abs_diff(PCM_SAMPLE_RATE), config))
        })
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, config)| config)
        .ok_or_else(|| {
            format!("'{TARGET_DEVICE}' has no supported stereo 16-bit output configuration")
        })
}

fn drain_time(config: &SupportedStreamConfig) -> Duration {
    let seconds = match config.buffer_size() {
        SupportedBufferSize::Range { max, .. } => {
            (f64::from(*max) / f64::from(config.sample_rate())) * 2.0
        }
        SupportedBufferSize::Unknown => 0.1,
    };
    Duration::from_secs_f64(seconds.clamp(0.05, 0.5))
}

fn fill_stereo_output(output: &mut [i16], mut next_sample: impl FnMut() -> Option<i16>) -> usize {
    let mut missing_frames = 0;
    let mut frames = output.chunks_exact_mut(usize::from(OUTPUT_CHANNELS));
    for frame in &mut frames {
        let sample = next_sample().unwrap_or_else(|| {
            missing_frames += 1;
            0
        });
        frame.fill(sample);
    }
    frames.into_remainder().fill(0);
    missing_frames
}

fn should_start(occupied_samples: usize, prebuffer_samples: usize, finished: bool) -> bool {
    occupied_samples >= prebuffer_samples || finished
}

#[derive(Debug)]
struct LinearResampler {
    input_rate: f64,
    output_rate: f64,
    previous: Option<i16>,
    input_position: u64,
    next_output_position: f64,
}

impl LinearResampler {
    fn new(input_rate: u32, output_rate: u32) -> Self {
        Self {
            input_rate: f64::from(input_rate),
            output_rate: f64::from(output_rate),
            previous: None,
            input_position: 0,
            next_output_position: 0.0,
        }
    }

    fn process(&mut self, samples: &[i16]) -> Vec<i16> {
        let mut output = Vec::with_capacity(
            (samples.len() as f64 * self.output_rate / self.input_rate).ceil() as usize + 1,
        );
        let step = self.input_rate / self.output_rate;

        for &sample in samples {
            match self.previous {
                None => {
                    output.push(sample);
                    self.next_output_position = step;
                }
                Some(previous) => {
                    let interval_end = self.input_position as f64;
                    let interval_start = interval_end - 1.0;
                    while self.next_output_position <= interval_end {
                        let fraction = self.next_output_position - interval_start;
                        output.push(interpolate(previous, sample, fraction));
                        self.next_output_position += step;
                    }
                }
            }
            self.previous = Some(sample);
            self.input_position += 1;
        }

        output
    }

    fn finish(&mut self) -> Vec<i16> {
        let mut output = Vec::new();
        let Some(last) = self.previous else {
            return output;
        };
        let step = self.input_rate / self.output_rate;
        while self.next_output_position < self.input_position as f64 {
            output.push(last);
            self.next_output_position += step;
        }
        output
    }
}

fn interpolate(start: i16, end: i16, fraction: f64) -> i16 {
    let value = f64::from(start) + (f64::from(end) - f64::from(start)) * fraction;
    value
        .round()
        .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_vb_cable_render_endpoint() {
        assert!(is_target_device_name("CABLE Input"));
        assert!(is_target_device_name(
            "CABLE Input (VB-Audio Virtual Cable)"
        ));
        assert!(!is_target_device_name("CABLE Output"));
        assert!(!is_target_device_name("Speakers"));
    }

    #[test]
    fn fills_stereo_and_uses_silence_on_underflow() {
        let mut samples = vec![100_i16, -200].into_iter();
        let mut output = [9_i16; 6];
        let missing = fill_stereo_output(&mut output, || samples.next());
        assert_eq!(output, [100, 100, -200, -200, 0, 0]);
        assert_eq!(missing, 1);
    }

    #[test]
    fn prebuffer_starts_at_threshold_or_when_finished() {
        assert!(!should_start(5, 10, false));
        assert!(should_start(10, 10, false));
        assert!(should_start(1, 10, true));
    }

    #[test]
    fn resampler_passes_through_24_khz() {
        let mut resampler = LinearResampler::new(24_000, 24_000);
        let mut output = resampler.process(&[0, 100, -100, 200]);
        output.extend(resampler.finish());
        assert_eq!(output, [0, 100, -100, 200]);
    }

    #[test]
    fn resampler_upsamples_24_to_48_khz() {
        let mut resampler = LinearResampler::new(24_000, 48_000);
        let mut output = resampler.process(&[0, 100, 200]);
        output.extend(resampler.finish());
        assert_eq!(output, [0, 50, 100, 150, 200, 200]);
    }

    #[test]
    fn resampler_downsamples_24_to_16_khz() {
        let mut resampler = LinearResampler::new(24_000, 16_000);
        let mut output = resampler.process(&[0, 100, 200]);
        output.extend(resampler.finish());
        assert_eq!(output, [0, 150]);
    }

    #[test]
    fn resampler_preserves_state_across_chunks() {
        let mut chunked = LinearResampler::new(24_000, 48_000);
        let mut chunked_output = chunked.process(&[0, 100]);
        chunked_output.extend(chunked.process(&[200, 300]));
        chunked_output.extend(chunked.finish());

        let mut whole = LinearResampler::new(24_000, 48_000);
        let mut whole_output = whole.process(&[0, 100, 200, 300]);
        whole_output.extend(whole.finish());
        assert_eq!(chunked_output, whole_output);
    }
}
