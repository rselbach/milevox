use std::env;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{PostProcessingConfig, PostProcessingProvider};
use crate::post_processing;
use crate::private_file;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenSource {
    Stored,
    Environment,
    None,
}

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

        private_file::secure(path)?;
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read credentials at {}", path.display()))?;
        toml::from_str(&contents)
            .with_context(|| format!("failed to parse credentials at {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let contents = toml::to_string_pretty(self).context("failed to serialize credentials")?;
        private_file::atomic_write(path, contents.as_bytes())
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

    pub fn source(&self, config: &PostProcessingConfig) -> TokenSource {
        if self.stored(config.provider).is_some() {
            TokenSource::Stored
        } else if env::var(post_processing::api_key_env(config))
            .ok()
            .is_some_and(|value| !value.trim().is_empty())
        {
            TokenSource::Environment
        } else {
            TokenSource::None
        }
    }

    pub fn remove(&mut self, provider: PostProcessingProvider) -> bool {
        match provider {
            PostProcessingProvider::Openrouter => self.openrouter.take().is_some(),
            PostProcessingProvider::OpencodeZen => self.opencode_zen.take().is_some(),
        }
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

    #[test]
    fn removes_only_the_selected_stored_token() {
        let mut credentials = Credentials::default();
        credentials
            .set(
                PostProcessingProvider::Openrouter,
                "openrouter-token".into(),
            )
            .unwrap();
        credentials
            .set(PostProcessingProvider::OpencodeZen, "zen-token".into())
            .unwrap();

        assert!(credentials.remove(PostProcessingProvider::Openrouter));
        assert!(!credentials.remove(PostProcessingProvider::Openrouter));
        assert_eq!(
            credentials.stored(PostProcessingProvider::OpencodeZen),
            Some("zen-token")
        );
    }
}
