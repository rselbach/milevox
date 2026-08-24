use std::collections::BTreeMap;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::audio::{CapturedAudio, Recording, RecordingReader};
use crate::config::{Config, PostProcessingProvider};
use crate::credentials::Credentials;
use crate::ipc::{
    CatalogOption, Command, DebugSettings, DeliveryMethod, Notice, PostProcessingSettings,
    SettingsSnapshot, State, StateEvent, ensure_socket_available,
};
use crate::{output, paths, post_processing, private_file, transcription};

const MAX_IPC_REQUEST_BYTES: usize = 64 * 1024;
const MAX_CLIENTS: usize = 16;
const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const PREVIEW_INTERVAL: Duration = Duration::from_millis(500);
const PREVIEW_OVERLAP_SECONDS: usize = 1;
const DEBUG_LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;

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
    CaptureFailed {
        generation: u64,
        error: String,
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
    delivery: DeliveryMethod,
    notices: Vec<Notice>,
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
    provider_attempts: &'a [post_processing::ProviderAttempt],
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
    completion_waiters: Vec<oneshot::Sender<StateEvent>>,
}

pub async fn run(config: Config, config_path: PathBuf) -> Result<()> {
    paths::prepare_runtime_dir()?;
    ensure_socket_available().await?;
    let socket_path = paths::socket_path();
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to listen at {}", socket_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to secure {}", socket_path.display()))?;
    }

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
        completion_waiters: Vec::new(),
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
    let clients = std::sync::Arc::new(Semaphore::new(MAX_CLIENTS));
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = result.context("failed to accept a Milevox client")?;
                let Ok(permit) = clients.clone().try_acquire_owned() else {
                    eprintln!("Milevox rejected a client because the connection limit was reached");
                    continue;
                };
                let inbox = inbox.clone();
                let events = events.subscribe();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(error) = handle_client(stream, inbox, events).await {
                        eprintln!("Milevox client error: {error:#}");
                    }
                });
            }
            result = shutdown_signal() => {
                result?;
                return Ok(());
            }
        }
    }
}

async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut interrupt =
            signal(SignalKind::interrupt()).context("failed to listen for SIGINT")?;
        let mut terminate =
            signal(SignalKind::terminate()).context("failed to listen for SIGTERM")?;
        tokio::select! {
            _ = interrupt.recv() => Ok(()),
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .context("failed to listen for shutdown signal")
    }
}

async fn handle_client(
    stream: UnixStream,
    inbox: mpsc::Sender<ActorMessage>,
    mut events: watch::Receiver<StateEvent>,
) -> Result<()> {
    verify_peer(&stream)?;
    let (reader, mut writer) = stream.into_split();
    let line = tokio::time::timeout(CLIENT_READ_TIMEOUT, read_request(reader))
        .await
        .context("client request timed out")??;
    let command: Command = serde_json::from_slice(&line).context("invalid client command")?;

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
    tokio::time::timeout(CLIENT_WRITE_TIMEOUT, async {
        writer.write_all(&line).await?;
        writer.write_all(b"\n").await
    })
    .await
    .context("client write timed out")??;
    Ok(())
}

async fn read_request(mut reader: tokio::net::unix::OwnedReadHalf) -> Result<Vec<u8>> {
    let mut request = Vec::with_capacity(1024);
    let mut buffer = [0_u8; 4096];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            bail!("client sent no complete command");
        }
        if let Some(newline) = buffer[..count].iter().position(|byte| *byte == b'\n') {
            if request.len() + newline > MAX_IPC_REQUEST_BYTES {
                bail!("client command exceeds {MAX_IPC_REQUEST_BYTES} bytes");
            }
            request.extend_from_slice(&buffer[..newline]);
            return Ok(request);
        }
        if request.len() + count > MAX_IPC_REQUEST_BYTES {
            bail!("client command exceeds {MAX_IPC_REQUEST_BYTES} bytes");
        }
        request.extend_from_slice(&buffer[..count]);
    }
}

fn verify_peer(stream: &UnixStream) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `credentials` and `length` point to valid writable storage for SO_PEERCRED.
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut credentials).cast(),
                &raw mut length,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).context("failed to inspect client UID");
        }
        verify_peer_uid(credentials.uid)?;
    }
    Ok(())
}

fn verify_peer_uid(uid: u32) -> Result<()> {
    if uid != paths::current_uid() {
        bail!("rejected Milevox client owned by another user");
    }
    Ok(())
}

impl Daemon {
    async fn run(mut self, mut receiver: mpsc::Receiver<ActorMessage>) {
        while let Some(message) = receiver.recv().await {
            match message {
                ActorMessage::Command { command, reply } => {
                    if matches!(&command, Command::StopAndWait) {
                        let event = self.stop_recording();
                        if self.pipeline.is_some() {
                            self.completion_waiters.push(reply);
                        } else {
                            let _ = reply.send(event);
                        }
                    } else {
                        let event = self.handle_command(command).await;
                        let _ = reply.send(event);
                    }
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
                ActorMessage::CaptureFailed { generation, error } => {
                    self.capture_failed(generation, error);
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
                    let event = match completion.result {
                        Ok(result) => self.publish(StateEvent::completed(
                            result.transcript,
                            result.delivery,
                            result.notices,
                        )),
                        Err(error) => self.publish(StateEvent::error(
                            State::Error,
                            "pipeline_failed",
                            format!("{error:#}"),
                        )),
                    };
                    self.resolve_completion_waiters(event);
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
            Command::StopAndWait => self.current_event(),
            Command::Toggle => match self.current_state() {
                State::Recording => self.stop_recording(),
                State::Idle | State::Error => self.start_recording(),
                State::Transcribing | State::Refining => self.cancel().await,
            },
            Command::Cancel => self.cancel().await,
            Command::Status { .. } => self.current_event(),
            Command::Settings {
                enabled,
                provider,
                model,
            } => self.update_settings(enabled, provider, model),
            Command::SettingsModels { provider } => {
                let mut config = self.config.clone();
                if let Some(provider) = provider {
                    config.post_processing.provider = provider;
                    config.post_processing.model = None;
                }
                StateEvent::new(self.current_state())
                    .with_settings(settings_snapshot(&config, &self.credentials))
            }
            Command::SetToken { provider, token } => self.update_token(provider, token),
            Command::RemoveToken { provider } => self.remove_token(provider),
            Command::Debug { enabled } => self.update_debug(enabled),
            Command::DebugLast => self.last_debug(),
            Command::DebugClear => self.clear_debug(),
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
            return self.decorate(StateEvent::error(
                self.current_state(),
                "settings_busy",
                "Post-processing settings cannot change during dictation",
            ));
        }

        let latest = match Config::load(&self.config_path) {
            Ok(config) => config,
            Err(error) => {
                return self.decorate(StateEvent::error(
                    self.current_state(),
                    "config_reload_failed",
                    format!("Could not reload post-processing settings: {error:#}"),
                ));
            }
        };
        let model_value = model.clone();
        let provider_changed =
            provider.is_some_and(|provider| provider != latest.post_processing.provider);
        let updated = match apply_settings(&latest, enabled, provider, model) {
            Ok(updated) => updated,
            Err(error) => {
                return self.decorate(StateEvent::error(
                    self.current_state(),
                    "invalid_settings",
                    format!("Invalid post-processing settings: {error:#}"),
                ));
            }
        };
        if let Err(error) = updated.save_post_processing(
            &self.config_path,
            enabled,
            provider,
            model_value.as_deref(),
            provider_changed && model_value.is_none(),
        ) {
            return self.decorate(StateEvent::error(
                self.current_state(),
                "settings_save_failed",
                format!("Could not save post-processing settings: {error:#}"),
            ));
        }

        self.config = Config::load(&self.config_path).unwrap_or(updated);
        self.publish(StateEvent::configuration_changed())
    }

    fn update_token(
        &mut self,
        provider: Option<PostProcessingProvider>,
        token: String,
    ) -> StateEvent {
        if self.recording.is_some() || self.pipeline.is_some() {
            return self.decorate(StateEvent::error(
                self.current_state(),
                "token_busy",
                "Provider tokens cannot change during dictation",
            ));
        }

        let provider = provider.unwrap_or(self.config.post_processing.provider);
        let mut updated = self.credentials.clone();
        if let Err(error) = updated.set(provider, token) {
            return self.decorate(StateEvent::error(
                self.current_state(),
                "invalid_token",
                format!("Invalid provider token: {error:#}"),
            ));
        }
        if let Err(error) = updated.save(&self.credentials_path) {
            return self.decorate(StateEvent::error(
                self.current_state(),
                "token_save_failed",
                format!("Could not save provider token: {error:#}"),
            ));
        }

        self.credentials = updated;
        self.publish(StateEvent::configuration_changed())
    }

    fn remove_token(&mut self, provider: Option<PostProcessingProvider>) -> StateEvent {
        if self.recording.is_some() || self.pipeline.is_some() {
            return self.decorate(StateEvent::error(
                self.current_state(),
                "token_busy",
                "Provider tokens cannot change during dictation",
            ));
        }
        let provider = provider.unwrap_or(self.config.post_processing.provider);
        let mut updated = self.credentials.clone();
        let removed = updated.remove(provider);
        if removed && let Err(error) = updated.save(&self.credentials_path) {
            return self.decorate(StateEvent::error(
                self.current_state(),
                "token_remove_failed",
                format!("Could not remove provider token: {error:#}"),
            ));
        }
        self.credentials = updated;
        let mut selected = self.config.post_processing.clone();
        selected.provider = provider;
        selected.model = None;
        let text = if self.credentials.is_configured(&selected) {
            "Stored token removed; an environment token remains active"
        } else if removed {
            "Stored token removed"
        } else {
            "No stored token was configured"
        };
        self.publish(
            StateEvent::configuration_changed().with_notice(Notice::info("token_removed", text)),
        )
    }

    fn update_debug(&mut self, enabled: bool) -> StateEvent {
        if self.recording.is_some() || self.pipeline.is_some() {
            return self.decorate(StateEvent::error(
                self.current_state(),
                "debug_busy",
                "Debug logging cannot change during dictation",
            ));
        }

        let mut updated = match Config::load(&self.config_path) {
            Ok(config) => config,
            Err(error) => {
                return self.decorate(StateEvent::error(
                    self.current_state(),
                    "config_reload_failed",
                    format!("Could not reload debug setting: {error:#}"),
                ));
            }
        };
        updated.debug.enabled = enabled;
        if let Err(error) = updated.save_debug_enabled(&self.config_path, enabled) {
            return self.decorate(StateEvent::error(
                self.current_state(),
                "debug_save_failed",
                format!("Could not save debug setting: {error:#}"),
            ));
        }

        self.config = updated;
        self.publish(StateEvent::configuration_changed())
    }

    fn clear_debug(&mut self) -> StateEvent {
        if self.recording.is_some() || self.pipeline.is_some() {
            return self.decorate(StateEvent::error(
                self.current_state(),
                "debug_busy",
                "Debug logs cannot be cleared during dictation",
            ));
        }
        match clear_debug_logs(&paths::debug_log_path()) {
            Ok(()) => {
                self.last_debug_entry = LastDebugEntry::default();
                self.publish(
                    StateEvent::configuration_changed()
                        .with_notice(Notice::info("debug_cleared", "Debug logs cleared")),
                )
            }
            Err(error) => self.decorate(StateEvent::error(
                self.current_state(),
                "debug_clear_failed",
                format!("Could not clear debug logs: {error:#}"),
            )),
        }
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
            Err(error) => self.publish(StateEvent::error(
                State::Error,
                "microphone_start_failed",
                format!("{error:#}"),
            )),
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
                return self.publish(StateEvent::error(
                    State::Error,
                    "microphone_finish_failed",
                    error,
                ));
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
            self.transcriber.cancel(recording.generation);
            recording.preview_task.abort();
            recording.level_task.abort();
        }
        if let Some(pipeline) = self.pipeline.take() {
            self.transcriber.cancel(pipeline.generation);
            pipeline.task.abort();
        }
        self.raw_preview_transcript = None;
        self.partial_transcript = None;
        self.level = 0.0;
        let event = self.publish(
            StateEvent::new(State::Idle)
                .with_notice(Notice::info("canceled", "Dictation canceled")),
        );
        self.resolve_completion_waiters(event.clone());
        event
    }

    fn capture_failed(&mut self, generation: u64, error: String) {
        if self
            .recording
            .as_ref()
            .map(|recording| recording.generation)
            != Some(generation)
        {
            return;
        }
        if let Some(recording) = self.recording.take() {
            recording.preview_task.abort();
            recording.level_task.abort();
        }
        self.transcriber.cancel(generation);
        self.raw_preview_transcript = None;
        self.partial_transcript = None;
        self.level = 0.0;
        let event = self.publish(StateEvent::error(
            State::Error,
            "microphone_capture_failed",
            format!("Microphone capture failed: {error}"),
        ));
        self.resolve_completion_waiters(event);
    }

    fn resolve_completion_waiters(&mut self, event: StateEvent) {
        for waiter in self.completion_waiters.drain(..) {
            let _ = waiter.send(event.clone());
        }
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
    let provider_options = post_processing::PROVIDERS
        .iter()
        .map(|provider| CatalogOption {
            value: provider.provider.as_str().into(),
            label: provider.label.into(),
        })
        .collect();
    let model_catalog = post_processing::PROVIDERS
        .iter()
        .map(|provider| {
            (
                provider.provider.as_str().into(),
                provider
                    .models
                    .iter()
                    .map(|model| CatalogOption {
                        value: model.value.into(),
                        label: model.label.into(),
                    })
                    .collect(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    SettingsSnapshot {
        post_processing: PostProcessingSettings {
            enabled: config.post_processing.enabled,
            provider: config.post_processing.provider,
            model: post_processing::selected_model(&config.post_processing).to_owned(),
            token_configured: credentials.is_configured(&config.post_processing),
            token_source: credentials.source(&config.post_processing),
            provider_options,
            model_catalog,
        },
        debug: DebugSettings {
            enabled: config.debug.enabled,
        },
    }
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
            match reader.stream_error() {
                Ok(Some(error)) => {
                    let _ = inbox
                        .send(ActorMessage::CaptureFailed { generation, error })
                        .await;
                    return;
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = inbox
                        .send(ActorMessage::CaptureFailed {
                            generation,
                            error: format!("{error:#}"),
                        })
                        .await;
                    return;
                }
            }
            let level = match reader.level() {
                Ok(level) => level,
                Err(error) => {
                    let _ = inbox
                        .send(ActorMessage::CaptureFailed {
                            generation,
                            error: format!("{error:#}"),
                        })
                        .await;
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
        let mut last_sample_count = 0;
        let mut merged_transcript = String::new();
        let overlap = usize::try_from(reader.sample_rate())
            .unwrap_or(usize::MAX)
            .saturating_mul(PREVIEW_OVERLAP_SECONDS);
        loop {
            tokio::time::sleep(PREVIEW_INTERVAL).await;
            let sample_count = match reader.sample_count() {
                Ok(sample_count) => sample_count,
                Err(error) => {
                    eprintln!("Milevox live preview stopped: {error:#}");
                    return;
                }
            };
            let minimum_samples = usize::try_from(reader.sample_rate() / 2).unwrap_or(usize::MAX);
            if sample_count < minimum_samples
                || sample_count.saturating_sub(last_sample_count) < minimum_samples
            {
                continue;
            }
            let preview_start = last_sample_count.saturating_sub(overlap);
            let audio = match reader.snapshot_from(preview_start) {
                Ok(audio) => audio,
                Err(error) => {
                    eprintln!("Milevox live preview stopped: {error:#}");
                    return;
                }
            };
            // Advance by captured audio, not successful inference. A failed or empty preview
            // must not make every later attempt copy and retranscribe the full recording.
            last_sample_count = sample_count;
            let segment = match transcriber.transcribe_preview(generation, audio).await {
                Ok(Some(transcript)) => transcript,
                Ok(None) => continue,
                Err(error) => {
                    eprintln!("Milevox live preview failed: {error:#}");
                    continue;
                }
            };
            merged_transcript = merge_preview(&merged_transcript, &segment);
            if merged_transcript.is_empty() {
                continue;
            }
            if inbox
                .send(ActorMessage::Preview {
                    generation,
                    raw_transcript: merged_transcript.clone(),
                    transcript: merged_transcript.clone(),
                })
                .await
                .is_err()
            {
                return;
            }
        }
    })
}

fn normalize_word(word: &str) -> String {
    word.trim_matches(|character: char| !character.is_alphanumeric())
        .to_lowercase()
}

fn merge_preview(existing: &str, next: &str) -> String {
    if existing.trim().is_empty() {
        return next.trim().to_owned();
    }
    let left = existing.split_whitespace().collect::<Vec<_>>();
    let right = next.split_whitespace().collect::<Vec<_>>();
    let overlap = (1..=left.len().min(right.len()))
        .rev()
        .find(|count| {
            left[left.len() - count..]
                .iter()
                .zip(&right[..*count])
                .all(|(left, right)| normalize_word(left) == normalize_word(right))
        })
        .unwrap_or(0);
    left.iter()
        .copied()
        .chain(right[overlap..].iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
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
    let raw = match transcriber.transcribe(generation, audio).await {
        Ok(raw) => raw,
        Err(error) => {
            let entry = failed_debug_entry(generation, &previews, &format!("{error:#}"));
            if let Err(log_error) = persist_debug_entry(config.debug.enabled, &entry).await {
                eprintln!("Milevox could not write debug log: {log_error:#}");
            }
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
        if let Err(log_error) = persist_debug_entry(config.debug.enabled, &entry).await {
            eprintln!("Milevox could not write debug log: {log_error:#}");
        }
        return PipelineCompletion {
            result: Err(error),
            debug_entry: entry,
        };
    }
    let api_key = credentials.resolve(&config.post_processing);
    let refined = post_processing::refine(&config.post_processing, api_key.as_deref(), &raw).await;
    let delivery = output::deliver(&config.output, &refined.text).await;
    let delivery_error = delivery.as_ref().err().map(|error| format!("{error:#}"));
    let mut notices = refined
        .warning
        .as_ref()
        .map(|warning| {
            vec![
                Notice::warning(
                    "post_processing_fallback",
                    "Post-processing failed; delivered the original transcript",
                )
                .with_detail(warning.clone()),
            ]
        })
        .unwrap_or_default();
    if let Ok(delivery) = &delivery {
        notices.extend(delivery.notices.clone());
    }
    let diagnostic_warning = notices
        .iter()
        .map(|notice| notice.text.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    let entry = debug_entry(
        generation,
        &previews,
        &raw,
        &raw,
        DebugOutcome {
            provider_attempts: &refined.provider_attempts,
            delivered_text: &refined.text,
            warning: (!diagnostic_warning.is_empty()).then_some(diagnostic_warning.as_str()),
            error: delivery_error.as_deref(),
        },
    );
    if let Err(error) = persist_debug_entry(config.debug.enabled, &entry).await {
        eprintln!("Milevox could not write debug log: {error:#}");
        notices.push(
            Notice::warning(
                "debug_write_failed",
                "The transcript was delivered, but its debug log could not be written",
            )
            .with_detail(format!("{error:#}")),
        );
    }
    let result = delivery.map(|delivery| PipelineResult {
        transcript: refined.text,
        delivery: delivery.method,
        notices,
    });

    PipelineCompletion {
        result,
        debug_entry: entry,
    }
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
    let provider_attempts = format_provider_attempts(outcome.provider_attempts);
    let warning = outcome.warning.unwrap_or("[none]");
    let error = outcome.error.unwrap_or("[none]");
    let delivered_text = outcome.delivered_text;
    format!(
        "=== RECORDING {generation} ===\nLAST RAW PREVIEW:\n{raw_preview}\n\nLAST STABILIZED PREVIEW:\n{stabilized_preview}\n\nFINAL RAW:\n{final_raw}\n\nPOST-PROCESSING INPUT:\n{post_processing_input}\n\n{provider_attempts}\n\nDELIVERED TEXT:\n{delivered_text}\n\nWARNING:\n{warning}\n\nERROR:\n{error}"
    )
}

fn format_provider_attempts(attempts: &[post_processing::ProviderAttempt]) -> String {
    if attempts.is_empty() {
        return "PROVIDER RESPONSES:\n[unavailable]".to_owned();
    }

    attempts
        .iter()
        .enumerate()
        .map(|(index, attempt)| {
            let number = index + 1;
            let validation = attempt.validation_error.as_deref().map_or_else(
                || "accepted".to_owned(),
                |error| format!("rejected: {error}"),
            );
            format!(
                "PROVIDER RESPONSE {number}:\n{}\n\nPROVIDER VALIDATION {number}:\n{validation}",
                attempt.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn failed_debug_entry(generation: u64, previews: &PreviewTranscripts, error: &str) -> String {
    debug_entry(
        generation,
        previews,
        "[unavailable]",
        "[unavailable]",
        DebugOutcome {
            provider_attempts: &[],
            delivered_text: "[unavailable]",
            warning: None,
            error: Some(error),
        },
    )
}

async fn persist_debug_entry(enabled: bool, entry: &str) -> Result<()> {
    if !enabled {
        return Ok(());
    }

    let log_path = paths::debug_log_path();
    let entry = entry.to_owned();
    tokio::task::spawn_blocking(move || append_debug_log(&log_path, &entry))
        .await
        .context("debug log writer stopped unexpectedly")?
}

fn append_debug_log(path: &Path, entry: &str) -> Result<()> {
    let entry_size = u64::try_from(entry.len().saturating_add(2)).unwrap_or(u64::MAX);
    let current_size = path.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    if current_size > 0 && current_size.saturating_add(entry_size) > DEBUG_LOG_MAX_BYTES {
        let backup = debug_log_backup(path);
        // The live log may predate permission hardening. Secure it before it becomes the
        // retained backup so both generations are private.
        private_file::secure(path)?;
        match std::fs::remove_file(&backup) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("failed to remove old debug log backup"),
        }
        std::fs::rename(path, &backup)
            .with_context(|| format!("failed to rotate debug log at {}", path.display()))?;
    }
    let mut file = private_file::open_append(path)?;
    file.write_all(entry.as_bytes())
        .with_context(|| format!("failed to write debug log at {}", path.display()))?;
    file.write_all(b"\n\n")
        .with_context(|| format!("failed to finish debug log entry at {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync debug log at {}", path.display()))?;
    Ok(())
}

fn debug_log_backup(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    name.into()
}

fn clear_debug_logs(path: &Path) -> Result<()> {
    for candidate in [path.to_path_buf(), debug_log_backup(path)] {
        match std::fs::remove_file(&candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to remove {}", candidate.display()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_merging_uses_the_longest_normalized_suffix_prefix() {
        assert_eq!(
            merge_preview("Troy and Abed,", "abed in the morning"),
            "Troy and Abed, in the morning"
        );
        assert_eq!(merge_preview("one two", "three four"), "one two three four");
        assert_eq!(merge_preview("", "  first preview  "), "first preview");
    }

    #[test]
    fn a_shorter_preview_does_not_replace_the_authoritative_final_decode() {
        let previews = PreviewTranscripts {
            raw: Some("First sentence".to_owned()),
            stabilized: Some("First sentence".to_owned()),
        };
        let final_decode = "First sentence. A final sentence.";
        let entry = debug_entry(
            1,
            &previews,
            final_decode,
            final_decode,
            DebugOutcome {
                provider_attempts: &[],
                delivered_text: final_decode,
                warning: None,
                error: None,
            },
        );

        assert!(entry.contains("POST-PROCESSING INPUT:\nFirst sentence. A final sentence."));
        assert!(entry.contains("DELIVERED TEXT:\nFirst sentence. A final sentence."));
    }

    #[test]
    fn diagnostic_compares_every_transcription_stage() {
        let previews = PreviewTranscripts {
            raw: Some("Troy and Abed in the morning".to_owned()),
            stabilized: Some("Troy and Abed in the morning".to_owned()),
        };
        let provider_attempts = [post_processing::ProviderAttempt {
            text: "Troy and Abed in the morning.".to_owned(),
            validation_error: None,
        }];
        let comparison = debug_entry(
            7,
            &previews,
            "Troy and a bed in the morning",
            "Troy and a bed in the morning",
            DebugOutcome {
                provider_attempts: &provider_attempts,
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
             POST-PROCESSING INPUT:\nTroy and a bed in the morning\n\n\
             PROVIDER RESPONSE 1:\nTroy and Abed in the morning.\n\n\
             PROVIDER VALIDATION 1:\naccepted\n\n\
             DELIVERED TEXT:\nTroy and Abed in the morning.\n\n\
             WARNING:\n[none]\n\n\
             ERROR:\n[none]"
        );
    }

    #[test]
    fn diagnostic_records_every_provider_attempt() {
        let attempts = [
            post_processing::ProviderAttempt {
                text: "This is a test.\nThis is another test.".to_owned(),
                validation_error: Some(
                    "output changes dictated word 5 (`new` became `this`)".to_owned(),
                ),
            },
            post_processing::ProviderAttempt {
                text: "This is a test, new line. This is another test.".to_owned(),
                validation_error: None,
            },
        ];

        assert_eq!(
            format_provider_attempts(&attempts),
            "PROVIDER RESPONSE 1:\n\
             This is a test.\nThis is another test.\n\n\
             PROVIDER VALIDATION 1:\n\
             rejected: output changes dictated word 5 (`new` became `this`)\n\n\
             PROVIDER RESPONSE 2:\n\
             This is a test, new line. This is another test.\n\n\
             PROVIDER VALIDATION 2:\n\
             accepted"
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
    fn debug_entries_append_across_sessions_and_correct_permissions() {
        let directory =
            std::env::temp_dir().join(format!("milevox-debug-log-append-{}", std::process::id()));
        let path = directory.join("debug.log");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(&path, "previous session\n\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        append_debug_log(&path, "new session").unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "previous session\n\nnew session\n\n"
        );
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

    #[test]
    fn debug_log_rotates_one_private_backup_and_clear_removes_both() {
        let directory =
            std::env::temp_dir().join(format!("milevox-debug-log-rotation-{}", std::process::id()));
        let path = directory.join("debug.log");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(&path, vec![b'x'; DEBUG_LOG_MAX_BYTES as usize]).unwrap();

        append_debug_log(&path, "next recording").unwrap();

        let backup = debug_log_backup(&path);
        assert_eq!(
            std::fs::metadata(&backup).unwrap().len(),
            DEBUG_LOG_MAX_BYTES
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "next recording\n\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        clear_debug_logs(&path).unwrap();
        assert!(!path.exists());
        assert!(!backup.exists());
        clear_debug_logs(&path).unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn ipc_reader_enforces_the_request_limit() {
        let (client, server) = UnixStream::pair().unwrap();
        let (reader, _) = server.into_split();
        let write = tokio::spawn(async move {
            let (_, mut writer) = client.into_split();
            writer
                .write_all(&vec![b'x'; MAX_IPC_REQUEST_BYTES + 1])
                .await
                .unwrap();
            writer.write_all(b"\n").await.unwrap();
        });

        let error = read_request(reader).await.unwrap_err();
        assert!(error.to_string().contains("exceeds"));
        write.await.unwrap();
    }

    #[tokio::test]
    async fn ipc_peer_for_a_local_socket_has_the_current_uid() {
        let (client, _server) = UnixStream::pair().unwrap();
        verify_peer(&client).unwrap();
        verify_peer_uid(paths::current_uid()).unwrap();
        assert!(
            verify_peer_uid(paths::current_uid().wrapping_add(1))
                .unwrap_err()
                .to_string()
                .contains("another user")
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

        assert!(error.to_string().contains("valid models"));
    }
}
