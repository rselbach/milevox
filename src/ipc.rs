use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::config::PostProcessingProvider;
use crate::credentials::TokenSource;
use crate::{paths, post_processing};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    Start {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_target: Option<String>,
    },
    #[serde(alias = "stop_and_wait")]
    Stop,
    Toggle {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_target: Option<String>,
    },
    Cancel,
    CopyLast,
    Status {
        follow: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        levels: bool,
    },
    Settings {
        enabled: Option<bool>,
        provider: Option<PostProcessingProvider>,
        model: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        json: bool,
    },
    SettingsModels {
        provider: Option<PostProcessingProvider>,
        #[serde(default, skip_serializing_if = "is_false")]
        json: bool,
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
    Loading,
    Idle,
    Recording,
    Transcribing,
    Refining,
    Canceling,
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
pub struct LevelEvent {
    #[serde(rename = "type")]
    event_type: String,
    pub level: f32,
}

impl LevelEvent {
    pub fn new(level: f32) -> Self {
        Self {
            event_type: "level".into(),
            level: level.clamp(0.0, 1.0),
        }
    }
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
    pub delivery: Option<DeliveryMethod>,
    pub transcript: Option<String>,
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

    pub fn completed(
        state: State,
        transcript: String,
        delivery: DeliveryMethod,
        notices: Vec<Notice>,
    ) -> Self {
        let mut event = Self::new(state);
        event.transcript = Some(transcript);
        event.delivery = Some(delivery);
        for notice in notices {
            event = event.with_notice(notice);
        }
        event
    }

    pub fn configuration_changed(state: State) -> Self {
        Self::new(state)
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

    pub fn with_transcript(mut self, transcript: String) -> Self {
        self.transcript = Some(transcript);
        self
    }

    pub fn with_debug_entry(mut self, debug_entry: String) -> Self {
        self.debug_entry = Some(debug_entry);
        self
    }
}

pub async fn run_client(command: Command) -> Result<()> {
    let follow = matches!(command, Command::Status { follow: true, .. });
    let output = client_output(&command);
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
        match output {
            ClientOutput::StateJson => println!("{line}"),
            ClientOutput::Settings { json } => println!("{}", settings_from_response(&line, json)?),
            ClientOutput::Models { provider, json } => {
                println!("{}", models_from_response(&line, provider, json)?)
            }
            ClientOutput::Debug if print_debug_entry => {
                println!("{}", debug_entry_from_response(&line)?)
            }
            ClientOutput::Mutation | ClientOutput::Debug => check_mutation_response(&line)?,
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

#[derive(Clone, Copy)]
enum ClientOutput {
    StateJson,
    Settings {
        json: bool,
    },
    Models {
        provider: Option<PostProcessingProvider>,
        json: bool,
    },
    Debug,
    Mutation,
}

fn client_output(command: &Command) -> ClientOutput {
    match command {
        Command::Status { .. } => ClientOutput::StateJson,
        Command::Settings {
            enabled: None,
            provider: None,
            model: None,
            json,
        } => ClientOutput::Settings { json: *json },
        Command::SettingsModels { provider, json } => ClientOutput::Models {
            provider: *provider,
            json: *json,
        },
        Command::DebugLast => ClientOutput::Debug,
        _ => ClientOutput::Mutation,
    }
}

fn response_settings(line: &str) -> Result<SettingsSnapshot> {
    let response: StateEvent =
        serde_json::from_str(line).context("Milevox returned a malformed response")?;
    response
        .settings
        .context("Milevox returned no settings data")
}

fn settings_from_response(line: &str, json: bool) -> Result<String> {
    let settings = response_settings(line)?.post_processing;
    if json {
        return serde_json::to_string(&serde_json::json!({
            "enabled": settings.enabled,
            "provider": settings.provider,
            "model": settings.model,
            "token_source": settings.token_source,
        }))
        .context("failed to format Milevox settings");
    }
    Ok(format!(
        "Enabled: {}\nProvider: {}\nModel: {}\nToken source: {}",
        if settings.enabled { "yes" } else { "no" },
        settings.provider.as_str(),
        settings.model,
        token_source_name(settings.token_source),
    ))
}

fn models_from_response(
    line: &str,
    provider: Option<PostProcessingProvider>,
    json: bool,
) -> Result<String> {
    let settings = response_settings(line)?.post_processing;
    let catalog = match provider {
        Some(provider) => {
            let name = provider.as_str();
            let models = settings
                .model_catalog
                .get(name)
                .cloned()
                .with_context(|| format!("Milevox returned no models for {name}"))?;
            BTreeMap::from([(name.to_owned(), models)])
        }
        None => settings.model_catalog,
    };
    if json {
        return serde_json::to_string(&catalog).context("failed to format Milevox model catalog");
    }
    Ok(catalog
        .iter()
        .flat_map(|(provider, models)| {
            std::iter::once(format!("{provider}:")).chain(
                models
                    .iter()
                    .map(|model| format!("  {}\t{}", model.value, model.label)),
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

fn token_source_name(source: TokenSource) -> &'static str {
    match source {
        TokenSource::Stored => "stored",
        TokenSource::Environment => "environment",
        TokenSource::None => "none",
    }
}

fn is_false(value: &bool) -> bool {
    !value
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
        bail!(post_processing::escape_diagnostic_text(&message));
    }
    response
        .debug_entry
        .map(|entry| post_processing::escape_diagnostic_text(&entry))
        .context("Milevox returned no diagnostics for the last transcription")
}

fn check_mutation_response(line: &str) -> Result<()> {
    let response: StateEvent =
        serde_json::from_str(line).context("Milevox returned a malformed response")?;
    let successful_copy = response
        .notices
        .iter()
        .any(|notice| notice.level == NoticeLevel::Info && notice.code == "transcript_copied");
    let recoverable_transcript = response
        .transcript
        .as_deref()
        .filter(|transcript| !transcript.is_empty());
    if let Some(notice) = response
        .notices
        .iter()
        .find(|notice| notice.level == NoticeLevel::Error)
    {
        if let Some(transcript) = recoverable_transcript {
            println!("{}", post_processing::escape_diagnostic_text(transcript));
        }
        bail!(format_notice(notice));
    }
    if response.state == State::Error && !successful_copy {
        if let Some(transcript) = recoverable_transcript {
            println!("{}", post_processing::escape_diagnostic_text(transcript));
        }
        bail!(
            response
                .message
                .map(|message| post_processing::escape_diagnostic_text(&message))
                .unwrap_or_else(|| "Milevox operation failed".into())
        );
    }
    for notice in response
        .notices
        .iter()
        .filter(|notice| notice.level == NoticeLevel::Warning)
    {
        eprintln!("warning: {}", format_notice(notice));
    }
    Ok(())
}

fn format_notice(notice: &Notice) -> String {
    match notice.detail.as_deref() {
        Some(detail) => format!(
            "{}: {}",
            post_processing::escape_diagnostic_text(&notice.text),
            post_processing::escape_diagnostic_text(detail)
        ),
        None => post_processing::escape_diagnostic_text(&notice.text),
    }
}

pub async fn ensure_socket_available() -> Result<()> {
    let path = paths::socket_path();
    ensure_socket_available_at(&path).await
}

pub(crate) async fn ensure_socket_available_at(path: &Path) -> Result<()> {
    match UnixStream::connect(path).await {
        Ok(_) => bail!("another Milevox daemon is already running"),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == ErrorKind::ConnectionRefused => tokio::fs::remove_file(path)
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
                model_catalog: BTreeMap::from([(
                    "openrouter".into(),
                    vec![CatalogOption {
                        value: "~openai/gpt-mini-latest".into(),
                        label: "GPT Mini".into(),
                    }],
                )]),
            },
            debug: DebugSettings { enabled: false },
        }
    }

    #[test]
    fn serializes_stable_protocol_names() {
        let start = serde_json::to_string(&Command::Start {
            output_target: Some("0xdecaf".into()),
        })
        .unwrap();
        let command = serde_json::to_string(&Command::Status {
            follow: true,
            levels: false,
        })
        .unwrap();
        let stop = serde_json::to_string(&Command::Stop).unwrap();
        let copy = serde_json::to_string(&Command::CopyLast).unwrap();
        let legacy_stop: Command = serde_json::from_str(r#"{"command":"stop_and_wait"}"#).unwrap();
        let event = serde_json::to_string(&StateEvent::new(State::Transcribing)).unwrap();
        let canceling = serde_json::to_string(&StateEvent::new(State::Canceling)).unwrap();
        let legacy_start: Command = serde_json::from_str(r#"{"command":"start"}"#).unwrap();

        assert_eq!(start, r#"{"command":"start","output_target":"0xdecaf"}"#);
        assert_eq!(command, r#"{"command":"status","follow":true}"#);
        assert_eq!(stop, r#"{"command":"stop"}"#);
        assert_eq!(copy, r#"{"command":"copy_last"}"#);
        assert!(matches!(legacy_stop, Command::Stop));
        assert!(matches!(
            legacy_start,
            Command::Start {
                output_target: None
            }
        ));
        assert!(!event.contains("output_target"));
        assert_eq!(
            event,
            r#"{"type":"state","state":"transcribing","delivery":null,"transcript":null,"partial_transcript":null}"#
        );
        assert_eq!(
            canceling,
            r#"{"type":"state","state":"canceling","delivery":null,"transcript":null,"partial_transcript":null}"#
        );
    }

    #[test]
    fn serializes_typed_completion_and_legacy_message() {
        let event = StateEvent::completed(
            State::Idle,
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
    fn error_event_with_a_recoverable_transcript_round_trips() {
        let event = StateEvent::new(State::Error)
            .with_transcript("Troy and Abed in the morning".to_owned())
            .with_notice(
                Notice::error(
                    "delivery_failed",
                    "The final transcript could not be delivered",
                )
                .with_detail("wl-copy exited with status 1"),
            );

        let encoded = serde_json::to_string(&event).unwrap();
        let decoded: StateEvent = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded.state, State::Error);
        assert_eq!(
            decoded.transcript.as_deref(),
            Some("Troy and Abed in the morning")
        );
        assert_eq!(decoded.notices, event.notices);
        assert!(decoded.delivery.is_none());
        assert!(decoded.partial_transcript.is_none());
    }

    #[test]
    fn level_events_are_compact_and_contain_no_snapshot_data() {
        let json = serde_json::to_string(&LevelEvent::new(0.25)).unwrap();

        assert_eq!(json, r#"{"type":"level","level":0.25}"#);
        assert!(json.len() <= 48);
        assert!(!json.contains("transcript"));
        assert!(!json.contains("settings"));
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
    fn snapshots_clear_transient_fields_with_explicit_nulls() {
        assert_eq!(
            serde_json::to_string(&StateEvent::configuration_changed(State::Idle)).unwrap(),
            r#"{"type":"state","state":"idle","delivery":null,"transcript":null,"partial_transcript":null}"#
        );
    }

    #[test]
    fn formats_human_and_json_settings_without_the_state_envelope() {
        let event =
            serde_json::to_string(&StateEvent::new(State::Idle).with_settings(settings())).unwrap();

        assert_eq!(
            settings_from_response(&event, false).unwrap(),
            "Enabled: yes\nProvider: openrouter\nModel: ~openai/gpt-mini-latest\nToken source: stored"
        );
        let json: serde_json::Value =
            serde_json::from_str(&settings_from_response(&event, true).unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "enabled": true,
                "provider": "openrouter",
                "model": "~openai/gpt-mini-latest",
                "token_source": "stored",
            })
        );
        assert!(json.get("state").is_none());

        let mut disabled = settings();
        disabled.post_processing.enabled = false;
        disabled.post_processing.token_configured = false;
        disabled.post_processing.token_source = TokenSource::None;
        let disabled =
            serde_json::to_string(&StateEvent::new(State::Idle).with_settings(disabled)).unwrap();
        assert_eq!(
            settings_from_response(&disabled, false).unwrap(),
            "Enabled: no\nProvider: openrouter\nModel: ~openai/gpt-mini-latest\nToken source: none"
        );
    }

    #[test]
    fn formats_all_models_and_one_provider_without_unrequested_state() {
        let event =
            serde_json::to_string(&StateEvent::new(State::Idle).with_settings(settings())).unwrap();

        assert_eq!(
            models_from_response(&event, None, false).unwrap(),
            "openrouter:\n  ~openai/gpt-mini-latest\tGPT Mini"
        );
        let selected: serde_json::Value = serde_json::from_str(
            &models_from_response(&event, Some(PostProcessingProvider::Openrouter), true).unwrap(),
        )
        .unwrap();
        assert_eq!(
            selected,
            serde_json::json!({
                "openrouter": [{
                    "value": "~openai/gpt-mini-latest",
                    "label": "GPT Mini",
                }],
            })
        );
    }

    #[test]
    fn settings_output_rejects_a_response_without_settings() {
        let line = serde_json::to_string(&StateEvent::new(State::Idle)).unwrap();

        assert_eq!(
            settings_from_response(&line, false)
                .unwrap_err()
                .to_string(),
            "Milevox returned no settings data"
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

    #[test]
    fn successful_copy_acknowledgment_overrides_the_retained_error_state() {
        let response = serde_json::to_string(
            &StateEvent::new(State::Error)
                .with_notice(Notice::info("transcript_copied", "Transcript copied")),
        )
        .unwrap();

        check_mutation_response(&response).unwrap();
    }

    #[test]
    fn human_notice_rendering_escapes_terminal_controls() {
        let notice = Notice::error("provider_failed", "Provider failed\u{1b}]52")
            .with_detail("secret\u{7}\u{202e}");

        assert_eq!(
            format_notice(&notice),
            "Provider failed\\u{001b}]52: secret\\u{0007}\\u{202e}"
        );
    }

    #[test]
    fn record_stop_delivery_error_includes_the_target_detail() {
        let response = serde_json::to_string(
            &StateEvent::new(State::Error)
                .with_transcript("Troy and Abed in the morning".to_owned())
                .with_notice(
                    Notice::error(
                        "delivery_failed",
                        "The final transcript could not be delivered",
                    )
                    .with_detail("the active output target changed during dictation"),
                ),
        )
        .unwrap();

        assert_eq!(
            check_mutation_response(&response).unwrap_err().to_string(),
            "The final transcript could not be delivered: the active output target changed during dictation"
        );
    }
}
