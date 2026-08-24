use std::collections::BTreeMap;
use std::io::ErrorKind;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::config::PostProcessingProvider;
use crate::credentials::TokenSource;
use crate::paths;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    Start,
    Stop,
    StopAndWait,
    Toggle,
    Cancel,
    Status {
        follow: bool,
    },
    Settings {
        enabled: Option<bool>,
        provider: Option<PostProcessingProvider>,
        model: Option<String>,
    },
    SettingsModels {
        provider: Option<PostProcessingProvider>,
    },
    SetToken {
        provider: Option<PostProcessingProvider>,
        token: String,
    },
    RemoveToken {
        provider: Option<PostProcessingProvider>,
    },
    Debug {
        enabled: bool,
    },
    DebugLast,
    DebugClear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Idle,
    Recording,
    Transcribing,
    Refining,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

impl NoticeLevel {
    fn priority(self) -> u8 {
        match self {
            Self::Info => 0,
            Self::Warning => 1,
            Self::Error => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Notice {
    pub level: NoticeLevel,
    pub code: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Notice {
    pub fn info(code: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(NoticeLevel::Info, code, text, None)
    }

    pub fn warning(code: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(NoticeLevel::Warning, code, text, None)
    }

    pub fn error(code: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(NoticeLevel::Error, code, text, None)
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    fn new(
        level: NoticeLevel,
        code: impl Into<String>,
        text: impl Into<String>,
        detail: Option<String>,
    ) -> Self {
        Self {
            level,
            code: code.into(),
            text: text.into(),
            detail,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMethod {
    Typed,
    Clipboard,
    ClipboardFallback,
    None,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SettingsSnapshot {
    pub post_processing: PostProcessingSettings,
    pub debug: DebugSettings,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DebugSettings {
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CatalogOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PostProcessingSettings {
    pub enabled: bool,
    pub provider: PostProcessingProvider,
    pub model: String,
    pub token_configured: bool,
    pub token_source: TokenSource,
    pub provider_options: Vec<CatalogOption>,
    pub model_catalog: BTreeMap<String, Vec<CatalogOption>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StateEvent {
    #[serde(rename = "type")]
    event_type: String,
    pub state: State,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<Notice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryMethod>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial_transcript: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<SettingsSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug_entry: Option<String>,
}

impl StateEvent {
    pub fn new(state: State) -> Self {
        Self {
            event_type: "state".into(),
            state,
            message: None,
            notices: Vec::new(),
            delivery: None,
            transcript: None,
            partial_transcript: None,
            level: None,
            settings: None,
            debug_entry: None,
        }
    }

    pub fn message(state: State, message: impl Into<String>) -> Self {
        let mut event = Self::new(state);
        event.message = Some(message.into());
        event
    }

    pub fn error(state: State, code: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(state).with_notice(Notice::error(code, text))
    }

    pub fn active(state: State, partial_transcript: Option<String>, level: Option<f32>) -> Self {
        let mut event = Self::new(state);
        event.partial_transcript = partial_transcript;
        event.level = level;
        event
    }

    pub fn completed(transcript: String, delivery: DeliveryMethod, notices: Vec<Notice>) -> Self {
        let mut event = Self::new(State::Idle);
        event.transcript = Some(transcript);
        event.delivery = Some(delivery);
        for notice in notices {
            event = event.with_notice(notice);
        }
        event
    }

    pub fn configuration_changed() -> Self {
        let mut event = Self::new(State::Idle);
        event.transcript = Some(String::new());
        event
    }

    pub fn with_notice(mut self, notice: Notice) -> Self {
        let replaces_message = self
            .notices
            .iter()
            .map(|existing| existing.level.priority())
            .max()
            .is_none_or(|priority| notice.level.priority() > priority);
        if replaces_message || self.message.is_none() {
            self.message = Some(notice.text.clone());
        }
        self.notices.push(notice);
        self
    }

    pub fn with_settings(mut self, settings: SettingsSnapshot) -> Self {
        self.settings = Some(settings);
        self
    }

    pub fn with_debug_entry(mut self, debug_entry: String) -> Self {
        self.debug_entry = Some(debug_entry);
        self
    }
}

pub async fn run_client(command: Command) -> Result<()> {
    let follow = matches!(command, Command::Status { follow: true });
    let print_json = command_prints_json(&command);
    let print_debug_entry = matches!(command, Command::DebugLast);
    let path = paths::socket_path();
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("could not connect to Milevox at {}", path.display()))?;

    let request = serde_json::to_string(&command)?;
    stream.write_all(request.as_bytes()).await?;
    stream.write_all(b"\n").await?;

    let mut lines = BufReader::new(stream).lines();
    while let Some(line) = lines.next_line().await? {
        if print_json {
            println!("{line}");
        } else if print_debug_entry {
            println!("{}", debug_entry_from_response(&line)?);
        } else {
            check_mutation_response(&line)?;
        }
        if !follow {
            return Ok(());
        }
    }

    if follow {
        bail!("Milevox status stream closed unexpectedly");
    }
    bail!("Milevox closed the connection without a response")
}

fn command_prints_json(command: &Command) -> bool {
    matches!(
        command,
        Command::Status { .. } | Command::SettingsModels { .. }
    ) || matches!(
        command,
        Command::Settings {
            enabled: None,
            provider: None,
            model: None,
        }
    )
}

fn debug_entry_from_response(line: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct DebugResponse {
        message: Option<String>,
        debug_entry: Option<String>,
    }

    let response: DebugResponse =
        serde_json::from_str(line).context("Milevox returned a malformed response")?;
    if let Some(message) = response.message.filter(|message| !message.is_empty()) {
        bail!(message);
    }
    response
        .debug_entry
        .context("Milevox returned no diagnostics for the last transcription")
}

fn check_mutation_response(line: &str) -> Result<()> {
    let response: StateEvent =
        serde_json::from_str(line).context("Milevox returned a malformed response")?;
    if let Some(notice) = response
        .notices
        .iter()
        .find(|notice| notice.level == NoticeLevel::Error)
    {
        bail!(notice.text.clone());
    }
    if response.state == State::Error {
        bail!(
            response
                .message
                .unwrap_or_else(|| "Milevox operation failed".into())
        );
    }
    for notice in response
        .notices
        .iter()
        .filter(|notice| notice.level == NoticeLevel::Warning)
    {
        eprintln!("warning: {}", notice.text);
    }
    Ok(())
}

pub async fn ensure_socket_available() -> Result<()> {
    let path = paths::socket_path();
    match UnixStream::connect(&path).await {
        Ok(_) => bail!("another Milevox daemon is already running"),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == ErrorKind::ConnectionRefused => tokio::fs::remove_file(&path)
            .await
            .with_context(|| format!("failed to remove stale socket at {}", path.display())),
        Err(error) => {
            Err(error).with_context(|| format!("failed to inspect socket at {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> SettingsSnapshot {
        SettingsSnapshot {
            post_processing: PostProcessingSettings {
                enabled: true,
                provider: PostProcessingProvider::Openrouter,
                model: "~openai/gpt-mini-latest".into(),
                token_configured: true,
                token_source: TokenSource::Stored,
                provider_options: vec![CatalogOption {
                    value: "openrouter".into(),
                    label: "OpenRouter".into(),
                }],
                model_catalog: BTreeMap::new(),
            },
            debug: DebugSettings { enabled: false },
        }
    }

    #[test]
    fn serializes_stable_protocol_names() {
        let command = serde_json::to_string(&Command::Status { follow: true }).unwrap();
        let event = serde_json::to_string(&StateEvent::new(State::Transcribing)).unwrap();

        assert_eq!(command, r#"{"command":"status","follow":true}"#);
        assert_eq!(event, r#"{"type":"state","state":"transcribing"}"#);
    }

    #[test]
    fn serializes_typed_completion_and_legacy_message() {
        let event = StateEvent::completed(
            "Cool. Cool cool cool.".into(),
            DeliveryMethod::ClipboardFallback,
            vec![Notice::warning(
                "clipboard_fallback",
                "Typing failed; transcript copied to clipboard",
            )],
        );
        let json = serde_json::to_value(event).unwrap();

        assert_eq!(json["delivery"], "clipboard_fallback");
        assert_eq!(json["notices"][0]["level"], "warning");
        assert_eq!(
            json["message"],
            "Typing failed; transcript copied to clipboard"
        );
    }

    #[test]
    fn parses_old_events_without_typed_fields() {
        let event: StateEvent = serde_json::from_str(
            r#"{"type":"state","state":"idle","message":"legacy","transcript":"done"}"#,
        )
        .unwrap();

        assert!(event.notices.is_empty());
        assert!(event.delivery.is_none());
        assert_eq!(event.message.as_deref(), Some("legacy"));
    }

    #[test]
    fn settings_never_include_a_token_value() {
        let serialized =
            serde_json::to_string(&StateEvent::new(State::Idle).with_settings(settings())).unwrap();

        assert!(serialized.contains(r#""token_source":"stored""#));
        assert!(!serialized.contains("greendale-openrouter-token"));
    }

    #[test]
    fn configuration_updates_clear_the_previous_transcript() {
        assert_eq!(
            serde_json::to_string(&StateEvent::configuration_changed()).unwrap(),
            r#"{"type":"state","state":"idle","transcript":""}"#
        );
    }

    #[test]
    fn typed_errors_and_terminal_states_become_cli_errors() {
        let typed = serde_json::to_string(
            &StateEvent::new(State::Idle)
                .with_notice(Notice::error("invalid_model", "Invalid model")),
        )
        .unwrap();
        assert_eq!(
            check_mutation_response(&typed).unwrap_err().to_string(),
            "Invalid model"
        );

        let terminal = serde_json::to_string(&StateEvent::message(State::Error, "failed")).unwrap();
        assert_eq!(
            check_mutation_response(&terminal).unwrap_err().to_string(),
            "failed"
        );
    }
}
