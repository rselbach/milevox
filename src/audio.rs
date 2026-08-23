use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, HostId, Sample, SampleFormat, SizedSample, Stream, StreamConfig};

pub struct Recording {
    stream: Stream,
    samples: Arc<Mutex<Vec<f32>>>,
    stream_error: Arc<Mutex<Option<String>>>,
    sample_rate: u32,
}

#[derive(Clone)]
pub struct RecordingReader {
    samples: Arc<Mutex<Vec<f32>>>,
    sample_rate: u32,
}

pub struct CapturedAudio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
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
        let config: StreamConfig = supported.into();
        let sample_rate = config.sample_rate;
        let channels = usize::from(config.channels);
        let samples = Arc::new(Mutex::new(Vec::new()));
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
            sample_rate: self.sample_rate,
        }
    }

    pub fn finish(self) -> Result<CapturedAudio> {
        drop(self.stream);

        if let Some(error) = self
            .stream_error
            .lock()
            .map_err(|_| anyhow::anyhow!("microphone error state was poisoned"))?
            .take()
        {
            bail!("microphone capture failed: {error}");
        }

        let samples = std::mem::take(
            &mut *self
                .samples
                .lock()
                .map_err(|_| anyhow::anyhow!("microphone sample buffer was poisoned"))?,
        );

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
    pub fn snapshot(&self) -> Result<CapturedAudio> {
        let samples = self
            .samples
            .lock()
            .map_err(|_| anyhow::anyhow!("microphone sample buffer was poisoned"))?
            .clone();
        Ok(CapturedAudio {
            samples,
            sample_rate: self.sample_rate,
        })
    }

    pub fn sample_count(&self) -> Result<usize> {
        Ok(self
            .samples
            .lock()
            .map_err(|_| anyhow::anyhow!("microphone sample buffer was poisoned"))?
            .len())
    }

    pub fn level(&self) -> Result<f32> {
        let samples = self
            .samples
            .lock()
            .map_err(|_| anyhow::anyhow!("microphone sample buffer was poisoned"))?;
        let window = usize::try_from(self.sample_rate / 10).unwrap_or(usize::MAX);
        let start = samples.len().saturating_sub(window);
        Ok(normalized_level(&samples[start..]))
    }
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
    samples: &Arc<Mutex<Vec<f32>>>,
    stream_error: &Arc<Mutex<Option<String>>>,
) -> Result<Stream>
where
    T: Sample + SizedSample,
    f32: FromSample<T>,
{
    let output = Arc::clone(samples);
    let errors = Arc::clone(stream_error);
    let stream = device.build_input_stream(
        *config,
        move |input: &[T], _| {
            let Ok(mut output) = output.lock() else {
                return;
            };
            for frame in input.chunks(channels) {
                let sum = frame.iter().copied().map(f32::from_sample).sum::<f32>();
                output.push(sum / channels as f32);
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
    fn audio_level_ignores_the_noise_floor() {
        assert_eq!(normalized_level(&[0.001, -0.001, 0.002]), 0.0);
    }

    #[test]
    fn audio_level_is_bounded() {
        let moderate = normalized_level(&[0.05, -0.05]);
        let loud = normalized_level(&[1.0, -1.0]);

        assert!(moderate > 0.0 && moderate < 1.0);
        assert_eq!(loud, 1.0);
    }
}
