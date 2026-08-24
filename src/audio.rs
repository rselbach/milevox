use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, HostId, Sample, SampleFormat, SizedSample, Stream, StreamConfig};

const TARGET_CAPTURE_RATE: u32 = 16_000;
const SAMPLE_CHUNK_SIZE: usize = 16_384;
const MAX_RECORDING_SECONDS: usize = 10 * 60;

pub struct Recording {
    stream: Stream,
    samples: Arc<Mutex<ChunkedSamples>>,
    stream_error: Arc<Mutex<Option<String>>>,
    sample_rate: u32,
}

#[derive(Clone)]
pub struct RecordingReader {
    samples: Arc<Mutex<ChunkedSamples>>,
    stream_error: Arc<Mutex<Option<String>>>,
    sample_rate: u32,
}

#[derive(Clone)]
pub struct CapturedAudio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

struct ChunkedSamples {
    chunks: Vec<Arc<[f32]>>,
    current: Vec<f32>,
    len: usize,
    max_len: usize,
}

struct Snapshot {
    chunks: Vec<Arc<[f32]>>,
    first_offset: usize,
    len: usize,
}

impl ChunkedSamples {
    fn new(sample_rate: u32) -> Self {
        Self {
            chunks: Vec::new(),
            current: Vec::with_capacity(SAMPLE_CHUNK_SIZE),
            len: 0,
            max_len: usize::try_from(sample_rate)
                .unwrap_or(usize::MAX)
                .saturating_mul(MAX_RECORDING_SECONDS),
        }
    }

    fn push(&mut self, sample: f32) -> bool {
        if self.len >= self.max_len {
            return false;
        }
        self.current.push(sample);
        self.len += 1;
        if self.current.len() == SAMPLE_CHUNK_SIZE {
            let full = std::mem::replace(&mut self.current, Vec::with_capacity(SAMPLE_CHUNK_SIZE));
            self.chunks.push(Arc::from(full));
        }
        true
    }

    fn snapshot(&self, start: usize) -> Snapshot {
        let start = start.min(self.len);
        let first_chunk = start / SAMPLE_CHUNK_SIZE;
        let first_offset = start % SAMPLE_CHUNK_SIZE;
        let mut chunks = self.chunks[first_chunk.min(self.chunks.len())..].to_vec();
        if !self.current.is_empty() {
            chunks.push(Arc::from(self.current.clone()));
        }
        Snapshot {
            chunks,
            first_offset,
            len: self.len - start,
        }
    }
}

impl Snapshot {
    fn flatten(self) -> Vec<f32> {
        let mut output = Vec::with_capacity(self.len);
        for (index, chunk) in self.chunks.iter().enumerate() {
            let start = if index == 0 { self.first_offset } else { 0 };
            if start < chunk.len() {
                output.extend_from_slice(&chunk[start..]);
            }
        }
        output.truncate(self.len);
        output
    }
}

impl Recording {
    pub fn start() -> Result<Self> {
        let host = cpal::host_from_id(HostId::PipeWire)
            .context("PipeWire audio is unavailable in this build")?;
        let device = host
            .default_input_device()
            .context("PipeWire has no default input device")?;
        let supported = device
            .default_input_config()
            .context("failed to read the default microphone format")?;
        let sample_format = supported.sample_format();
        let mut config: StreamConfig = supported.into();
        if supports_rate(&device, sample_format, config.channels, TARGET_CAPTURE_RATE)? {
            config.sample_rate = TARGET_CAPTURE_RATE;
        }
        let sample_rate = config.sample_rate;
        let channels = usize::from(config.channels);
        let samples = Arc::new(Mutex::new(ChunkedSamples::new(sample_rate)));
        let stream_error = Arc::new(Mutex::new(None));

        let stream = match sample_format {
            SampleFormat::I8 => {
                build_stream::<i8>(&device, &config, channels, &samples, &stream_error)
            }
            SampleFormat::I16 => {
                build_stream::<i16>(&device, &config, channels, &samples, &stream_error)
            }
            SampleFormat::I32 => {
                build_stream::<i32>(&device, &config, channels, &samples, &stream_error)
            }
            SampleFormat::I64 => {
                build_stream::<i64>(&device, &config, channels, &samples, &stream_error)
            }
            SampleFormat::U8 => {
                build_stream::<u8>(&device, &config, channels, &samples, &stream_error)
            }
            SampleFormat::U16 => {
                build_stream::<u16>(&device, &config, channels, &samples, &stream_error)
            }
            SampleFormat::U32 => {
                build_stream::<u32>(&device, &config, channels, &samples, &stream_error)
            }
            SampleFormat::U64 => {
                build_stream::<u64>(&device, &config, channels, &samples, &stream_error)
            }
            SampleFormat::F32 => {
                build_stream::<f32>(&device, &config, channels, &samples, &stream_error)
            }
            SampleFormat::F64 => {
                build_stream::<f64>(&device, &config, channels, &samples, &stream_error)
            }
            other => bail!("unsupported microphone sample format: {other}"),
        }?;

        stream
            .play()
            .context("failed to start microphone capture")?;

        Ok(Self {
            stream,
            samples,
            stream_error,
            sample_rate,
        })
    }

    pub fn reader(&self) -> RecordingReader {
        RecordingReader {
            samples: Arc::clone(&self.samples),
            stream_error: Arc::clone(&self.stream_error),
            sample_rate: self.sample_rate,
        }
    }

    pub fn finish(self) -> Result<CapturedAudio> {
        drop(self.stream);
        if let Some(error) = stream_error(&self.stream_error)? {
            bail!("microphone capture failed: {error}");
        }
        let snapshot = self
            .samples
            .lock()
            .map_err(|_| anyhow::anyhow!("microphone sample buffer was poisoned"))?
            .snapshot(0);
        let samples = snapshot.flatten();
        if samples.is_empty() {
            bail!("the microphone captured no audio");
        }
        Ok(CapturedAudio {
            samples,
            sample_rate: self.sample_rate,
        })
    }
}

impl RecordingReader {
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn snapshot_from(&self, start: usize) -> Result<CapturedAudio> {
        let snapshot = self
            .samples
            .lock()
            .map_err(|_| anyhow::anyhow!("microphone sample buffer was poisoned"))?
            .snapshot(start);
        Ok(CapturedAudio {
            samples: snapshot.flatten(),
            sample_rate: self.sample_rate,
        })
    }

    pub fn sample_count(&self) -> Result<usize> {
        Ok(self
            .samples
            .lock()
            .map_err(|_| anyhow::anyhow!("microphone sample buffer was poisoned"))?
            .len)
    }

    pub fn stream_error(&self) -> Result<Option<String>> {
        stream_error(&self.stream_error)
    }

    pub fn level(&self) -> Result<f32> {
        let count = self.sample_count()?;
        let window = usize::try_from(self.sample_rate / 10).unwrap_or(usize::MAX);
        let audio = self.snapshot_from(count.saturating_sub(window))?;
        Ok(normalized_level(&audio.samples))
    }
}

fn stream_error(error: &Mutex<Option<String>>) -> Result<Option<String>> {
    Ok(error
        .lock()
        .map_err(|_| anyhow::anyhow!("microphone error state was poisoned"))?
        .clone())
}

fn supports_rate(
    device: &cpal::Device,
    format: SampleFormat,
    channels: u16,
    rate: u32,
) -> Result<bool> {
    let supported = device
        .supported_input_configs()
        .context("failed to inspect supported microphone formats")?;
    Ok(supported.into_iter().any(|range| {
        range.sample_format() == format
            && range.channels() == channels
            && range.min_sample_rate() <= rate
            && range.max_sample_rate() >= rate
    }))
}

fn normalized_level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mean_square =
        samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32;
    let rms = mean_square.sqrt();
    if rms <= 0.003 {
        return 0.0;
    }
    ((rms - 0.003) * 10.0).sqrt().clamp(0.0, 1.0)
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    channels: usize,
    samples: &Arc<Mutex<ChunkedSamples>>,
    stream_error: &Arc<Mutex<Option<String>>>,
) -> Result<Stream>
where
    T: Sample + SizedSample,
    f32: FromSample<T>,
{
    let output = Arc::clone(samples);
    let errors = Arc::clone(stream_error);
    let callback_errors = Arc::clone(stream_error);
    let stream = device.build_input_stream(
        *config,
        move |input: &[T], _| {
            let Ok(mut output) = output.lock() else {
                return;
            };
            for frame in input.chunks(channels) {
                let sum = frame.iter().copied().map(f32::from_sample).sum::<f32>();
                if !output.push(sum / channels as f32) {
                    if let Ok(mut slot) = callback_errors.lock()
                        && slot.is_none()
                    {
                        *slot = Some(format!(
                            "maximum recording duration of {MAX_RECORDING_SECONDS} seconds reached"
                        ));
                    }
                    return;
                }
            }
        },
        move |error| {
            eprintln!("Milevox microphone stream error: {error}");
            if let Ok(mut slot) = errors.lock() {
                *slot = Some(error.to_string());
            }
        },
        None,
    )?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked_snapshots_flatten_after_the_lock_and_from_an_offset() {
        let mut samples = ChunkedSamples::new(16_000);
        for index in 0..(SAMPLE_CHUNK_SIZE + 20) {
            assert!(samples.push(index as f32));
        }
        let snapshot = samples.snapshot(SAMPLE_CHUNK_SIZE - 2);

        assert_eq!(snapshot.len, 22);
        assert_eq!(
            snapshot.flatten(),
            (SAMPLE_CHUNK_SIZE - 2..SAMPLE_CHUNK_SIZE + 20)
                .map(|index| index as f32)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn chunked_storage_enforces_its_sample_budget() {
        let mut samples = ChunkedSamples::new(1);
        for _ in 0..MAX_RECORDING_SECONDS {
            assert!(samples.push(0.0));
        }
        assert!(!samples.push(0.0));
    }

    #[test]
    fn audio_level_ignores_the_noise_floor_and_is_bounded() {
        assert_eq!(normalized_level(&[0.001, -0.001, 0.002]), 0.0);
        let moderate = normalized_level(&[0.05, -0.05]);
        assert!(moderate > 0.0 && moderate < 1.0);
        assert_eq!(normalized_level(&[1.0, -1.0]), 1.0);
    }

    #[test]
    fn polling_a_stream_error_does_not_hide_it_from_stop() {
        let error = Mutex::new(Some("device disconnected".to_owned()));

        assert_eq!(
            stream_error(&error).unwrap().as_deref(),
            Some("device disconnected")
        );
        assert_eq!(
            stream_error(&error).unwrap().as_deref(),
            Some("device disconnected")
        );
    }
}
