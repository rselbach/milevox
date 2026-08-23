use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use parakeet_rs::{ParakeetTDT, Transcriber as ParakeetTranscriberTrait};

use crate::audio::CapturedAudio;
use crate::config::TranscriptionConfig;
use crate::paths;

const PARAKEET_SAMPLE_RATE: u32 = 16_000;

#[derive(Clone)]
pub struct ParakeetTranscriber {
    model_path: PathBuf,
    model: Arc<Mutex<Option<ParakeetTDT>>>,
}

impl ParakeetTranscriber {
    pub fn new(config: &TranscriptionConfig) -> Self {
        Self {
            model_path: config
                .model_path
                .clone()
                .unwrap_or_else(paths::default_model_path),
            model: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn transcribe(&self, audio: CapturedAudio) -> Result<String> {
        self.transcribe_inner(audio, false)
            .await?
            .context("Parakeet returned no text")
    }

    pub async fn transcribe_preview(&self, audio: CapturedAudio) -> Result<Option<String>> {
        self.transcribe_inner(audio, true).await
    }

    async fn transcribe_inner(
        &self,
        audio: CapturedAudio,
        allow_empty: bool,
    ) -> Result<Option<String>> {
        let model_path = self.model_path.clone();
        let model = Arc::clone(&self.model);
        tokio::task::spawn_blocking(move || {
            let samples = resample_mono(&audio.samples, audio.sample_rate, PARAKEET_SAMPLE_RATE)?;
            let mut model = model
                .lock()
                .map_err(|_| anyhow::anyhow!("Parakeet model lock was poisoned"))?;
            if model.is_none() {
                validate_model_path(&model_path)?;
                *model = Some(
                    ParakeetTDT::from_pretrained(&model_path, None).with_context(|| {
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
        })
        .await
        .context("Parakeet worker stopped unexpectedly")?
    }
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

    let output_len =
        ((samples.len() as u64 * u64::from(target_rate)).div_ceil(u64::from(source_rate))) as usize;
    let rate_ratio = source_rate as f64 / target_rate as f64;
    let mut output = Vec::with_capacity(output_len);
    for output_index in 0..output_len {
        let position = output_index as f64 * rate_ratio;
        let left_index = (position.floor() as usize).min(samples.len() - 1);
        let right_index = (left_index + 1).min(samples.len() - 1);
        let fraction = (position - left_index as f64) as f32;
        output.push(samples[left_index] * (1.0 - fraction) + samples[right_index] * fraction);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn leaves_16khz_audio_unchanged() {
        let samples = vec![-1.0, 0.0, 1.0];

        let output = resample_mono(&samples, 16_000, 16_000).unwrap();

        assert_eq!(output, samples);
    }

    #[test]
    fn resamples_default_pipewire_audio_to_16khz() {
        let samples = vec![0.25; 48_000];

        let output = resample_mono(&samples, 48_000, 16_000).unwrap();

        assert_eq!(output.len(), 16_000);
        assert!(output.iter().all(|sample| *sample == 0.25));
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

    #[test]
    fn reports_a_missing_model_file() {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "milevox-incomplete-model-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();

        let error = validate_model_path(&path).unwrap_err();

        assert!(error.to_string().contains("encoder model"));
        std::fs::remove_dir_all(path).unwrap();
    }
}
