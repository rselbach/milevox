use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{Context, Result, bail};
use parakeet_rs::{ParakeetTDT, Transcriber as ParakeetTranscriberTrait};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
use tokio::sync::oneshot;

use crate::audio::CapturedAudio;
use crate::config::TranscriptionConfig;
use crate::paths;

const PARAKEET_SAMPLE_RATE: u32 = 16_000;
const RESAMPLER_CHUNK_SIZE: usize = 1024;

#[derive(Clone)]
pub struct ParakeetTranscriber {
    worker: Arc<Worker>,
}

struct Worker {
    queue: Mutex<WorkerQueue>,
    ready: Condvar,
}

#[derive(Default)]
struct WorkerQueue {
    finals: VecDeque<TranscriptionRequest>,
    preview: Option<TranscriptionRequest>,
    canceled: HashSet<u64>,
    active: Option<u64>,
}

struct TranscriptionRequest {
    generation: u64,
    audio: CapturedAudio,
    allow_empty: bool,
    reply: oneshot::Sender<std::result::Result<Option<String>, String>>,
}

impl ParakeetTranscriber {
    pub fn new(config: &TranscriptionConfig) -> Self {
        let model_path = config
            .model_path
            .clone()
            .unwrap_or_else(paths::default_model_path);
        let worker = Arc::new(Worker {
            queue: Mutex::new(WorkerQueue::default()),
            ready: Condvar::new(),
        });
        let worker_thread = Arc::clone(&worker);
        std::thread::Builder::new()
            .name("milevox-transcription".into())
            .spawn(move || run_worker(worker_thread, model_path))
            .expect("failed to start the Milevox transcription worker");
        Self { worker }
    }

    pub async fn transcribe(&self, generation: u64, audio: CapturedAudio) -> Result<String> {
        self.enqueue(generation, audio, false, true)
            .await?
            .context("transcription was canceled")
    }

    pub async fn transcribe_preview(
        &self,
        generation: u64,
        audio: CapturedAudio,
    ) -> Result<Option<String>> {
        self.enqueue(generation, audio, true, false).await
    }

    pub fn cancel(&self, generation: u64) {
        let Ok(mut queue) = self.worker.queue.lock() else {
            return;
        };
        if queue.active == Some(generation) {
            queue.canceled.insert(generation);
        }
        let mut retained = VecDeque::new();
        while let Some(request) = queue.finals.pop_front() {
            if request.generation == generation {
                let _ = request.reply.send(Ok(None));
            } else {
                retained.push_back(request);
            }
        }
        queue.finals = retained;
        if queue.preview.as_ref().map(|request| request.generation) == Some(generation)
            && let Some(request) = queue.preview.take()
        {
            let _ = request.reply.send(Ok(None));
        }
    }

    async fn enqueue(
        &self,
        generation: u64,
        audio: CapturedAudio,
        allow_empty: bool,
        final_request: bool,
    ) -> Result<Option<String>> {
        let (reply, response) = oneshot::channel();
        let request = TranscriptionRequest {
            generation,
            audio,
            allow_empty,
            reply,
        };
        {
            let mut queue = self
                .worker
                .queue
                .lock()
                .map_err(|_| anyhow::anyhow!("transcription queue was poisoned"))?;
            queue.canceled.remove(&generation);
            if final_request {
                if queue.preview.as_ref().map(|preview| preview.generation) == Some(generation)
                    && let Some(preview) = queue.preview.take()
                {
                    let _ = preview.reply.send(Ok(None));
                }
                queue.finals.push_back(request);
            } else {
                if let Some(replaced) = queue.preview.replace(request) {
                    let _ = replaced.reply.send(Ok(None));
                }
            }
            self.worker.ready.notify_one();
        }
        response
            .await
            .context("transcription worker stopped unexpectedly")?
            .map_err(anyhow::Error::msg)
    }
}

fn run_worker(worker: Arc<Worker>, model_path: PathBuf) {
    let mut model = None;
    loop {
        let request = {
            let mut queue = match worker.queue.lock() {
                Ok(queue) => queue,
                Err(_) => return,
            };
            while queue.finals.is_empty() && queue.preview.is_none() {
                queue = match worker.ready.wait(queue) {
                    Ok(queue) => queue,
                    Err(_) => return,
                };
            }
            let request = queue.finals.pop_front().or_else(|| queue.preview.take());
            queue.active = request.as_ref().map(|request| request.generation);
            request
        };
        let Some(request) = request else {
            continue;
        };
        if is_canceled(&worker, request.generation) {
            let _ = request.reply.send(Ok(None));
            finish_request(&worker, request.generation);
            continue;
        }
        let result = transcribe_inner(&model_path, &mut model, request.audio, request.allow_empty)
            .map_err(|error| format!("{error:#}"));
        if is_canceled(&worker, request.generation) {
            let _ = request.reply.send(Ok(None));
        } else {
            let _ = request.reply.send(result);
        }
        finish_request(&worker, request.generation);
    }
}

fn finish_request(worker: &Worker, generation: u64) {
    let Ok(mut queue) = worker.queue.lock() else {
        return;
    };
    if queue.active == Some(generation) {
        queue.active = None;
    }
    queue.canceled.remove(&generation);
}

fn is_canceled(worker: &Worker, generation: u64) -> bool {
    worker
        .queue
        .lock()
        .map(|queue| queue.canceled.contains(&generation))
        .unwrap_or(true)
}

fn transcribe_inner(
    model_path: &Path,
    model: &mut Option<ParakeetTDT>,
    audio: CapturedAudio,
    allow_empty: bool,
) -> Result<Option<String>> {
    let samples = resample_mono(&audio.samples, audio.sample_rate, PARAKEET_SAMPLE_RATE)?;
    if model.is_none() {
        validate_model_path(model_path)?;
        *model = Some(
            ParakeetTDT::from_pretrained(model_path, None).with_context(|| {
                format!("failed to load Parakeet model at {}", model_path.display())
            })?,
        );
    }
    let result = model
        .as_mut()
        .context("Parakeet model did not load")?
        .transcribe_samples(samples, PARAKEET_SAMPLE_RATE, 1, None)
        .context("Parakeet transcription failed")?;
    let transcript = result.text.trim().to_owned();
    if transcript.is_empty() {
        if allow_empty {
            return Ok(None);
        }
        bail!("Parakeet returned no text");
    }
    Ok(Some(transcript))
}

fn validate_model_path(path: &Path) -> Result<()> {
    if !path.is_dir() {
        bail!(
            "Parakeet model directory does not exist at {}; set transcription.model_path or install the default model",
            path.display()
        );
    }
    require_one(
        path,
        &[
            "encoder-model.int8.onnx",
            "encoder-model.onnx",
            "encoder.onnx",
        ],
        "encoder model",
    )?;
    require_one(
        path,
        &[
            "decoder_joint-model.int8.onnx",
            "decoder_joint-model.onnx",
            "decoder_joint.onnx",
        ],
        "decoder model",
    )?;
    if !path.join("vocab.txt").is_file() {
        bail!("Parakeet model at {} is missing vocab.txt", path.display());
    }
    Ok(())
}

fn require_one(path: &Path, filenames: &[&str], description: &str) -> Result<()> {
    if filenames
        .iter()
        .any(|filename| path.join(filename).is_file())
    {
        return Ok(());
    }
    bail!(
        "Parakeet model at {} is missing its {description}",
        path.display()
    )
}

fn resample_mono(samples: &[f32], source_rate: u32, target_rate: u32) -> Result<Vec<f32>> {
    if samples.is_empty() {
        bail!("cannot transcribe an empty audio buffer");
    }
    if source_rate == 0 || target_rate == 0 {
        bail!("audio sample rates must be greater than zero");
    }
    if source_rate == target_rate {
        return Ok(samples.to_vec());
    }

    let input_len = samples.len();
    let input = InterleavedSlice::new(samples, 1, input_len)
        .context("failed to prepare audio for resampling")?;
    let mut resampler = Fft::<f32>::new(
        source_rate as usize,
        target_rate as usize,
        RESAMPLER_CHUNK_SIZE,
        1,
        FixedSync::Both,
    )
    .context("failed to create the audio resampler")?;
    let output = resampler
        .process_all(&input, input_len, None)
        .context("failed to resample microphone audio")?;
    let mut samples = output.take_data();
    let expected_len =
        ((input_len as u64 * u64::from(target_rate)).div_ceil(u64::from(source_rate))) as usize;
    if samples.len() > expected_len {
        samples.truncate(expected_len);
    } else if samples.len() < expected_len {
        samples.resize(expected_len, samples.last().copied().unwrap_or(0.0));
    }
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

    fn tone(rate: u32, frequency: f32, seconds: f32) -> Vec<f32> {
        let len = (rate as f32 * seconds) as usize;
        (0..len)
            .map(|index| (TAU * frequency * index as f32 / rate as f32).sin())
            .collect()
    }

    fn component(samples: &[f32], rate: u32, frequency: f32) -> f32 {
        let (sin, cos) =
            samples
                .iter()
                .enumerate()
                .fold((0.0, 0.0), |(sin, cos), (index, sample)| {
                    let phase = TAU * frequency * index as f32 / rate as f32;
                    (sin + sample * phase.sin(), cos + sample * phase.cos())
                });
        2.0 * (sin * sin + cos * cos).sqrt() / samples.len() as f32
    }

    #[test]
    fn leaves_16khz_audio_unchanged() {
        let samples = vec![-1.0, 0.0, 1.0];
        assert_eq!(resample_mono(&samples, 16_000, 16_000).unwrap(), samples);
    }

    #[test]
    fn preserves_duration_within_one_target_sample() {
        for (source, target, len) in [(48_000, 16_000, 48_001), (44_100, 16_000, 44_123)] {
            let output = resample_mono(&vec![0.25; len], source, target).unwrap();
            let expected = (len as f64 * target as f64 / source as f64).ceil() as usize;
            assert!(output.len().abs_diff(expected) <= 1);
        }
    }

    #[test]
    fn keeps_a_1khz_tone_and_rejects_10khz_aliasing() {
        let passband = resample_mono(&tone(48_000, 1_000.0, 0.25), 48_000, 16_000).unwrap();
        let stopband = resample_mono(&tone(48_000, 10_000.0, 0.25), 48_000, 16_000).unwrap();

        assert!(component(&passband, 16_000, 1_000.0) > 0.8);
        assert!(component(&stopband, 16_000, 6_000.0) < 0.1);
    }

    #[test]
    fn handles_dc_impulses_and_empty_input() {
        let dc = resample_mono(&vec![0.5; 48_000], 48_000, 16_000).unwrap();
        assert!((dc.iter().sum::<f32>() / dc.len() as f32 - 0.5).abs() < 0.01);
        let mut impulse = vec![0.0; 4_800];
        impulse[2_400] = 1.0;
        assert_eq!(
            resample_mono(&impulse, 48_000, 16_000).unwrap().len(),
            1_600
        );
        assert!(resample_mono(&[], 48_000, 16_000).is_err());
    }

    #[test]
    fn validates_a_quantized_tdt_model_layout() {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "milevox-parakeet-model-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        for filename in [
            "encoder-model.int8.onnx",
            "decoder_joint-model.int8.onnx",
            "vocab.txt",
        ] {
            std::fs::write(path.join(filename), []).unwrap();
        }
        validate_model_path(&path).unwrap();
        std::fs::remove_dir_all(path).unwrap();
    }
}
