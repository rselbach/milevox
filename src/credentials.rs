use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{PostProcessingConfig, PostProcessingProvider};
use crate::post_processing;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Credentials {
    #[serde(skip_serializing_if = "Option::is_none")]
    openrouter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    opencode_zen: Option<String>,
}

impl Credentials {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read credentials at {}", path.display()))?;
        toml::from_str(&contents)
            .with_context(|| format!("failed to parse credentials at {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().context("credentials path has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let contents = toml::to_string_pretty(self).context("failed to serialize credentials")?;
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
                    "failed to open temporary credentials at {}",
                    temporary_path.display()
                )
            })?;
            file.write_all(contents.as_bytes()).with_context(|| {
                format!(
                    "failed to write temporary credentials at {}",
                    temporary_path.display()
                )
            })?;
            file.sync_all().with_context(|| {
                format!(
                    "failed to sync temporary credentials at {}",
                    temporary_path.display()
                )
            })?;
            fs::rename(&temporary_path, path)
                .with_context(|| format!("failed to replace credentials at {}", path.display()))?;
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

    pub fn set(&mut self, provider: PostProcessingProvider, token: String) -> Result<()> {
        if token.is_empty() {
            bail!("token cannot be empty");
        }
        if token.len() > 8192 {
            bail!("token is too long");
        }
        if token.trim() != token || token.chars().any(char::is_control) {
            bail!("token cannot contain whitespace at its edges or control characters");
        }

        match provider {
            PostProcessingProvider::Openrouter => self.openrouter = Some(token),
            PostProcessingProvider::OpencodeZen => self.opencode_zen = Some(token),
        }
        Ok(())
    }

    pub fn resolve(&self, config: &PostProcessingConfig) -> Option<String> {
        self.stored(config.provider).map(str::to_owned).or_else(|| {
            env::var(post_processing::api_key_env(config))
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
    }

    pub fn is_configured(&self, config: &PostProcessingConfig) -> bool {
        self.resolve(config).is_some()
    }

    fn stored(&self, provider: PostProcessingProvider) -> Option<&str> {
        match provider {
            PostProcessingProvider::Openrouter => self.openrouter.as_deref(),
            PostProcessingProvider::OpencodeZen => self.opencode_zen.as_deref(),
        }
        .filter(|token| !token.trim().is_empty())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn stores_provider_tokens_without_mixing_them() {
        let mut credentials = Credentials::default();
        credentials
            .set(
                PostProcessingProvider::Openrouter,
                "greendale-openrouter-token".to_owned(),
            )
            .unwrap();
        credentials
            .set(
                PostProcessingProvider::OpencodeZen,
                "greendale-zen-token".to_owned(),
            )
            .unwrap();

        assert_eq!(
            credentials.stored(PostProcessingProvider::Openrouter),
            Some("greendale-openrouter-token")
        );
        assert_eq!(
            credentials.stored(PostProcessingProvider::OpencodeZen),
            Some("greendale-zen-token")
        );
    }

    #[test]
    fn saves_credentials_with_user_only_permissions() {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("milevox-credentials-{}-{id}", std::process::id()));
        let path = directory.join("credentials.toml");
        let mut credentials = Credentials::default();
        credentials
            .set(
                PostProcessingProvider::Openrouter,
                "greendale-openrouter-token".to_owned(),
            )
            .unwrap();

        credentials.save(&path).unwrap();
        let loaded = Credentials::load(&path).unwrap();

        assert_eq!(
            loaded.stored(PostProcessingProvider::Openrouter),
            Some("greendale-openrouter-token")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(!path.with_extension("toml.tmp").exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejects_tokens_with_edge_whitespace() {
        let mut credentials = Credentials::default();

        let error = credentials
            .set(
                PostProcessingProvider::Openrouter,
                " greendale-token".to_owned(),
            )
            .unwrap_err();

        assert!(error.to_string().contains("whitespace"));
    }
}
