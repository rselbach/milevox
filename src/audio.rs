use std::fmt;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, HostId, Sample, SampleFormat, SizedSample, Stream, StreamConfig};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};

const TARGET_CAPTURE_RATE: u32 = 16_000;
const SAMPLE_CHUNK_SIZE: usize = 16_384;
const MAX_RECORDING_SECONDS: usize = 10 * 60;
const CAPTURE_BLOCK_FRAMES: usize = 4_096;
const CAPTURE_QUEUE_DEPTH: usize = 16;
const RESAMPLER_CHUNK_SIZE: usize = 1_024;

pub struct Recording {
    source: RecordingSource,
    samples: Arc<Mutex<ChunkedSamples>>,
    capture_issue: Arc<Mutex<Option<CaptureIssue>>>,
    sample_rate: u32,
}

enum RecordingSource {
    Live {
        stream: Stream,
        worker: JoinHandle<()>,
    },
    #[cfg(test)]
    Test,
}

#[derive(Clone)]
pub struct RecordingReader {
    samples: Arc<Mutex<ChunkedSamples>>,
    capture_issue: Arc<Mutex<Option<CaptureIssue>>>,
    sample_rate: u32,
}

#[derive(Clone, Debug)]
pub struct CapturedAudio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

#[derive(Debug)]
pub struct FinishedCapture {
    pub audio: CapturedAudio,
    pub warning: Option<CaptureIssue>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureIssue {
    DurationLimitReached,
    Device(String),
    WorkerOverflow,
}

impl fmt::Display for CaptureIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DurationLimitReached => write!(
                formatter,
                "maximum recording duration of {MAX_RECORDING_SECONDS} seconds reached"
            ),
            Self::Device(error) => write!(formatter, "microphone device failed: {error}"),
            Self::WorkerOverflow => {
                write!(formatter, "microphone capture worker could not keep up")
            }
        }
    }
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

struct CaptureResampler {
    inner: Option<Fft<f32>>,
    pending: Vec<f32>,
    pending_start: usize,
    frames_to_trim: usize,
    input_frames: usize,
    output_frames: usize,
    source_rate: u32,
}

impl CaptureResampler {
    fn new(source_rate: u32) -> Result<Self> {
        if source_rate == 0 {
            bail!("microphone sample rate must be greater than zero");
        }
        let inner = if source_rate == TARGET_CAPTURE_RATE {
            None
        } else {
            Some(
                Fft::<f32>::new(
                    source_rate as usize,
                    TARGET_CAPTURE_RATE as usize,
                    RESAMPLER_CHUNK_SIZE,
                    1,
                    FixedSync::Both,
                )
                .context("failed to create the capture resampler")?,
            )
        };
        let frames_to_trim = inner.as_ref().map_or(0, Resampler::output_delay);
        Ok(Self {
            inner,
            pending: Vec::with_capacity(RESAMPLER_CHUNK_SIZE * 2),
            pending_start: 0,
            frames_to_trim,
            input_frames: 0,
            output_frames: 0,
            source_rate,
        })
    }

    fn push(&mut self, input: &[f32]) -> Result<Vec<f32>> {
        self.input_frames = self.input_frames.saturating_add(input.len());
        if self.inner.is_none() {
            self.output_frames = self.output_frames.saturating_add(input.len());
            return Ok(input.to_vec());
        }

        self.pending.extend_from_slice(input);
        let mut output = Vec::new();
        loop {
            let frames = self
                .inner
                .as_ref()
                .expect("resampler was checked above")
                .input_frames_next();
            if self.pending.len() - self.pending_start < frames {
                break;
            }
            let end = self.pending_start + frames;
            let resampled = {
                let input =
                    InterleavedSlice::new(&self.pending[self.pending_start..end], 1, frames)
                        .context("failed to prepare a microphone block for resampling")?;
                self.inner
                    .as_mut()
                    .expect("resampler was checked above")
                    .process(&input, None)
                    .context("failed to resample a microphone block")?
                    .take_data()
            };
            self.pending_start = end;
            self.append_output(resampled, None, &mut output);
        }
        self.compact_pending();
        Ok(output)
    }

    fn finish(&mut self) -> Result<Vec<f32>> {
        let Some(_) = self.inner else {
            return Ok(Vec::new());
        };
        let target_frames = usize::try_from(
            (self.input_frames as u64 * u64::from(TARGET_CAPTURE_RATE))
                .div_ceil(u64::from(self.source_rate)),
        )
        .unwrap_or(usize::MAX);
        let mut output = Vec::new();
        let remaining = self.pending.len() - self.pending_start;
        if remaining > 0 {
            let resampled = {
                let input =
                    InterleavedSlice::new(&self.pending[self.pending_start..], 1, remaining)
                        .context("failed to prepare the final microphone block for resampling")?;
                let indexing = Indexing::new().partial_len(remaining);
                self.inner
                    .as_mut()
                    .expect("resampler was checked above")
                    .process(&input, Some(&indexing))
                    .context("failed to resample the final microphone block")?
                    .take_data()
            };
            self.pending_start = self.pending.len();
            self.append_output(resampled, Some(target_frames), &mut output);
        }

        while self.output_frames < target_frames {
            let resampled = {
                let empty = InterleavedSlice::new(&[], 1, 0)
                    .context("failed to prepare capture resampler padding")?;
                let indexing = Indexing::new().partial_len(0);
                self.inner
                    .as_mut()
                    .expect("resampler was checked above")
                    .process(&empty, Some(&indexing))
                    .context("failed to flush the capture resampler")?
                    .take_data()
            };
            self.append_output(resampled, Some(target_frames), &mut output);
        }
        Ok(output)
    }

    fn append_output(
        &mut self,
        samples: Vec<f32>,
        target_frames: Option<usize>,
        output: &mut Vec<f32>,
    ) {
        let trim = self.frames_to_trim.min(samples.len());
        self.frames_to_trim -= trim;
        let mut useful = &samples[trim..];
        if let Some(target_frames) = target_frames {
            useful = &useful[..useful
                .len()
                .min(target_frames.saturating_sub(self.output_frames))];
        }
        output.extend_from_slice(useful);
        self.output_frames = self.output_frames.saturating_add(useful.len());
    }

    fn compact_pending(&mut self) {
        if self.pending_start == 0 {
            return;
        }
        self.pending.drain(..self.pending_start);
        self.pending_start = 0;
    }
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

    fn level(&self, window: usize) -> f32 {
        let start = self.len.saturating_sub(window);
        let mut skip = start;
        let mut sum = 0.0;
        let mut count = 0;

        for samples in self
            .chunks
            .iter()
            .map(AsRef::as_ref)
            .chain(std::iter::once(self.current.as_slice()))
        {
            let offset = skip.min(samples.len());
            skip -= offset;
            let samples = &samples[offset..];
            sum += samples.iter().map(|sample| sample * sample).sum::<f32>();
            count += samples.len();
        }

        normalized_level_from_sum(sum, count)
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
        let source_rate = config.sample_rate;
        let channels = usize::from(config.channels);
        let samples = Arc::new(Mutex::new(ChunkedSamples::new(TARGET_CAPTURE_RATE)));
        let capture_issue = Arc::new(Mutex::new(None));
        let resampler = CaptureResampler::new(source_rate)?;
        let (filled_sender, filled_receiver) = sync_channel(CAPTURE_QUEUE_DEPTH);
        let (free_sender, free_receiver) = sync_channel(CAPTURE_QUEUE_DEPTH);
        for _ in 0..CAPTURE_QUEUE_DEPTH {
            free_sender
                .send(Vec::with_capacity(CAPTURE_BLOCK_FRAMES))
                .expect("the capture buffer pool receiver is still alive");
        }

        let stream = match sample_format {
            SampleFormat::I8 => build_stream::<i8>(
                &device,
                &config,
                channels,
                filled_sender,
                free_receiver,
                &capture_issue,
            ),
            SampleFormat::I16 => build_stream::<i16>(
                &device,
                &config,
                channels,
                filled_sender,
                free_receiver,
                &capture_issue,
            ),
            SampleFormat::I32 => build_stream::<i32>(
                &device,
                &config,
                channels,
                filled_sender,
                free_receiver,
                &capture_issue,
            ),
            SampleFormat::I64 => build_stream::<i64>(
                &device,
                &config,
                channels,
                filled_sender,
                free_receiver,
                &capture_issue,
            ),
            SampleFormat::U8 => build_stream::<u8>(
                &device,
                &config,
                channels,
                filled_sender,
                free_receiver,
                &capture_issue,
            ),
            SampleFormat::U16 => build_stream::<u16>(
                &device,
                &config,
                channels,
                filled_sender,
                free_receiver,
                &capture_issue,
            ),
            SampleFormat::U32 => build_stream::<u32>(
                &device,
                &config,
                channels,
                filled_sender,
                free_receiver,
                &capture_issue,
            ),
            SampleFormat::U64 => build_stream::<u64>(
                &device,
                &config,
                channels,
                filled_sender,
                free_receiver,
                &capture_issue,
            ),
            SampleFormat::F32 => build_stream::<f32>(
                &device,
                &config,
                channels,
                filled_sender,
                free_receiver,
                &capture_issue,
            ),
            SampleFormat::F64 => build_stream::<f64>(
                &device,
                &config,
                channels,
                filled_sender,
                free_receiver,
                &capture_issue,
            ),
            other => bail!("unsupported microphone sample format: {other}"),
        }?;

        let worker_samples = Arc::clone(&samples);
        let worker_issue = Arc::clone(&capture_issue);
        let worker = thread::Builder::new()
            .name("milevox-capture".to_owned())
            .spawn(move || {
                capture_worker(
                    resampler,
                    filled_receiver,
                    free_sender,
                    worker_samples,
                    worker_issue,
                );
            })
            .context("failed to start the microphone capture worker")?;

        stream
            .play()
            .context("failed to start microphone capture")?;

        Ok(Self {
            source: RecordingSource::Live { stream, worker },
            samples,
            capture_issue,
            sample_rate: TARGET_CAPTURE_RATE,
        })
    }

    pub fn reader(&self) -> RecordingReader {
        RecordingReader {
            samples: Arc::clone(&self.samples),
            capture_issue: Arc::clone(&self.capture_issue),
            sample_rate: self.sample_rate,
        }
    }

    pub fn finish(self) -> Result<FinishedCapture> {
        let Self {
            source,
            samples,
            capture_issue: issue,
            sample_rate,
        } = self;
        match source {
            RecordingSource::Live { stream, worker } => {
                drop(stream);
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("microphone capture worker panicked"))?;
            }
            #[cfg(test)]
            RecordingSource::Test => {}
        }
        finish_capture(&samples, sample_rate, capture_issue(&issue)?)
    }

    #[cfg(test)]
    pub(crate) fn test_capture(samples: Vec<f32>, warning: Option<CaptureIssue>) -> Self {
        let mut captured = ChunkedSamples::new(TARGET_CAPTURE_RATE);
        for sample in samples {
            assert!(captured.push(sample));
        }
        Self {
            source: RecordingSource::Test,
            samples: Arc::new(Mutex::new(captured)),
            capture_issue: Arc::new(Mutex::new(warning)),
            sample_rate: TARGET_CAPTURE_RATE,
        }
    }
}

fn finish_capture(
    captured: &Mutex<ChunkedSamples>,
    sample_rate: u32,
    warning: Option<CaptureIssue>,
) -> Result<FinishedCapture> {
    let samples = captured
        .lock()
        .map_err(|_| anyhow::anyhow!("microphone sample buffer was poisoned"))?
        .snapshot(0)
        .flatten();
    if samples.is_empty() {
        if let Some(issue) = &warning {
            bail!("microphone capture failed before recording usable audio: {issue}");
        }
        bail!("the microphone captured no audio");
    }
    Ok(FinishedCapture {
        audio: CapturedAudio {
            samples,
            sample_rate,
        },
        warning,
    })
}

impl RecordingReader {
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn snapshot_range(&self, start: usize, end: usize) -> Result<CapturedAudio> {
        let samples = self
            .samples
            .lock()
            .map_err(|_| anyhow::anyhow!("microphone sample buffer was poisoned"))?;
        let end = end.min(samples.len);
        let start = start.min(end);
        let mut snapshot = samples.snapshot(start);
        snapshot.len = end - start;
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

    pub fn capture_issue(&self) -> Result<Option<CaptureIssue>> {
        capture_issue(&self.capture_issue)
    }

    pub fn level(&self) -> Result<f32> {
        let window = usize::try_from(self.sample_rate / 10).unwrap_or(usize::MAX);
        Ok(self
            .samples
            .lock()
            .map_err(|_| anyhow::anyhow!("microphone sample buffer was poisoned"))?
            .level(window))
    }
}

fn capture_issue(issue: &Mutex<Option<CaptureIssue>>) -> Result<Option<CaptureIssue>> {
    Ok(issue
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

#[cfg(test)]
fn normalized_level(samples: &[f32]) -> f32 {
    let sum = samples.iter().map(|sample| sample * sample).sum::<f32>();
    normalized_level_from_sum(sum, samples.len())
}

fn normalized_level_from_sum(sum: f32, count: usize) -> f32 {
    if count == 0 {
        return 0.0;
    }
    let mean_square = sum / count as f32;
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
    filled_sender: SyncSender<Vec<f32>>,
    free_receiver: Receiver<Vec<f32>>,
    capture_issue: &Arc<Mutex<Option<CaptureIssue>>>,
) -> Result<Stream>
where
    T: Sample + SizedSample,
    f32: FromSample<T>,
{
    let errors = Arc::clone(capture_issue);
    let callback_errors = Arc::clone(capture_issue);
    let stream = device.build_input_stream(
        *config,
        move |input: &[T], _| {
            let mut offset = 0;
            while offset + channels <= input.len() {
                let mut block = match free_receiver.try_recv() {
                    Ok(block) => block,
                    Err(TryRecvError::Empty) => {
                        set_capture_issue(&callback_errors, CaptureIssue::WorkerOverflow);
                        return;
                    }
                    Err(TryRecvError::Disconnected) => {
                        set_capture_issue(
                            &callback_errors,
                            CaptureIssue::Device(
                                "microphone capture worker stopped unexpectedly".to_owned(),
                            ),
                        );
                        return;
                    }
                };
                block.clear();
                let available_frames = (input.len() - offset) / channels;
                let block_frames = available_frames.min(CAPTURE_BLOCK_FRAMES);
                for frame in input[offset..].chunks_exact(channels).take(block_frames) {
                    let sum = frame.iter().copied().map(f32::from_sample).sum::<f32>();
                    block.push(sum / channels as f32);
                }
                offset += block_frames * channels;
                if !enqueue_capture_block(&filled_sender, block, &callback_errors) {
                    return;
                }
            }
        },
        move |error| {
            eprintln!("Milevox microphone stream error: {error}");
            set_capture_issue(&errors, CaptureIssue::Device(error.to_string()));
        },
        None,
    )?;
    Ok(stream)
}

fn enqueue_capture_block(
    sender: &SyncSender<Vec<f32>>,
    block: Vec<f32>,
    capture_issue: &Mutex<Option<CaptureIssue>>,
) -> bool {
    match sender.try_send(block) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            set_capture_issue(capture_issue, CaptureIssue::WorkerOverflow);
            false
        }
        Err(TrySendError::Disconnected(_)) => {
            set_capture_issue(
                capture_issue,
                CaptureIssue::Device("microphone capture worker stopped unexpectedly".to_owned()),
            );
            false
        }
    }
}

fn capture_worker(
    mut resampler: CaptureResampler,
    filled_receiver: Receiver<Vec<f32>>,
    free_sender: SyncSender<Vec<f32>>,
    samples: Arc<Mutex<ChunkedSamples>>,
    capture_issue: Arc<Mutex<Option<CaptureIssue>>>,
) {
    let mut accepting_audio = true;
    while let Ok(mut block) = filled_receiver.recv() {
        if accepting_audio {
            match resampler.push(&block) {
                Ok(output) => {
                    accepting_audio = append_capture_samples(&samples, &capture_issue, &output);
                }
                Err(error) => {
                    set_capture_issue(
                        &capture_issue,
                        CaptureIssue::Device(format!("capture resampling failed: {error:#}")),
                    );
                    accepting_audio = false;
                }
            }
        }
        block.clear();
        let _ = free_sender.try_send(block);
    }

    if accepting_audio {
        match resampler.finish() {
            Ok(output) => {
                append_capture_samples(&samples, &capture_issue, &output);
            }
            Err(error) => set_capture_issue(
                &capture_issue,
                CaptureIssue::Device(format!("capture resampling failed: {error:#}")),
            ),
        }
    }
}

fn append_capture_samples(
    captured: &Mutex<ChunkedSamples>,
    capture_issue: &Mutex<Option<CaptureIssue>>,
    samples: &[f32],
) -> bool {
    let Ok(mut captured) = captured.lock() else {
        set_capture_issue(
            capture_issue,
            CaptureIssue::Device("microphone sample buffer was poisoned".to_owned()),
        );
        return false;
    };
    for &sample in samples {
        if !captured.push(sample) {
            drop(captured);
            set_capture_issue(capture_issue, CaptureIssue::DurationLimitReached);
            return false;
        }
    }
    true
}

fn set_capture_issue(slot: &Mutex<Option<CaptureIssue>>, issue: CaptureIssue) {
    if let Ok(mut slot) = slot.lock()
        && slot.is_none()
    {
        *slot = Some(issue);
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;

    use super::*;

    fn tone(rate: u32, frequency: f32, seconds: u32) -> Vec<f32> {
        (0..usize::try_from(rate * seconds).unwrap())
            .map(|index| (TAU * frequency * index as f32 / rate as f32).sin())
            .collect()
    }

    fn stream_resample(rate: u32, input: &[f32]) -> Vec<f32> {
        let mut resampler = CaptureResampler::new(rate).unwrap();
        let mut output = Vec::new();
        for block in input.chunks(317) {
            output.extend(resampler.push(block).unwrap());
        }
        output.extend(resampler.finish().unwrap());
        output
    }

    fn tone_amplitude(samples: &[f32], rate: u32, frequency: f32) -> f32 {
        let edge = (rate / 20) as usize;
        let samples = &samples[edge..samples.len() - edge];
        let (sine, cosine) =
            samples
                .iter()
                .enumerate()
                .fold((0.0, 0.0), |(sine, cosine), (index, sample)| {
                    let phase = TAU * frequency * (index + edge) as f32 / rate as f32;
                    (sine + sample * phase.sin(), cosine + sample * phase.cos())
                });
        2.0 * sine.hypot(cosine) / samples.len() as f32
    }

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
    fn recording_reader_snapshot_stops_at_the_scheduled_end() {
        let recording =
            Recording::test_capture((0..20).map(|sample| sample as f32).collect(), None);
        let reader = recording.reader();

        let audio = reader.snapshot_range(4, 8).unwrap();

        assert_eq!(audio.samples, [4.0, 5.0, 6.0, 7.0]);
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
    fn direct_chunked_level_matches_a_flattened_tail() {
        for (len, window) in [
            (0, 1),
            (100, 80),
            (SAMPLE_CHUNK_SIZE + 20, 40),
            (SAMPLE_CHUNK_SIZE * 2 + 7, SAMPLE_CHUNK_SIZE + 11),
        ] {
            let mut samples = ChunkedSamples::new(100_000);
            for index in 0..len {
                let sample = if index % 7 == 0 {
                    1.5
                } else {
                    index as f32 / len.max(1) as f32
                };
                assert!(samples.push(sample));
            }
            let start = len.saturating_sub(window);
            let flattened = samples.snapshot(start).flatten();

            assert_eq!(samples.level(window), normalized_level(&flattened));
        }
    }

    #[test]
    fn polling_a_capture_issue_does_not_hide_it_from_stop() {
        let issue = Mutex::new(Some(CaptureIssue::Device("device disconnected".to_owned())));

        assert_eq!(
            capture_issue(&issue).unwrap(),
            Some(CaptureIssue::Device("device disconnected".to_owned()))
        );
        assert_eq!(
            capture_issue(&issue).unwrap(),
            Some(CaptureIssue::Device("device disconnected".to_owned()))
        );
    }

    #[test]
    fn exact_budget_can_be_recovered_in_full() {
        let mut samples = ChunkedSamples::new(1);
        for index in 0..MAX_RECORDING_SECONDS {
            assert!(samples.push(index as f32));
        }

        let finished = finish_capture(&Mutex::new(samples), 1, None).unwrap();

        assert_eq!(finished.audio.samples.len(), MAX_RECORDING_SECONDS);
        assert_eq!(finished.audio.samples[0], 0.0);
        assert_eq!(
            finished.audio.samples[MAX_RECORDING_SECONDS - 1],
            (MAX_RECORDING_SECONDS - 1) as f32
        );
    }

    #[test]
    fn ten_minute_store_stays_within_the_target_rate_memory_budget() {
        let mut samples = ChunkedSamples::new(TARGET_CAPTURE_RATE);
        for _ in 0..samples.max_len {
            assert!(samples.push(0.25));
        }

        let payload_bytes = samples.len * std::mem::size_of::<f32>();
        let allocated_samples = samples
            .chunks
            .iter()
            .map(|chunk| chunk.len())
            .sum::<usize>()
            + samples.current.capacity();
        let allocated_bytes = allocated_samples * std::mem::size_of::<f32>();

        assert_eq!(samples.len, 9_600_000);
        assert_eq!(payload_bytes, 38_400_000);
        assert!(allocated_bytes <= payload_bytes + SAMPLE_CHUNK_SIZE * std::mem::size_of::<f32>());
        assert!(!samples.push(0.25));
    }

    #[test]
    fn capture_issues_salvage_nonempty_audio_but_reject_empty_audio() {
        let mut samples = ChunkedSamples::new(16_000);
        assert!(samples.push(0.25));
        let issue = CaptureIssue::Device("USB microphone disconnected".to_owned());

        let finished = finish_capture(&Mutex::new(samples), 16_000, Some(issue.clone())).unwrap();
        assert_eq!(finished.audio.samples, [0.25]);
        assert_eq!(finished.warning, Some(issue.clone()));

        let error = finish_capture(
            &Mutex::new(ChunkedSamples::new(16_000)),
            16_000,
            Some(issue),
        )
        .unwrap_err();
        assert!(error.to_string().contains("before recording usable audio"));
    }

    #[test]
    fn capture_worker_resamples_common_native_rates_to_exact_16khz_duration() {
        for rate in [48_000, 96_000, 192_000] {
            let input = tone(rate, 1_000.0, 2);
            let output = stream_resample(rate, &input);

            assert_eq!(output.len(), TARGET_CAPTURE_RATE as usize * 2, "{rate}");
            let amplitude = tone_amplitude(&output, TARGET_CAPTURE_RATE, 1_000.0);
            assert!(amplitude > 0.95 && amplitude < 1.05, "{rate}: {amplitude}");
        }
    }

    #[test]
    fn capture_worker_keeps_native_rate_audio_out_of_the_recording_store() {
        for rate in [16_000, 48_000, 96_000, 192_000] {
            let mut resampler = CaptureResampler::new(rate).unwrap();
            let source = vec![0.25; rate as usize];
            let mut captured = ChunkedSamples::new(TARGET_CAPTURE_RATE);
            for block in source.chunks(509) {
                for sample in resampler.push(block).unwrap() {
                    assert!(captured.push(sample));
                }
            }
            for sample in resampler.finish().unwrap() {
                assert!(captured.push(sample));
            }

            assert_eq!(captured.len, TARGET_CAPTURE_RATE as usize, "{rate}");
            assert_eq!(
                captured.max_len,
                TARGET_CAPTURE_RATE as usize * MAX_RECORDING_SECONDS
            );
        }
    }

    #[test]
    fn capture_resampler_passes_through_16khz_without_delay() {
        let input = vec![0.25, -0.5, 0.75];
        let mut resampler = CaptureResampler::new(TARGET_CAPTURE_RATE).unwrap();

        assert_eq!(resampler.push(&input).unwrap(), input);
        assert!(resampler.finish().unwrap().is_empty());
    }

    #[test]
    fn bounded_capture_queue_reports_worker_overflow() {
        let (sender, _receiver) = sync_channel(1);
        sender.try_send(vec![0.0]).unwrap();
        let issue = Mutex::new(None);

        assert!(!enqueue_capture_block(&sender, vec![1.0], &issue));
        assert_eq!(
            capture_issue(&issue).unwrap(),
            Some(CaptureIssue::WorkerOverflow)
        );
    }

    #[test]
    fn capture_worker_flushes_and_stops_when_the_callback_closes() {
        let (filled_sender, filled_receiver) = sync_channel(CAPTURE_QUEUE_DEPTH);
        let (free_sender, _free_receiver) = sync_channel(CAPTURE_QUEUE_DEPTH);
        let samples = Arc::new(Mutex::new(ChunkedSamples::new(TARGET_CAPTURE_RATE)));
        let issue = Arc::new(Mutex::new(None));
        let worker_samples = Arc::clone(&samples);
        let worker_issue = Arc::clone(&issue);
        let worker = thread::spawn(move || {
            capture_worker(
                CaptureResampler::new(48_000).unwrap(),
                filled_receiver,
                free_sender,
                worker_samples,
                worker_issue,
            );
        });
        for block in tone(48_000, 440.0, 1).chunks(CAPTURE_BLOCK_FRAMES) {
            filled_sender.send(block.to_vec()).unwrap();
        }

        drop(filled_sender);
        worker.join().unwrap();

        assert_eq!(samples.lock().unwrap().len, TARGET_CAPTURE_RATE as usize);
        assert_eq!(capture_issue(&issue).unwrap(), None);
    }
}
