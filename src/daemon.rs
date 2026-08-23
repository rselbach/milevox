use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::audio::{CapturedAudio, Recording, RecordingReader};
use crate::config::{Config, PostProcessingProvider};
use crate::credentials::Credentials;
use crate::ipc::{
    Command, DebugSettings, PostProcessingSettings, SettingsSnapshot, State, StateEvent,
    ensure_socket_available,
};
use crate::{output, paths, post_processing, transcription};

enum ActorMessage {
    Command {
        command: Command,
        reply: oneshot::Sender<StateEvent>,
    },
    Progress {
        generation: u64,
        state: State,
    },
    Preview {
        generation: u64,
        raw_transcript: String,
        transcript: String,
    },
    Level {
        generation: u64,
        level: f32,
    },
    Completed {
        generation: u64,
        completion: PipelineCompletion,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

struct PipelineResult {
    transcript: String,
    warning: Option<String>,
}

struct PipelineCompletion {
    result: Result<PipelineResult>,
    debug_entry: String,
}

#[derive(Default)]
struct LastDebugEntry {
    entry: Option<String>,
}

impl LastDebugEntry {
    fn remember(&mut self, entry: String) {
        self.entry = Some(entry);
    }

    fn event(&self, state: State) -> StateEvent {
        let Some(entry) = &self.entry else {
            return StateEvent::message(state, "No transcription diagnostics are available");
        };

        StateEvent::new(state).with_debug_entry(entry.clone())
    }
}

struct PreviewTranscripts {
    raw: Option<String>,
    stabilized: Option<String>,
}

struct DebugOutcome<'a> {
    provider_text: Option<&'a str>,
    delivered_text: &'a str,
    warning: Option<&'a str>,
    error: Option<&'a str>,
}

struct Pipeline {
    generation: u64,
    task: JoinHandle<()>,
}

struct ActiveRecording {
    generation: u64,
    recording: Recording,
    preview_task: JoinHandle<()>,
    level_task: JoinHandle<()>,
}

struct Daemon {
    config: Config,
    config_path: PathBuf,
    credentials: Credentials,
    credentials_path: PathBuf,
    recording: Option<ActiveRecording>,
    pipeline: Option<Pipeline>,
    generation: u64,
    raw_preview_transcript: Option<String>,
    partial_transcript: Option<String>,
    last_debug_entry: LastDebugEntry,
    level: f32,
    events: watch::Sender<StateEvent>,
    inbox: mpsc::Sender<ActorMessage>,
    transcriber: transcription::ParakeetTranscriber,
}

pub async fn run(config: Config, config_path: PathBuf) -> Result<()> {
    let runtime_dir = paths::runtime_dir();
    tokio::fs::create_dir_all(&runtime_dir)
        .await
        .with_context(|| format!("failed to create {}", runtime_dir.display()))?;
    ensure_socket_available().await?;
    let socket_path = paths::socket_path();
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to listen at {}", socket_path.display()))?;
    reset_debug_log(&paths::debug_log_path())?;

    let credentials_path = paths::credentials_path(&config_path);
    let credentials = Credentials::load(&credentials_path)?;
    let initial_event =
        StateEvent::new(State::Idle).with_settings(settings_snapshot(&config, &credentials));
    let (events, _) = watch::channel(initial_event);
    let (inbox, receiver) = mpsc::channel(16);
    let daemon = Daemon {
        transcriber: transcription::ParakeetTranscriber::new(&config.transcription),
        config,
        config_path,
        credentials,
        credentials_path,
        recording: None,
        pipeline: None,
        generation: 0,
        raw_preview_transcript: None,
        partial_transcript: None,
        last_debug_entry: LastDebugEntry::default(),
        level: 0.0,
        events: events.clone(),
        inbox: inbox.clone(),
    };
    let actor = tokio::spawn(daemon.run(receiver));

    let serve_result = serve(listener, inbox.clone(), events).await;
    let (reply, response) = oneshot::channel();
    if inbox.send(ActorMessage::Shutdown { reply }).await.is_ok() {
        let _ = response.await;
    }
    let actor_result = actor.await;
    let remove_result = tokio::fs::remove_file(&socket_path).await;
    if let Err(error) = remove_result
        && error.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!(
            "Milevox could not remove socket {}: {error}",
            socket_path.display()
        );
    }
    actor_result.context("Milevox daemon task failed")?;
    serve_result
}

async fn serve(
    listener: UnixListener,
    inbox: mpsc::Sender<ActorMessage>,
    events: watch::Sender<StateEvent>,
) -> Result<()> {
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = result.context("failed to accept a Milevox client")?;
                let inbox = inbox.clone();
                let events = events.subscribe();
                tokio::spawn(async move {
                    if let Err(error) = handle_client(stream, inbox, events).await {
                        eprintln!("Milevox client error: {error:#}");
                    }
                });
            }
            result = tokio::signal::ctrl_c() => {
                result.context("failed to listen for shutdown signal")?;
                return Ok(());
            }
        }
    }
}

async fn handle_client(
    stream: UnixStream,
    inbox: mpsc::Sender<ActorMessage>,
    mut events: watch::Receiver<StateEvent>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let line = lines.next_line().await?.context("client sent no command")?;
    let command: Command = serde_json::from_str(&line).context("invalid client command")?;

    if matches!(command, Command::Status { follow: true }) {
        let initial = events.borrow().clone();
        write_event(&mut writer, &initial).await?;
        while events.changed().await.is_ok() {
            let event = events.borrow().clone();
            write_event(&mut writer, &event).await?;
        }
        return Ok(());
    }

    let (reply, response) = oneshot::channel();
    inbox
        .send(ActorMessage::Command { command, reply })
        .await
        .context("Milevox daemon stopped")?;
    let response = response.await.context("Milevox daemon did not respond")?;
    write_event(&mut writer, &response).await
}

async fn write_event(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    event: &StateEvent,
) -> Result<()> {
    let line = serde_json::to_vec(event)?;
    writer.write_all(&line).await?;
    writer.write_all(b"\n").await?;
    Ok(())
}

impl Daemon {
    async fn run(mut self, mut receiver: mpsc::Receiver<ActorMessage>) {
        while let Some(message) = receiver.recv().await {
            match message {
                ActorMessage::Command { command, reply } => {
                    let event = self.handle_command(command).await;
                    let _ = reply.send(event);
                }
                ActorMessage::Progress { generation, state } => {
                    if self.pipeline.as_ref().map(|pipeline| pipeline.generation)
                        == Some(generation)
                    {
                        self.publish(StateEvent::active(
                            state,
                            self.partial_transcript.clone(),
                            None,
                        ));
                    }
                }
                ActorMessage::Preview {
                    generation,
                    raw_transcript,
                    transcript,
                } => {
                    if self
                        .recording
                        .as_ref()
                        .map(|recording| recording.generation)
                        == Some(generation)
                    {
                        self.raw_preview_transcript = Some(raw_transcript);
                        let changed = self.partial_transcript.as_deref() != Some(&transcript);
                        self.partial_transcript = Some(transcript);
                        if changed {
                            self.publish_recording();
                        }
                    }
                }
                ActorMessage::Level { generation, level } => {
                    if self
                        .recording
                        .as_ref()
                        .map(|recording| recording.generation)
                        == Some(generation)
                    {
                        self.level = level;
                        self.publish_recording();
                    }
                }
                ActorMessage::Completed {
                    generation,
                    completion,
                } => {
                    if self.pipeline.as_ref().map(|pipeline| pipeline.generation)
                        != Some(generation)
                    {
                        continue;
                    }
                    self.pipeline = None;
                    self.raw_preview_transcript = None;
                    self.partial_transcript = None;
                    self.level = 0.0;
                    self.last_debug_entry.remember(completion.debug_entry);
                    match completion.result {
                        Ok(result) => {
                            self.publish(StateEvent::completed(result.transcript, result.warning));
                        }
                        Err(error) => {
                            self.publish(StateEvent::message(State::Error, format!("{error:#}")));
                        }
                    }
                }
                ActorMessage::Shutdown { reply } => {
                    self.cancel().await;
                    let _ = reply.send(());
                    return;
                }
            }
        }
    }

    async fn handle_command(&mut self, command: Command) -> StateEvent {
        match command {
            Command::Start => self.start_recording(),
            Command::Stop => self.stop_recording(),
            Command::Toggle => match self.current_state() {
                State::Recording => self.stop_recording(),
                State::Idle | State::Error => self.start_recording(),
                _ => self.current_event(),
            },
            Command::Cancel => self.cancel().await,
            Command::Status { .. } => self.current_event(),
            Command::Settings {
                enabled,
                provider,
                model,
            } => self.update_settings(enabled, provider, model),
            Command::SetToken { provider, token } => self.update_token(provider, token),
            Command::Debug { enabled } => self.update_debug(enabled),
            Command::DebugLast => self.last_debug(),
        }
    }

    fn last_debug(&self) -> StateEvent {
        self.decorate(self.last_debug_entry.event(self.current_state()))
    }

    fn update_settings(
        &mut self,
        enabled: Option<bool>,
        provider: Option<PostProcessingProvider>,
        model: Option<String>,
    ) -> StateEvent {
        if enabled.is_none() && provider.is_none() && model.is_none() {
            return self.current_event();
        }
        if self.recording.is_some() || self.pipeline.is_some() {
            return self.decorate(StateEvent::message(
                self.current_state(),
                "Post-processing settings cannot change during dictation",
            ));
        }

        let updated = match apply_settings(&self.config, enabled, provider, model) {
            Ok(updated) => updated,
            Err(error) => {
                return self.decorate(StateEvent::message(
                    self.current_state(),
                    format!("Invalid post-processing settings: {error:#}"),
                ));
            }
        };
        if let Err(error) = updated.save(&self.config_path) {
            return self.decorate(StateEvent::message(
                self.current_state(),
                format!("Could not save post-processing settings: {error:#}"),
            ));
        }

        self.config = updated;
        self.publish(StateEvent::configuration_changed())
    }

    fn update_token(
        &mut self,
        provider: Option<PostProcessingProvider>,
        token: String,
    ) -> StateEvent {
        if self.recording.is_some() || self.pipeline.is_some() {
            return self.decorate(StateEvent::message(
                self.current_state(),
                "Provider tokens cannot change during dictation",
            ));
        }

        let provider = provider.unwrap_or(self.config.post_processing.provider);
        let mut updated = self.credentials.clone();
        if let Err(error) = updated.set(provider, token) {
            return self.decorate(StateEvent::message(
                self.current_state(),
                format!("Invalid provider token: {error:#}"),
            ));
        }
        if let Err(error) = updated.save(&self.credentials_path) {
            return self.decorate(StateEvent::message(
                self.current_state(),
                format!("Could not save provider token: {error:#}"),
            ));
        }

        self.credentials = updated;
        self.publish(StateEvent::configuration_changed())
    }

    fn update_debug(&mut self, enabled: bool) -> StateEvent {
        if self.recording.is_some() || self.pipeline.is_some() {
            return self.decorate(StateEvent::message(
                self.current_state(),
                "Debug logging cannot change during dictation",
            ));
        }

        let mut updated = self.config.clone();
        updated.debug.enabled = enabled;
        if let Err(error) = updated.save(&self.config_path) {
            return self.decorate(StateEvent::message(
                self.current_state(),
                format!("Could not save debug setting: {error:#}"),
            ));
        }

        self.config = updated;
        self.publish(StateEvent::configuration_changed())
    }

    fn start_recording(&mut self) -> StateEvent {
        if self.recording.is_some() || self.pipeline.is_some() {
            return self.current_event();
        }

        match Recording::start() {
            Ok(recording) => {
                self.generation += 1;
                let generation = self.generation;
                let reader = recording.reader();
                let preview_task = spawn_preview_loop(
                    reader.clone(),
                    generation,
                    self.transcriber.clone(),
                    self.inbox.clone(),
                );
                let level_task = spawn_level_loop(reader, generation, self.inbox.clone());
                self.recording = Some(ActiveRecording {
                    generation,
                    recording,
                    preview_task,
                    level_task,
                });
                self.raw_preview_transcript = None;
                self.partial_transcript = None;
                self.level = 0.0;
                self.publish_recording()
            }
            Err(error) => self.publish(StateEvent::message(State::Error, format!("{error:#}"))),
        }
    }

    fn stop_recording(&mut self) -> StateEvent {
        let Some(active) = self.recording.take() else {
            return self.current_event();
        };
        active.preview_task.abort();
        active.level_task.abort();
        let generation = active.generation;
        let previews = PreviewTranscripts {
            raw: self.raw_preview_transcript.clone(),
            stabilized: self.partial_transcript.clone(),
        };
        let audio = match active.recording.finish() {
            Ok(audio) => audio,
            Err(error) => {
                let error = format!("{error:#}");
                self.last_debug_entry
                    .remember(failed_debug_entry(generation, &previews, &error));
                self.raw_preview_transcript = None;
                self.partial_transcript = None;
                self.level = 0.0;
                return self.publish(StateEvent::message(State::Error, error));
            }
        };

        let config = self.config.clone();
        let credentials = self.credentials.clone();
        let inbox = self.inbox.clone();
        let transcriber = self.transcriber.clone();
        let task = tokio::spawn(async move {
            let completion = run_pipeline(
                generation,
                audio,
                previews,
                config,
                credentials,
                transcriber,
                &inbox,
            )
            .await;
            let _ = inbox
                .send(ActorMessage::Completed {
                    generation,
                    completion,
                })
                .await;
        });
        self.pipeline = Some(Pipeline { generation, task });
        self.publish(StateEvent::active(
            State::Transcribing,
            self.partial_transcript.clone(),
            None,
        ))
    }

    async fn cancel(&mut self) -> StateEvent {
        if let Some(recording) = self.recording.take() {
            recording.preview_task.abort();
            recording.level_task.abort();
        }
        if let Some(pipeline) = self.pipeline.take() {
            pipeline.task.abort();
        }
        self.raw_preview_transcript = None;
        self.partial_transcript = None;
        self.level = 0.0;
        self.publish(StateEvent::new(State::Idle))
    }

    fn current_state(&self) -> State {
        self.events.borrow().state
    }

    fn current_event(&self) -> StateEvent {
        self.events.borrow().clone()
    }

    fn publish(&self, event: StateEvent) -> StateEvent {
        let event = self.decorate(event);
        self.events.send_replace(event.clone());
        event
    }

    fn decorate(&self, event: StateEvent) -> StateEvent {
        event.with_settings(settings_snapshot(&self.config, &self.credentials))
    }

    fn publish_recording(&self) -> StateEvent {
        self.publish(StateEvent::active(
            State::Recording,
            self.partial_transcript.clone(),
            Some(self.level),
        ))
    }
}

fn settings_snapshot(config: &Config, credentials: &Credentials) -> SettingsSnapshot {
    SettingsSnapshot {
        post_processing: PostProcessingSettings {
            enabled: config.post_processing.enabled,
            provider: config.post_processing.provider,
            model: post_processing::selected_model(&config.post_processing).to_owned(),
            token_configured: credentials.is_configured(&config.post_processing),
        },
        debug: DebugSettings {
            enabled: config.debug.enabled,
        },
    }
}

fn reset_debug_log(path: &Path) -> Result<()> {
    let parent = path.parent().context("debug log path has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed to reset debug log at {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = file
            .metadata()
            .with_context(|| format!("failed to inspect permissions for {}", path.display()))?
            .permissions();
        permissions.set_mode(0o600);
        file.set_permissions(permissions)
            .with_context(|| format!("failed to secure debug log at {}", path.display()))?;
    }
    Ok(())
}

fn apply_settings(
    config: &Config,
    enabled: Option<bool>,
    provider: Option<PostProcessingProvider>,
    model: Option<String>,
) -> Result<Config> {
    let mut updated = config.clone();
    if let Some(enabled) = enabled {
        updated.post_processing.enabled = enabled;
    }
    let provider_changed = provider
        .map(|provider| provider != updated.post_processing.provider)
        .unwrap_or(false);
    if let Some(provider) = provider {
        updated.post_processing.provider = provider;
    }
    if let Some(model) = model {
        updated.post_processing.model = Some(model);
    } else if provider_changed {
        updated.post_processing.model = None;
    }
    post_processing::validate(&updated.post_processing)?;
    Ok(updated)
}

fn spawn_level_loop(
    reader: RecordingReader,
    generation: u64,
    inbox: mpsc::Sender<ActorMessage>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(80));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let level = match reader.level() {
                Ok(level) => level,
                Err(error) => {
                    eprintln!("Milevox audio level stopped: {error:#}");
                    return;
                }
            };
            if inbox
                .send(ActorMessage::Level { generation, level })
                .await
                .is_err()
            {
                return;
            }
        }
    })
}

fn spawn_preview_loop(
    reader: RecordingReader,
    generation: u64,
    transcriber: transcription::ParakeetTranscriber,
    inbox: mpsc::Sender<ActorMessage>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut stabilizer = PreviewStabilizer::default();
        let mut last_sample_count = 0;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let sample_count = match reader.sample_count() {
                Ok(sample_count) => sample_count,
                Err(error) => {
                    eprintln!("Milevox live preview stopped: {error:#}");
                    return;
                }
            };
            let audio = match reader.snapshot() {
                Ok(audio) => audio,
                Err(error) => {
                    eprintln!("Milevox live preview stopped: {error:#}");
                    return;
                }
            };
            let minimum_samples = usize::try_from(audio.sample_rate / 2).unwrap_or(usize::MAX);
            if sample_count < minimum_samples || sample_count <= last_sample_count {
                continue;
            }

            let raw_transcript = match transcriber.transcribe_preview(audio).await {
                Ok(Some(transcript)) => transcript,
                Ok(None) => continue,
                Err(error) => {
                    eprintln!("Milevox live preview failed: {error:#}");
                    continue;
                }
            };
            last_sample_count = sample_count;
            let transcript = stabilizer.stabilize(&raw_transcript);
            if transcript.is_empty() {
                continue;
            }
            if inbox
                .send(ActorMessage::Preview {
                    generation,
                    raw_transcript,
                    transcript,
                })
                .await
                .is_err()
            {
                return;
            }
        }
    })
}

#[derive(Default)]
struct PreviewStabilizer {
    committed_words: Vec<String>,
    previous_words: Vec<String>,
}

impl PreviewStabilizer {
    fn stabilize(&mut self, text: &str) -> String {
        let words = text
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let agreed_count = self
            .previous_words
            .iter()
            .zip(&words)
            .take_while(|(left, right)| normalize_word(left) == normalize_word(right))
            .count();
        if agreed_count > self.committed_words.len() {
            self.committed_words = self.previous_words[..agreed_count].to_vec();
        }
        self.previous_words = words.clone();

        if words.len() <= self.committed_words.len() {
            return self.committed_words.join(" ");
        }
        self.committed_words
            .iter()
            .chain(&words[self.committed_words.len()..])
            .cloned()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn normalize_word(word: &str) -> String {
    word.trim_matches(|character: char| !character.is_alphanumeric())
        .to_lowercase()
}

async fn run_pipeline(
    generation: u64,
    audio: CapturedAudio,
    previews: PreviewTranscripts,
    config: Config,
    credentials: Credentials,
    transcriber: transcription::ParakeetTranscriber,
    inbox: &mpsc::Sender<ActorMessage>,
) -> PipelineCompletion {
    let raw = match transcriber.transcribe(audio).await {
        Ok(raw) => raw,
        Err(error) => {
            let entry = failed_debug_entry(generation, &previews, &format!("{error:#}"));
            persist_debug_entry(config.debug.enabled, &entry).await;
            return PipelineCompletion {
                result: Err(error),
                debug_entry: entry,
            };
        }
    };

    if config.post_processing.enabled
        && let Err(error) = inbox
            .send(ActorMessage::Progress {
                generation,
                state: State::Refining,
            })
            .await
            .context("Milevox daemon stopped during post-processing")
    {
        let entry = failed_debug_entry(generation, &previews, &format!("{error:#}"));
        persist_debug_entry(config.debug.enabled, &entry).await;
        return PipelineCompletion {
            result: Err(error),
            debug_entry: entry,
        };
    }
    let api_key = credentials.resolve(&config.post_processing);
    let post_processing_input = if config.post_processing.enabled {
        select_post_processing_input(&previews, &raw)
    } else {
        &raw
    };
    let refined = post_processing::refine(
        &config.post_processing,
        api_key.as_deref(),
        post_processing_input,
    )
    .await;
    let delivery = output::deliver(&config.output, &refined.text).await;
    let delivery_error = delivery.as_ref().err().map(|error| format!("{error:#}"));
    let entry = debug_entry(
        generation,
        &previews,
        &raw,
        post_processing_input,
        DebugOutcome {
            provider_text: refined.provider_text.as_deref(),
            delivered_text: &refined.text,
            warning: refined.warning.as_deref(),
            error: delivery_error.as_deref(),
        },
    );
    persist_debug_entry(config.debug.enabled, &entry).await;
    let result = delivery.map(|()| PipelineResult {
        transcript: refined.text,
        warning: refined.warning,
    });

    PipelineCompletion {
        result,
        debug_entry: entry,
    }
}

fn select_post_processing_input<'a>(
    previews: &'a PreviewTranscripts,
    final_raw: &'a str,
) -> &'a str {
    previews.stabilized.as_deref().unwrap_or(final_raw)
}

fn debug_entry(
    generation: u64,
    previews: &PreviewTranscripts,
    final_raw: &str,
    post_processing_input: &str,
    outcome: DebugOutcome<'_>,
) -> String {
    let raw_preview = previews.raw.as_deref().unwrap_or("[unavailable]");
    let stabilized_preview = previews.stabilized.as_deref().unwrap_or("[unavailable]");
    let provider_text = outcome.provider_text.unwrap_or("[unavailable]");
    let warning = outcome.warning.unwrap_or("[none]");
    let error = outcome.error.unwrap_or("[none]");
    let delivered_text = outcome.delivered_text;
    format!(
        "=== RECORDING {generation} ===\nLAST RAW PREVIEW:\n{raw_preview}\n\nLAST STABILIZED PREVIEW:\n{stabilized_preview}\n\nFINAL RAW:\n{final_raw}\n\nPOST-PROCESSING INPUT:\n{post_processing_input}\n\nPROVIDER RESPONSE:\n{provider_text}\n\nDELIVERED TEXT:\n{delivered_text}\n\nWARNING:\n{warning}\n\nERROR:\n{error}"
    )
}

fn failed_debug_entry(generation: u64, previews: &PreviewTranscripts, error: &str) -> String {
    debug_entry(
        generation,
        previews,
        "[unavailable]",
        "[unavailable]",
        DebugOutcome {
            provider_text: None,
            delivered_text: "[unavailable]",
            warning: None,
            error: Some(error),
        },
    )
}

async fn persist_debug_entry(enabled: bool, entry: &str) {
    if !enabled {
        return;
    }

    let log_path = paths::debug_log_path();
    if let Err(error) = append_debug_log(&log_path, entry).await {
        eprintln!("Milevox could not write debug log: {error:#}");
    }
}

async fn append_debug_log(path: &Path, entry: &str) -> Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("failed to open debug log at {}", path.display()))?;
    file.write_all(entry.as_bytes())
        .await
        .with_context(|| format!("failed to write debug log at {}", path.display()))?;
    file.write_all(b"\n\n")
        .await
        .with_context(|| format!("failed to finish debug log entry at {}", path.display()))?;
    file.flush()
        .await
        .with_context(|| format!("failed to flush debug log at {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_stabilizer_commits_an_agreed_prefix() {
        let mut stabilizer = PreviewStabilizer::default();

        assert_eq!(stabilizer.stabilize("Troy and a"), "Troy and a");
        assert_eq!(stabilizer.stabilize("troy, and Abed"), "Troy and Abed");
        assert_eq!(
            stabilizer.stabilize("Troy and a bed in the morning"),
            "Troy and a bed in the morning"
        );
    }

    #[test]
    fn preview_stabilizer_keeps_committed_words_during_a_short_decode() {
        let mut stabilizer = PreviewStabilizer::default();
        stabilizer.stabilize("Greendale Community College");
        stabilizer.stabilize("greendale community campus");

        assert_eq!(stabilizer.stabilize("Greendale"), "Greendale Community");
    }

    #[test]
    fn diagnostic_compares_every_transcription_stage() {
        let previews = PreviewTranscripts {
            raw: Some("Troy and Abed in the morning".to_owned()),
            stabilized: Some("Troy and Abed in the morning".to_owned()),
        };
        let comparison = debug_entry(
            7,
            &previews,
            "Troy and a bed in the morning",
            "Troy and Abed in the morning",
            DebugOutcome {
                provider_text: Some("Troy and Abed in the morning."),
                delivered_text: "Troy and Abed in the morning.",
                warning: None,
                error: None,
            },
        );

        assert_eq!(
            comparison,
            "=== RECORDING 7 ===\n\
             LAST RAW PREVIEW:\nTroy and Abed in the morning\n\n\
             LAST STABILIZED PREVIEW:\nTroy and Abed in the morning\n\n\
             FINAL RAW:\nTroy and a bed in the morning\n\n\
             POST-PROCESSING INPUT:\nTroy and Abed in the morning\n\n\
             PROVIDER RESPONSE:\nTroy and Abed in the morning.\n\n\
             DELIVERED TEXT:\nTroy and Abed in the morning.\n\n\
             WARNING:\n[none]\n\n\
             ERROR:\n[none]"
        );
    }

    #[test]
    fn last_diagnostic_replaces_the_previous_entry() {
        let mut last = LastDebugEntry::default();

        assert_eq!(
            last.event(State::Idle).message.as_deref(),
            Some("No transcription diagnostics are available")
        );
        last.remember("Troy's transcription".to_owned());
        last.remember("Abed's transcription".to_owned());

        let event = last.event(State::Idle);
        assert_eq!(event.debug_entry.as_deref(), Some("Abed's transcription"));
        assert!(event.message.is_none());
    }

    #[test]
    fn failed_transcription_replaces_stale_diagnostics() {
        let previews = PreviewTranscripts {
            raw: Some("Dean-a-ling".to_owned()),
            stabilized: Some("Dean a ling".to_owned()),
        };
        let mut last = LastDebugEntry::default();
        last.remember("previous transcription".to_owned());

        last.remember(failed_debug_entry(
            9,
            &previews,
            "Parakeet transcription failed",
        ));

        let entry = last.event(State::Error).debug_entry.unwrap();
        assert!(entry.contains("LAST RAW PREVIEW:\nDean-a-ling"));
        assert!(entry.contains("ERROR:\nParakeet transcription failed"));
        assert!(!entry.contains("previous transcription"));
    }

    #[test]
    fn resetting_the_debug_log_discards_the_previous_session() {
        let directory =
            std::env::temp_dir().join(format!("milevox-debug-log-reset-{}", std::process::id()));
        let path = directory.join("debug.log");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(&path, "previous session").unwrap();

        reset_debug_log(&path).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn debug_entries_append_within_a_session() {
        let directory =
            std::env::temp_dir().join(format!("milevox-debug-log-append-{}", std::process::id()));
        let path = directory.join("debug.log");
        reset_debug_log(&path).unwrap();

        append_debug_log(&path, "recording one").await.unwrap();
        append_debug_log(&path, "recording two").await.unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "recording one\n\nrecording two\n\n"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn post_processing_prefers_the_stabilized_preview() {
        let previews = PreviewTranscripts {
            raw: Some("Troy and a bed".to_owned()),
            stabilized: Some("Troy and Abed".to_owned()),
        };

        assert_eq!(
            select_post_processing_input(&previews, "Troy and a bet"),
            "Troy and Abed"
        );
    }

    #[test]
    fn post_processing_uses_the_final_decode_without_a_preview() {
        let previews = PreviewTranscripts {
            raw: None,
            stabilized: None,
        };

        assert_eq!(
            select_post_processing_input(&previews, "Study at Greendale"),
            "Study at Greendale"
        );
    }

    #[test]
    fn changing_provider_selects_its_default_model() {
        let mut config = Config::default();
        config.post_processing.model = Some("~openai/gpt-mini-latest".to_owned());

        let updated = apply_settings(
            &config,
            None,
            Some(PostProcessingProvider::OpencodeZen),
            None,
        )
        .unwrap();

        assert_eq!(
            updated.post_processing.provider,
            PostProcessingProvider::OpencodeZen
        );
        assert!(updated.post_processing.model.is_none());
        assert_eq!(
            post_processing::selected_model(&updated.post_processing),
            "deepseek-v4-flash"
        );
    }

    #[test]
    fn rejects_a_model_from_another_provider() {
        let config = Config::default();

        let error = apply_settings(&config, None, None, Some("glm-5.2".to_owned())).unwrap_err();

        assert!(error.to_string().contains("curated OpenRouter model list"));
    }
}
