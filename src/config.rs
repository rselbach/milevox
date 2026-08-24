use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, value};

use crate::private_file;

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
        let Some(contents) = private_file::read_to_string(path)? else {
            return Ok(Self::default());
        };
        toml::from_str(&contents)
            .with_context(|| format!("failed to parse configuration at {}", path.display()))
    }

    #[cfg(test)]
    pub fn save(&self, path: &Path) -> Result<()> {
        let contents = toml::to_string_pretty(self).context("failed to serialize configuration")?;
        private_file::atomic_write(path, contents.as_bytes())
    }

    pub fn save_post_processing(
        &self,
        path: &Path,
        enabled: Option<bool>,
        provider: Option<PostProcessingProvider>,
        model: Option<&str>,
        remove_model: bool,
    ) -> Result<()> {
        let mut document = load_document(path)?;
        if let Some(enabled) = enabled {
            document["post_processing"]["enabled"] = value(enabled);
        }
        if let Some(provider) = provider {
            document["post_processing"]["provider"] = value(provider.as_str());
        }
        if let Some(model) = model {
            document["post_processing"]["model"] = value(model);
        } else if remove_model && let Some(table) = document["post_processing"].as_table_mut() {
            table.remove("model");
        }
        private_file::atomic_write(path, document.to_string().as_bytes())
    }

    pub fn save_debug_enabled(&self, path: &Path, enabled: bool) -> Result<()> {
        let mut document = load_document(path)?;
        document["debug"]["enabled"] = value(enabled);
        private_file::atomic_write(path, document.to_string().as_bytes())
    }
}

impl PostProcessingProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Openrouter => "openrouter",
            Self::OpencodeZen => "opencode_zen",
        }
    }
}

fn load_document(path: &Path) -> Result<DocumentMut> {
    let Some(contents) = private_file::read_to_string(path)? else {
        return Ok(DocumentMut::new());
    };
    contents
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse configuration at {}", path.display()))
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
    Type,
    #[default]
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
            mode: OutputMode::Clipboard,
            clipboard_fallback: false,
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
        assert!(matches!(config.output.mode, OutputMode::Clipboard));
        assert!(!config.output.clipboard_fallback);
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
        std::fs::create_dir(&directory).unwrap();
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

    #[test]
    fn targeted_updates_preserve_comments_order_and_unrelated_values() {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("milevox-config-edit-{}-{id}", std::process::id()));
        let path = directory.join("config.toml");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            &path,
            "# hand written\n[output]\nmode = \"clipboard\" # keep\n\n[post_processing]\nenabled = false\nprovider = \"openrouter\"\nmodel = \"~openai/gpt-mini-latest\"\n",
        )
        .unwrap();
        let config = Config::load(&path).unwrap();

        config
            .save_post_processing(
                &path,
                Some(true),
                Some(PostProcessingProvider::OpencodeZen),
                None,
                true,
            )
            .unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.starts_with("# hand written\n[output]"));
        assert!(contents.contains("mode = \"clipboard\" # keep"));
        assert!(contents.contains("enabled = true"));
        assert!(contents.contains("provider = \"opencode_zen\""));
        assert!(!contents.contains("model ="));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
