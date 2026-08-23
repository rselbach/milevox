use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub transcription: TranscriptionConfig,
    pub post_processing: PostProcessingConfig,
    pub output: OutputConfig,
    pub debug: DebugConfig,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read configuration at {}", path.display()))?;
        toml::from_str(&contents)
            .with_context(|| format!("failed to parse configuration at {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().context("configuration path has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let contents = toml::to_string_pretty(self).context("failed to serialize configuration")?;
        let temporary_path = path.with_extension("toml.tmp");

        let write_result = (|| -> Result<()> {
            let mut options = OpenOptions::new();
            options.create(true).truncate(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary_path).with_context(|| {
                format!(
                    "failed to open temporary configuration at {}",
                    temporary_path.display()
                )
            })?;
            file.write_all(contents.as_bytes()).with_context(|| {
                format!(
                    "failed to write temporary configuration at {}",
                    temporary_path.display()
                )
            })?;
            file.sync_all().with_context(|| {
                format!(
                    "failed to sync temporary configuration at {}",
                    temporary_path.display()
                )
            })?;
            fs::rename(&temporary_path, path).with_context(|| {
                format!("failed to replace configuration at {}", path.display())
            })?;
            Ok(())
        })();

        if let Err(error) = write_result {
            if let Err(cleanup_error) = fs::remove_file(&temporary_path)
                && cleanup_error.kind() != std::io::ErrorKind::NotFound
            {
                eprintln!(
                    "Milevox could not remove {}: {cleanup_error}",
                    temporary_path.display()
                );
            }
            return Err(error);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TranscriptionConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PostProcessingProvider {
    #[default]
    Openrouter,
    OpencodeZen,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PostProcessingConfig {
    pub enabled: bool,
    pub provider: PostProcessingProvider,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

impl Default for PostProcessingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: PostProcessingProvider::Openrouter,
            model: None,
            api_key_env: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    #[default]
    Type,
    Clipboard,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    pub mode: OutputMode,
    pub clipboard_fallback: bool,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            mode: OutputMode::Type,
            clipboard_fallback: true,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DebugConfig {
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn defaults_keep_cloud_processing_off() {
        let config = Config::default();

        assert!(!config.post_processing.enabled);
        assert!(!config.debug.enabled);
        assert!(config.transcription.model_path.is_none());
    }

    #[test]
    fn rejects_unknown_configuration() {
        let error = toml::from_str::<Config>("mystery = true").unwrap_err();

        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn saves_and_loads_configuration_atomically() {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("milevox-config-{}-{id}", std::process::id()));
        let path = directory.join("config.toml");
        let mut config = Config::default();
        config.post_processing.enabled = true;
        config.post_processing.provider = PostProcessingProvider::OpencodeZen;
        config.post_processing.model = Some("glm-5.2".to_owned());
        config.debug.enabled = true;

        config.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();

        assert!(loaded.post_processing.enabled);
        assert_eq!(
            loaded.post_processing.provider,
            PostProcessingProvider::OpencodeZen
        );
        assert_eq!(loaded.post_processing.model.as_deref(), Some("glm-5.2"));
        assert!(loaded.debug.enabled);
        assert!(!path.with_extension("toml.tmp").exists());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
