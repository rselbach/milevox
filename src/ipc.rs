use std::io::ErrorKind;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::config::PostProcessingProvider;
use crate::paths;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    Start,
    Stop,
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
    SetToken {
        provider: Option<PostProcessingProvider>,
        token: String,
    },
    Debug {
        enabled: bool,
    },
    DebugLast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Idle,
    Recording,
    Transcribing,
    Refining,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingsSnapshot {
    pub post_processing: PostProcessingSettings,
    pub debug: DebugSettings,
}

#[derive(Debug, Clone, Serialize)]
pub struct DebugSettings {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PostProcessingSettings {
    pub enabled: bool,
    pub provider: PostProcessingProvider,
    pub model: String,
    pub token_configured: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StateEvent {
    #[serde(rename = "type")]
    event_type: &'static str,
    pub state: State,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
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
            event_type: "state",
            state,
            message: None,
            transcript: None,
            partial_transcript: None,
            level: None,
            settings: None,
            debug_entry: None,
        }
    }

    pub fn message(state: State, message: impl Into<String>) -> Self {
        Self {
            event_type: "state",
            state,
            message: Some(message.into()),
            transcript: None,
            partial_transcript: None,
            level: None,
            settings: None,
            debug_entry: None,
        }
    }

    pub fn active(state: State, partial_transcript: Option<String>, level: Option<f32>) -> Self {
        Self {
            event_type: "state",
            state,
            message: None,
            transcript: None,
            partial_transcript,
            level,
            settings: None,
            debug_entry: None,
        }
    }

    pub fn completed(transcript: String, warning: Option<String>) -> Self {
        Self {
            event_type: "state",
            state: State::Idle,
            message: warning,
            transcript: Some(transcript),
            partial_transcript: None,
            level: None,
            settings: None,
            debug_entry: None,
        }
    }

    pub fn configuration_changed() -> Self {
        Self {
            event_type: "state",
            state: State::Idle,
            message: None,
            transcript: Some(String::new()),
            partial_transcript: None,
            level: None,
            settings: None,
            debug_entry: None,
        }
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
        return Ok(());
    }

    bail!("Milevox closed the connection without a response")
}

fn command_prints_json(command: &Command) -> bool {
    matches!(command, Command::Status { .. })
        || matches!(
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
    #[derive(Deserialize)]
    struct MutationResponse {
        message: Option<String>,
    }

    let response: MutationResponse =
        serde_json::from_str(line).context("Milevox returned a malformed response")?;
    if let Some(message) = response.message.filter(|message| !message.is_empty()) {
        bail!(message);
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

    #[test]
    fn serializes_stable_protocol_names() {
        let command = serde_json::to_string(&Command::Status { follow: true }).unwrap();
        let event = serde_json::to_string(&StateEvent::new(State::Transcribing)).unwrap();

        assert_eq!(command, r#"{"command":"status","follow":true}"#);
        assert_eq!(event, r#"{"type":"state","state":"transcribing"}"#);
    }

    #[test]
    fn serializes_live_preview_data() {
        let event = StateEvent::active(
            State::Recording,
            Some("Troy and Abed".to_owned()),
            Some(0.25),
        );

        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"type":"state","state":"recording","partial_transcript":"Troy and Abed","level":0.25}"#
        );
    }

    #[test]
    fn serializes_post_processing_updates() {
        let command = Command::Settings {
            enabled: Some(true),
            provider: Some(PostProcessingProvider::OpencodeZen),
            model: Some("glm-5.2".to_owned()),
        };

        assert_eq!(
            serde_json::to_string(&command).unwrap(),
            r#"{"command":"settings","enabled":true,"provider":"opencode_zen","model":"glm-5.2"}"#
        );
    }

    #[test]
    fn never_returns_a_provider_token_in_settings() {
        let event = StateEvent::new(State::Idle).with_settings(SettingsSnapshot {
            post_processing: PostProcessingSettings {
                enabled: true,
                provider: PostProcessingProvider::Openrouter,
                model: "~openai/gpt-mini-latest".to_owned(),
                token_configured: true,
            },
            debug: DebugSettings { enabled: false },
        });
        let serialized = serde_json::to_string(&event).unwrap();

        assert!(serialized.contains(r#""token_configured":true"#));
        assert!(!serialized.contains("greendale-openrouter-token"));
    }

    #[test]
    fn serializes_debug_updates() {
        let command = Command::Debug { enabled: true };

        assert_eq!(
            serde_json::to_string(&command).unwrap(),
            r#"{"command":"debug","enabled":true}"#
        );
    }

    #[test]
    fn serializes_last_debug_request() {
        let command = Command::DebugLast;

        assert_eq!(
            serde_json::to_string(&command).unwrap(),
            r#"{"command":"debug_last"}"#
        );
    }

    #[test]
    fn configuration_updates_clear_the_previous_transcript() {
        let event = StateEvent::configuration_changed();

        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"type":"state","state":"idle","transcript":""}"#
        );
    }

    #[test]
    fn only_structured_read_commands_print_json() {
        assert!(command_prints_json(&Command::Status { follow: false }));
        assert!(command_prints_json(&Command::Settings {
            enabled: None,
            provider: None,
            model: None,
        }));
        assert!(!command_prints_json(&Command::DebugLast));
        assert!(!command_prints_json(&Command::Debug { enabled: true }));
        assert!(!command_prints_json(&Command::Settings {
            enabled: Some(true),
            provider: None,
            model: None,
        }));
    }

    #[test]
    fn extracts_last_debug_entry() {
        let entry = debug_entry_from_response(
            r#"{"type":"state","state":"idle","debug_entry":"FINAL RAW:\nCool. Cool cool cool."}"#,
        )
        .unwrap();

        assert_eq!(entry, "FINAL RAW:\nCool. Cool cool cool.");
    }

    #[test]
    fn reports_when_no_last_debug_entry_exists() {
        let error = debug_entry_from_response(
            r#"{"type":"state","state":"idle","message":"No transcription diagnostics are available"}"#,
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "No transcription diagnostics are available"
        );
    }

    #[test]
    fn mutation_errors_become_cli_errors() {
        let error =
            check_mutation_response(r#"{"type":"state","state":"idle","message":"Invalid model"}"#)
                .unwrap_err();

        assert_eq!(error.to_string(), "Invalid model");
        assert!(check_mutation_response(r#"{"type":"state","state":"idle"}"#).is_ok());
    }
}
