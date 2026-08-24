use std::env;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{PostProcessingConfig, PostProcessingProvider};
use crate::post_processing;
use crate::private_file;

pub const MAX_TOKEN_BYTES: usize = 8192;
const MAX_CREDENTIAL_FILE_BYTES: usize = MAX_TOKEN_BYTES * 4 + 4096;

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
        let Some(contents) = private_file::read_to_string_bounded(path, MAX_CREDENTIAL_FILE_BYTES)?
        else {
            return Ok(Self::default());
        };
        let credentials: Self = toml::from_str(&contents)
            .with_context(|| format!("failed to parse credentials at {}", path.display()))?;
        credentials.validate()?;
        Ok(credentials)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let contents = toml::to_string_pretty(self).context("failed to serialize credentials")?;
        private_file::atomic_write(path, contents.as_bytes())
    }

    pub fn set(&mut self, provider: PostProcessingProvider, token: String) -> Result<()> {
        validate_token(&token)?;

        match provider {
            PostProcessingProvider::Openrouter => self.openrouter = Some(token),
            PostProcessingProvider::OpencodeZen => self.opencode_zen = Some(token),
        }
        Ok(())
    }

    pub fn resolve(&self, config: &PostProcessingConfig) -> Result<Option<String>> {
        if let Some(token) = self.stored(config.provider) {
            return Ok(Some(token.to_owned()));
        }
        let name = post_processing::api_key_env(config);
        let token = match env::var(name) {
            Ok(token) => token,
            Err(env::VarError::NotPresent) => return Ok(None),
            Err(env::VarError::NotUnicode(_)) => bail!("{name} is not valid Unicode"),
        };
        validate_token(&token).with_context(|| format!("{name} contains an invalid token"))?;
        Ok(Some(token))
    }

    pub fn is_configured(&self, config: &PostProcessingConfig) -> bool {
        self.resolve(config).is_ok_and(|token| token.is_some())
    }

    pub fn source(&self, config: &PostProcessingConfig) -> TokenSource {
        if self.stored(config.provider).is_some() {
            TokenSource::Stored
        } else if self.resolve(config).is_ok_and(|token| token.is_some()) {
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

    fn validate(&self) -> Result<()> {
        for token in [&self.openrouter, &self.opencode_zen].into_iter().flatten() {
            validate_token(token)?;
        }
        Ok(())
    }
}

pub fn validate_token(token: &str) -> Result<()> {
    if token.is_empty() {
        bail!("token cannot be empty");
    }
    if token.len() > MAX_TOKEN_BYTES {
        bail!("token is too long");
    }
    if token.trim() != token || token.chars().any(char::is_control) {
        bail!("token cannot contain whitespace at its edges or control characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

    struct EnvironmentGuard(String);

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            // SAFETY: each test uses a process-unique variable name.
            unsafe { env::remove_var(&self.0) };
        }
    }

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
        fs::create_dir(&directory).unwrap();
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
    fn enforces_the_provider_token_byte_limit() {
        let mut credentials = Credentials::default();
        credentials
            .set(
                PostProcessingProvider::Openrouter,
                "x".repeat(MAX_TOKEN_BYTES),
            )
            .unwrap();

        let error = credentials
            .set(
                PostProcessingProvider::Openrouter,
                "x".repeat(MAX_TOKEN_BYTES + 1),
            )
            .unwrap_err();

        assert!(error.to_string().contains("too long"));
    }

    #[test]
    fn rejects_oversized_tokens_loaded_from_storage() {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("milevox-token-load-{}-{id}", std::process::id()));
        let path = directory.join("credentials.toml");
        fs::create_dir(&directory).unwrap();
        fs::write(
            &path,
            format!("openrouter = {:?}\n", "x".repeat(MAX_TOKEN_BYTES + 1)),
        )
        .unwrap();

        let error = match Credentials::load(&path) {
            Ok(_) => panic!("oversized stored token was accepted"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("too long"));
        fs::remove_dir_all(directory).unwrap();
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

    #[test]
    fn environment_tokens_are_validated_before_use() {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let name = format!("MILEVOX_TEST_TOKEN_{}_{id}", std::process::id());
        let _guard = EnvironmentGuard(name.clone());
        let config = PostProcessingConfig {
            api_key_env: Some(name.clone()),
            ..PostProcessingConfig::default()
        };
        let credentials = Credentials::default();

        for invalid in [
            " greendale-token".to_owned(),
            "x".repeat(MAX_TOKEN_BYTES + 1),
        ] {
            // SAFETY: this test owns the process-unique variable name.
            unsafe { env::set_var(&name, invalid) };
            assert!(credentials.resolve(&config).is_err());
            assert!(!credentials.is_configured(&config));
            assert_eq!(credentials.source(&config), TokenSource::None);
        }

        // SAFETY: this test owns the process-unique variable name.
        unsafe { env::set_var(&name, "greendale-token") };
        assert_eq!(
            credentials.resolve(&config).unwrap().as_deref(),
            Some("greendale-token")
        );
        assert_eq!(credentials.source(&config), TokenSource::Environment);
    }

    #[test]
    fn two_maximum_escaped_tokens_round_trip_within_the_file_cap() {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "milevox-credentials-escaped-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("credentials.toml");
        let token = "\\".repeat(MAX_TOKEN_BYTES);
        let mut credentials = Credentials::default();
        credentials
            .set(PostProcessingProvider::Openrouter, token.clone())
            .unwrap();
        credentials
            .set(PostProcessingProvider::OpencodeZen, token.clone())
            .unwrap();

        credentials.save(&path).unwrap();
        let loaded = Credentials::load(&path).unwrap();

        assert_eq!(
            loaded.stored(PostProcessingProvider::Openrouter),
            Some(token.as_str())
        );
        assert_eq!(
            loaded.stored(PostProcessingProvider::OpencodeZen),
            Some(token.as_str())
        );
        assert!(fs::metadata(&path).unwrap().len() <= MAX_CREDENTIAL_FILE_BYTES as u64);
        fs::remove_dir_all(directory).unwrap();
    }
}
