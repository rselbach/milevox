use std::collections::BTreeMap;
use std::future::Future;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::audio::{CaptureIssue, CapturedAudio, FinishedCapture, Recording, RecordingReader};
use crate::config::{Config, PostProcessingProvider};
use crate::credentials::Credentials;
use crate::ipc::{
    CatalogOption, Command, DebugSettings, DeliveryMethod, LevelEvent, Notice,
    PostProcessingSettings, SettingsSnapshot, State, StateEvent, ensure_socket_available,
};
use crate::{output, paths, post_processing, private_file, transcription};

const MAX_IPC_REQUEST_BYTES: usize = 64 * 1024;
const MAX_CLIENTS: usize = 16;
const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const PREVIEW_INTERVAL: Duration = Duration::from_secs(1);
const DEBUG_LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;
const DEBUG_LOG_TRUNCATION_MARKER: &str = "\n[truncated to fit the 5 MiB debug-log limit]";
const POST_PROCESSING_FALLBACK_TEXT: &str = "Post-processing failed; using the original transcript";
const DEBUG_WRITE_FAILED_TEXT: &str =
    "The final transcript remains available, but its debug log could not be written";

#[derive(Debug)]
struct StartupLock {
    _file: std::fs::File,
}

impl StartupLock {
    fn acquire(path: &Path) -> Result<Self> {
        let file = private_file::open_lock(path)?;

        // SAFETY: `file` owns a valid descriptor for the duration of this call.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN)
            {
                bail!("another Milevox daemon is starting");
            }
            return Err(error)
                .with_context(|| format!("failed to lock startup at {}", path.display()));
        }

        Ok(Self { _file: file })
    }
}

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
        transcript: String,
    },
    Level {
        generation: u64,
        level: f32,
    },
    CaptureEnded {
        generation: u64,
    },
    Completed {
        generation: u64,
        output_target: Option<String>,
        completion: PipelineCompletion,
    },
    CancellationFinished {
        generation: u64,
    },
    ModelStatus(transcription::ModelStatus),
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

enum TerminalOutcome {
    Delivered {
        transcript: String,
        delivery: DeliveryMethod,
        notices: Vec<Notice>,
    },
    DeliveryFailed {
        transcript: String,
        notices: Vec<Notice>,
        error: String,
    },
    Failed(anyhow::Error),
}

struct PipelineCompletion {
    outcome: TerminalOutcome,
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

struct DebugOutcome<'a> {
    provider_response: Option<&'a post_processing::ProviderAttempt>,
    final_text: &'a str,
    delivery_result: &'a str,
    warning: Option<&'a str>,
    error: Option<&'a str>,
}

#[derive(Clone)]
struct DebugLog {
    commands: mpsc::UnboundedSender<DebugLogCommand>,
}

enum DebugLogCommand {
    Append {
        path: PathBuf,
        entry: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Clear {
        path: PathBuf,
        reply: oneshot::Sender<Result<()>>,
    },
}

impl DebugLog {
    fn new() -> Self {
        let (commands, mut receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(command) = receiver.recv().await {
                let (reply, result) = match command {
                    DebugLogCommand::Append { path, entry, reply } => {
                        let result =
                            tokio::task::spawn_blocking(move || append_debug_log(&path, &entry))
                                .await
                                .context("debug log writer stopped unexpectedly")
                                .and_then(|result| result);
                        (reply, result)
                    }
                    DebugLogCommand::Clear { path, reply } => {
                        let result = tokio::task::spawn_blocking(move || clear_debug_logs(&path))
                            .await
                            .context("debug log clearer stopped unexpectedly")
                            .and_then(|result| result);
                        (reply, result)
                    }
                };
                let _ = reply.send(result);
            }
        });
        Self { commands }
    }

    async fn persist(&self, enabled: bool, entry: &str) -> Result<()> {
        if !enabled {
            return Ok(());
        }
        self.persist_at(paths::debug_log_path(), entry).await
    }

    async fn persist_at(&self, path: PathBuf, entry: &str) -> Result<()> {
        self.submit_append(path, entry.to_owned())?
            .await
            .context("debug log worker stopped before append completed")?
    }

    fn submit_append(&self, path: PathBuf, entry: String) -> Result<oneshot::Receiver<Result<()>>> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(DebugLogCommand::Append { path, entry, reply })
            .map_err(|_| anyhow::anyhow!("debug log worker is unavailable"))?;
        Ok(response)
    }

    async fn clear(&self, path: PathBuf) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(DebugLogCommand::Clear { path, reply })
            .map_err(|_| anyhow::anyhow!("debug log worker is unavailable"))?;
        response
            .await
            .context("debug log worker stopped before clear completed")?
    }
}

struct Pipeline {
    generation: u64,
    output_target: Option<String>,
    task: JoinHandle<()>,
}

struct PipelineRequest {
    generation: u64,
    output_target: Option<String>,
    audio: CapturedAudio,
    capture_warning: Option<CaptureIssue>,
    last_preview: Option<String>,
    config: Config,
    credentials: Credentials,
    transcriber: transcription::ParakeetTranscriber,
    post_processor: post_processing::PostProcessor,
    debug_log: DebugLog,
}

fn pipeline_identity_matches(
    active_generation: u64,
    active_target: Option<&str>,
    completion_generation: u64,
    completion_target: Option<&str>,
) -> bool {
    active_generation == completion_generation && active_target == completion_target
}

struct ActiveRecording {
    generation: u64,
    output_target: Option<String>,
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
    canceling_generation: Option<u64>,
    generation: u64,
    last_preview: Option<String>,
    last_debug_entry: LastDebugEntry,
    level: f32,
    events: watch::Sender<StateEvent>,
    levels: watch::Sender<f32>,
    settings: SettingsSnapshot,
    inbox: mpsc::Sender<ActorMessage>,
    transcriber: transcription::ParakeetTranscriber,
    post_processor: post_processing::PostProcessor,
    debug_log: DebugLog,
    model_status: transcription::ModelStatus,
    completion_waiters: Vec<oneshot::Sender<StateEvent>>,
    latest_transcript: Option<String>,
}

pub async fn run(config: Config, config_path: PathBuf) -> Result<()> {
    let runtime_dir = paths::prepare_runtime_dir()?;
    let _startup_lock = StartupLock::acquire(&runtime_dir.join("milevox.lock"))?;
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
    let transcriber = transcription::ParakeetTranscriber::new(&config.transcription);
    let post_processor = post_processing::PostProcessor::new()?;
    let debug_log = DebugLog::new();
    let mut model_status = transcriber.subscribe_status();
    let settings = settings_snapshot(&config, &credentials);
    let initial_event = StateEvent::new(State::Loading).with_settings(settings.clone());
    let (events, _) = watch::channel(initial_event);
    let (levels, _) = watch::channel(0.0);
    let (inbox, receiver) = mpsc::channel(16);
    let daemon = Daemon {
        transcriber,
        post_processor,
        debug_log,
        model_status: transcription::ModelStatus::Loading,
        config,
        config_path,
        credentials,
        credentials_path,
        recording: None,
        pipeline: None,
        canceling_generation: None,
        generation: 0,
        last_preview: None,
        last_debug_entry: LastDebugEntry::default(),
        level: 0.0,
        events: events.clone(),
        levels: levels.clone(),
        settings,
        inbox: inbox.clone(),
        completion_waiters: Vec::new(),
        latest_transcript: None,
    };
    let actor = tokio::spawn(daemon.run(receiver));
    let model_inbox = inbox.clone();
    let model_watcher = tokio::spawn(async move {
        loop {
            let status = model_status.borrow().clone();
            if model_inbox
                .send(ActorMessage::ModelStatus(status))
                .await
                .is_err()
            {
                return;
            }
            if model_status.changed().await.is_err() {
                return;
            }
        }
    });

    let serve_result = serve(listener, &socket_path, inbox.clone(), events, levels).await;
    let (reply, response) = oneshot::channel();
    if inbox.send(ActorMessage::Shutdown { reply }).await.is_ok() {
        let _ = response.await;
    }
    let actor_result = actor.await;
    model_watcher.abort();
    actor_result.context("Milevox daemon task failed")?;
    serve_result
}

async fn serve(
    listener: UnixListener,
    socket_path: &Path,
    inbox: mpsc::Sender<ActorMessage>,
    events: watch::Sender<StateEvent>,
    levels: watch::Sender<f32>,
) -> Result<()> {
    let serve_result = serve_until(listener, inbox, events, levels, shutdown_signal()).await;
    let remove_result = tokio::fs::remove_file(socket_path).await;
    if let Err(error) = remove_result
        && error.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!(
            "Milevox could not remove socket {}: {error}",
            socket_path.display()
        );
    }
    serve_result
}

async fn serve_until<F>(
    listener: UnixListener,
    inbox: mpsc::Sender<ActorMessage>,
    events: watch::Sender<StateEvent>,
    levels: watch::Sender<f32>,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    let clients = std::sync::Arc::new(Semaphore::new(MAX_CLIENTS));
    tokio::pin!(shutdown);
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
                let levels = levels.subscribe();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(error) = handle_client(stream, inbox, events, levels).await {
                        eprintln!("Milevox client error: {error:#}");
                    }
                });
            }
            result = &mut shutdown => {
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
    mut levels: watch::Receiver<f32>,
) -> Result<()> {
    verify_peer(&stream)?;
    let (reader, mut writer) = stream.into_split();
    let line = tokio::time::timeout(CLIENT_READ_TIMEOUT, read_request(reader))
        .await
        .context("client request timed out")??;
    let command: Command = serde_json::from_slice(&line).context("invalid client command")?;

    if let Command::Status {
        follow: true,
        levels: include_levels,
    } = &command
    {
        let initial = events.borrow().clone();
        write_event(&mut writer, &initial).await?;
        if *include_levels {
            let level = *levels.borrow();
            write_level(&mut writer, level).await?;
        }
        loop {
            tokio::select! {
                result = events.changed() => {
                    if result.is_err() {
                        return Ok(());
                    }
                    let event = events.borrow().clone();
                    write_event(&mut writer, &event).await?;
                }
                result = levels.changed(), if *include_levels => {
                    if result.is_err() {
                        return Ok(());
                    }
                    let level = *levels.borrow();
                    write_level(&mut writer, level).await?;
                }
            }
        }
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

async fn write_level(writer: &mut tokio::net::unix::OwnedWriteHalf, level: f32) -> Result<()> {
    let line = serde_json::to_vec(&LevelEvent::new(level))?;
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
                    if matches!(&command, Command::Stop) {
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
                        self.publish(StateEvent::active(state, self.last_preview.clone(), None));
                    }
                }
                ActorMessage::Preview {
                    generation,
                    transcript,
                } => {
                    if self
                        .recording
                        .as_ref()
                        .map(|recording| recording.generation)
                        == Some(generation)
                    {
                        let changed = self.last_preview.as_deref() != Some(&transcript);
                        self.last_preview = Some(transcript);
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
                        self.levels.send_replace(level);
                    }
                }
                ActorMessage::CaptureEnded { generation } => {
                    self.finalize_recording(Some(generation));
                }
                ActorMessage::Completed {
                    generation,
                    output_target,
                    completion,
                } => {
                    if !self.pipeline.as_ref().is_some_and(|pipeline| {
                        pipeline_identity_matches(
                            pipeline.generation,
                            pipeline.output_target.as_deref(),
                            generation,
                            output_target.as_deref(),
                        )
                    }) {
                        continue;
                    }
                    self.pipeline = None;
                    self.last_preview = None;
                    self.level = 0.0;
                    self.levels.send_replace(0.0);
                    self.last_debug_entry.remember(completion.debug_entry);
                    let event = match completion.outcome {
                        TerminalOutcome::Delivered {
                            transcript,
                            delivery,
                            notices,
                        } => {
                            self.latest_transcript = Some(transcript.clone());
                            let event = self.delivered_event(transcript, delivery, notices);
                            self.publish(event)
                        }
                        TerminalOutcome::DeliveryFailed {
                            transcript,
                            notices,
                            error,
                        } => {
                            self.latest_transcript = Some(transcript.clone());
                            self.publish(delivery_failure_event(transcript, notices, error))
                        }
                        TerminalOutcome::Failed(error) => self.publish(StateEvent::error(
                            State::Error,
                            "pipeline_failed",
                            format!("{error:#}"),
                        )),
                    };
                    self.resolve_completion_waiters(event);
                }
                ActorMessage::CancellationFinished { generation } => {
                    if self.canceling_generation == Some(generation) {
                        self.canceling_generation = None;
                        let event = self.quiescent_event();
                        self.publish(event);
                    }
                }
                ActorMessage::ModelStatus(status) => self.update_model_status(status),
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
            Command::Start { output_target } => self.start_recording(output_target),
            Command::Stop => self.current_event(),
            Command::Toggle { output_target } => match self.current_state() {
                State::Recording => self.stop_recording(),
                State::Idle | State::Error => self.start_recording(output_target),
                State::Loading => self.current_event(),
                State::Transcribing | State::Refining | State::Canceling => self.cancel().await,
            },
            Command::Cancel => self.cancel().await,
            Command::CopyLast => self.copy_last().await,
            Command::Status { .. } => self.current_event(),
            Command::Settings {
                enabled,
                provider,
                model,
                ..
            } => self.update_settings(enabled, provider, model),
            Command::SettingsModels { .. } => {
                StateEvent::new(self.current_state()).with_settings(self.settings.clone())
            }
            Command::SetToken { provider, token } => self.update_token(provider, token),
            Command::RemoveToken { provider } => self.remove_token(provider),
            Command::Debug { enabled } => self.update_debug(enabled),
            Command::DebugLast => self.last_debug(),
            Command::DebugClear => self.clear_debug().await,
        }
    }

    fn last_debug(&self) -> StateEvent {
        self.decorate(self.last_debug_entry.event(self.current_state()))
    }

    async fn copy_last(&self) -> StateEvent {
        let Some(transcript) = &self.latest_transcript else {
            return self.decorate(StateEvent::error(
                self.current_state(),
                "no_transcript",
                "No final transcript is available to copy",
            ));
        };
        match output::copy_transcript(transcript).await {
            Ok(()) => self.decorate(
                StateEvent::new(self.current_state())
                    .with_notice(Notice::info("transcript_copied", "Transcript copied")),
            ),
            Err(error) => self.decorate(
                StateEvent::new(self.current_state()).with_notice(
                    Notice::error("clipboard_failed", "The transcript could not be copied")
                        .with_detail(format!("{error:#}")),
                ),
            ),
        }
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
        if self.recording.is_some()
            || self.pipeline.is_some()
            || self.canceling_generation.is_some()
        {
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

        self.config = updated;
        self.refresh_settings();
        let event = self.configuration_changed_event();
        self.publish(event)
    }

    fn update_token(
        &mut self,
        provider: Option<PostProcessingProvider>,
        token: String,
    ) -> StateEvent {
        if self.recording.is_some()
            || self.pipeline.is_some()
            || self.canceling_generation.is_some()
        {
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
        self.refresh_settings();
        let event = self.configuration_changed_event();
        self.publish(event)
    }

    fn remove_token(&mut self, provider: Option<PostProcessingProvider>) -> StateEvent {
        if self.recording.is_some()
            || self.pipeline.is_some()
            || self.canceling_generation.is_some()
        {
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
        self.refresh_settings();
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
        let event = self
            .configuration_changed_event()
            .with_notice(Notice::info("token_removed", text));
        self.publish(event)
    }

    fn update_debug(&mut self, enabled: bool) -> StateEvent {
        if self.recording.is_some()
            || self.pipeline.is_some()
            || self.canceling_generation.is_some()
        {
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
        self.refresh_settings();
        let event = self.configuration_changed_event();
        self.publish(event)
    }

    async fn clear_debug(&mut self) -> StateEvent {
        self.clear_debug_at(&paths::debug_log_path()).await
    }

    async fn clear_debug_at(&mut self, path: &Path) -> StateEvent {
        if self.recording.is_some()
            || self.pipeline.is_some()
            || self.canceling_generation.is_some()
        {
            return self.decorate(StateEvent::error(
                self.current_state(),
                "debug_busy",
                "Debug logs cannot be cleared during dictation",
            ));
        }
        match self.debug_log.clear(path.to_path_buf()).await {
            Ok(()) => {
                self.last_debug_entry = LastDebugEntry::default();
                let event = self
                    .configuration_changed_event()
                    .with_notice(Notice::info("debug_cleared", "Debug logs cleared"));
                self.publish(event)
            }
            Err(error) => self.decorate(StateEvent::error(
                self.current_state(),
                "debug_clear_failed",
                format!("Could not clear debug logs: {error:#}"),
            )),
        }
    }

    fn start_recording(&mut self, output_target: Option<String>) -> StateEvent {
        if self.recording.is_some() {
            return self.current_event();
        }
        if self.pipeline.is_some() || self.canceling_generation.is_some() {
            return self.decorate(StateEvent::error(
                self.current_state(),
                "recording_busy",
                "Milevox is still processing the previous recording",
            ));
        }
        match &self.model_status {
            transcription::ModelStatus::Loading => {
                return self.decorate(StateEvent::error(
                    State::Loading,
                    "recording_busy",
                    "The speech model is still loading",
                ));
            }
            transcription::ModelStatus::Unavailable(error) => {
                return self.decorate(
                    StateEvent::new(State::Error).with_notice(
                        Notice::error("model_unavailable", "The speech model is unavailable")
                            .with_detail(error.clone()),
                    ),
                );
            }
            transcription::ModelStatus::Ready => {}
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
                    output_target,
                    recording,
                    preview_task,
                    level_task,
                });
                self.last_preview = None;
                self.level = 0.0;
                self.levels.send_replace(0.0);
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
        self.finalize_recording(None)
    }

    fn finalize_recording(&mut self, expected_generation: Option<u64>) -> StateEvent {
        if expected_generation.is_some()
            && self
                .recording
                .as_ref()
                .map(|recording| recording.generation)
                != expected_generation
        {
            return self.current_event();
        }
        let Some(active) = self.recording.take() else {
            return self.current_event();
        };
        active.preview_task.abort();
        active.level_task.abort();
        self.level = 0.0;
        self.levels.send_replace(0.0);
        let generation = active.generation;
        let output_target = active.output_target;
        let last_preview = self.last_preview.clone();
        let FinishedCapture { audio, warning } = match active.recording.finish() {
            Ok(capture) => capture,
            Err(error) => {
                let error = format!("{error:#}");
                self.last_debug_entry.remember(failed_debug_entry(
                    generation,
                    &last_preview,
                    &error,
                ));
                self.last_preview = None;
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
        let post_processor = self.post_processor.clone();
        let debug_log = self.debug_log.clone();
        let pipeline_output_target = output_target.clone();
        let completion_output_target = output_target.clone();
        let task = tokio::spawn(async move {
            let completion = run_pipeline(
                PipelineRequest {
                    generation,
                    output_target,
                    audio,
                    capture_warning: warning,
                    last_preview,
                    config,
                    credentials,
                    transcriber,
                    post_processor,
                    debug_log,
                },
                &inbox,
            )
            .await;
            let _ = inbox
                .send(ActorMessage::Completed {
                    generation,
                    output_target: completion_output_target,
                    completion,
                })
                .await;
        });
        self.pipeline = Some(Pipeline {
            generation,
            output_target: pipeline_output_target,
            task,
        });
        self.publish(StateEvent::active(
            State::Transcribing,
            self.last_preview.clone(),
            None,
        ))
    }

    async fn cancel(&mut self) -> StateEvent {
        if self.canceling_generation.is_some() {
            return self.current_event();
        }
        if self.recording.is_none() && self.pipeline.is_none() {
            return self.current_event();
        }
        let mut canceled_generation = None;
        let mut worker_active = false;
        if let Some(recording) = self.recording.take() {
            recording.preview_task.abort();
            recording.level_task.abort();
            canceled_generation = Some(recording.generation);
            worker_active |= self.transcriber.cancel(recording.generation).await;
        }
        if let Some(pipeline) = self.pipeline.take() {
            pipeline.task.abort();
            canceled_generation = Some(pipeline.generation);
            let _ = pipeline.task.await;
            worker_active |= self.transcriber.cancel(pipeline.generation).await;
        }
        self.last_preview = None;
        self.level = 0.0;
        self.levels.send_replace(0.0);
        let mut event = if worker_active {
            StateEvent::new(State::Canceling)
        } else {
            self.quiescent_event()
        };
        event = event.with_notice(Notice::info("canceled", "Dictation canceled"));
        let waiter_state = event.state;
        let event = self.publish(event);
        let stopped = self.decorate(StateEvent::error(
            waiter_state,
            "dictation_canceled",
            "Dictation was canceled before delivery finished",
        ));
        self.resolve_completion_waiters(stopped);
        if worker_active && let Some(generation) = canceled_generation {
            self.canceling_generation = Some(generation);
            let transcriber = self.transcriber.clone();
            let inbox = self.inbox.clone();
            tokio::spawn(async move {
                transcriber.wait_until_available().await;
                let _ = inbox
                    .send(ActorMessage::CancellationFinished { generation })
                    .await;
            });
        }
        event
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

    fn quiescent_event(&self) -> StateEvent {
        if self.canceling_generation.is_some() {
            return StateEvent::new(State::Canceling);
        }
        match &self.model_status {
            transcription::ModelStatus::Loading => StateEvent::new(State::Loading),
            transcription::ModelStatus::Ready => StateEvent::new(State::Idle),
            transcription::ModelStatus::Unavailable(error) => {
                model_unavailable_event(error.clone())
            }
        }
    }

    fn configuration_changed_event(&self) -> StateEvent {
        let quiescent = self.quiescent_event();
        let mut event = StateEvent::configuration_changed(quiescent.state);
        for notice in quiescent.notices {
            event = event.with_notice(notice);
        }
        event
    }

    fn delivered_event(
        &self,
        transcript: String,
        delivery: DeliveryMethod,
        mut notices: Vec<Notice>,
    ) -> StateEvent {
        let quiescent = self.quiescent_event();
        notices.splice(0..0, quiescent.notices);
        StateEvent::completed(quiescent.state, transcript, delivery, notices)
    }

    fn preserve_terminal_payload(&self, mut event: StateEvent) -> StateEvent {
        let current = self.current_event();
        if current.transcript.is_none() {
            return event;
        }
        event.transcript = current.transcript;
        event.delivery = current.delivery;
        for notice in current.notices {
            if !event
                .notices
                .iter()
                .any(|existing| existing.code == notice.code)
            {
                event = event.with_notice(notice);
            }
        }
        event
    }

    fn publish(&self, event: StateEvent) -> StateEvent {
        let event = self.decorate(event);
        self.events.send_replace(event.clone());
        event
    }

    fn decorate(&self, event: StateEvent) -> StateEvent {
        event.with_settings(self.settings.clone())
    }

    fn publish_recording(&self) -> StateEvent {
        self.publish(StateEvent::active(
            State::Recording,
            self.last_preview.clone(),
            None,
        ))
    }

    fn refresh_settings(&mut self) {
        self.settings = settings_snapshot(&self.config, &self.credentials);
    }

    fn update_model_status(&mut self, status: transcription::ModelStatus) {
        self.model_status = status.clone();
        match status {
            transcription::ModelStatus::Loading => {
                if self.recording.is_none()
                    && self.pipeline.is_none()
                    && self.canceling_generation.is_none()
                {
                    self.publish(StateEvent::new(State::Loading));
                }
            }
            transcription::ModelStatus::Ready => {
                if self.canceling_generation.take().is_some()
                    || (self.recording.is_none()
                        && self.pipeline.is_none()
                        && self.current_state() == State::Loading)
                {
                    let event = self.preserve_terminal_payload(StateEvent::new(State::Idle));
                    self.publish(event);
                }
            }
            transcription::ModelStatus::Unavailable(error) => {
                if self.recording.is_none() && self.pipeline.is_none() {
                    self.canceling_generation = None;
                    let event = self.preserve_terminal_payload(model_unavailable_event(error));
                    self.publish(event);
                }
            }
        }
    }
}

fn model_unavailable_event(error: String) -> StateEvent {
    StateEvent::new(State::Error).with_notice(
        Notice::error("model_unavailable", "The speech model is unavailable").with_detail(error),
    )
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

fn delivery_failure_event(transcript: String, notices: Vec<Notice>, error: String) -> StateEvent {
    let mut event = StateEvent::message(State::Error, "Transcript delivery failed")
        .with_transcript(transcript)
        .with_notice(
            Notice::error(
                "delivery_failed",
                "The final transcript could not be delivered",
            )
            .with_detail(error),
        );
    for notice in notices {
        event = event.with_notice(notice);
    }
    event
}

fn apply_settings(
    config: &Config,
    enabled: Option<bool>,
    provider: Option<PostProcessingProvider>,
    model: Option<String>,
) -> Result<Config> {
    let mut updated = config.clone();
    let model_changed = model.is_some();
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
    if updated.post_processing.enabled || provider_changed || model_changed {
        post_processing::validate_model(&updated.post_processing)?;
    } else {
        post_processing::validate(&updated.post_processing)?;
    }
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
            match reader.capture_issue() {
                Ok(Some(_)) => {
                    let _ = inbox.send(ActorMessage::CaptureEnded { generation }).await;
                    return;
                }
                Ok(None) => {}
                Err(_) => {
                    let _ = inbox.send(ActorMessage::CaptureEnded { generation }).await;
                    return;
                }
            }
            let level = match reader.level() {
                Ok(level) => level,
                Err(error) => {
                    eprintln!("Milevox could not calculate the microphone level: {error:#}");
                    let _ = inbox.send(ActorMessage::CaptureEnded { generation }).await;
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
        loop {
            tokio::time::sleep(PREVIEW_INTERVAL).await;
            let sample_count = match reader.sample_count() {
                Ok(sample_count) => sample_count,
                Err(error) => {
                    eprintln!("Milevox live preview stopped: {error:#}");
                    return;
                }
            };
            let Some(window) =
                preview_window(last_sample_count, sample_count, reader.sample_rate())
            else {
                continue;
            };
            let audio = match reader.snapshot_range(window.start, window.end) {
                Ok(audio) => audio,
                Err(error) => {
                    eprintln!("Milevox live preview stopped: {error:#}");
                    return;
                }
            };
            // Advance by captured audio, not successful inference. A failed or empty preview
            // must not make every later attempt copy and retranscribe the full recording.
            last_sample_count = window.end;
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

fn preview_window(
    last_sample_count: usize,
    sample_count: usize,
    sample_rate: u32,
) -> Option<std::ops::Range<usize>> {
    let cadence = usize::try_from(sample_rate).unwrap_or(usize::MAX);
    if cadence == 0
        || sample_count < cadence
        || sample_count.saturating_sub(last_sample_count) < cadence
    {
        return None;
    }
    let overlap = cadence / 2;
    Some(last_sample_count.saturating_sub(overlap)..sample_count)
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
    request: PipelineRequest,
    inbox: &mpsc::Sender<ActorMessage>,
) -> PipelineCompletion {
    let PipelineRequest {
        generation,
        output_target,
        audio,
        capture_warning,
        last_preview,
        config,
        credentials,
        transcriber,
        post_processor,
        debug_log,
    } = request;
    let raw = match transcriber.transcribe(generation, audio).await {
        Ok(raw) => raw,
        Err(error) => {
            let entry = failed_debug_entry(generation, &last_preview, &format!("{error:#}"));
            if let Err(log_error) = debug_log.persist(config.debug.enabled, &entry).await {
                eprintln!("Milevox could not write debug log: {log_error:#}");
            }
            return PipelineCompletion {
                outcome: TerminalOutcome::Failed(error),
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
        let entry = failed_debug_entry(generation, &last_preview, &format!("{error:#}"));
        if let Err(log_error) = debug_log.persist(config.debug.enabled, &entry).await {
            eprintln!("Milevox could not write debug log: {log_error:#}");
        }
        return PipelineCompletion {
            outcome: TerminalOutcome::Failed(error),
            debug_entry: entry,
        };
    }
    let refined = match credentials.resolve(&config.post_processing) {
        Ok(api_key) => {
            post_processor
                .refine(&config.post_processing, api_key.as_deref(), &raw)
                .await
        }
        Err(error) => post_processing::RefinedTranscript {
            text: raw.clone(),
            provider_response: None,
            warning: Some(format!("post-processing skipped: {error:#}")),
        },
    };
    let delivery =
        output::deliver_to_target(&config.output, &refined.text, output_target.as_deref()).await;
    let delivery_error = delivery.as_ref().err().map(|error| format!("{error:#}"));
    let mut notices = pipeline_notices(
        refined.warning.as_deref(),
        capture_warning.as_ref(),
        delivery.as_ref().ok(),
    );
    let diagnostic_warning = diagnostic_warning(&notices);
    let delivery_result = match &delivery {
        Ok(delivery) => delivery_method_name(delivery.method).to_owned(),
        Err(error) => format!("failed: {error:#}"),
    };
    let entry = debug_entry(
        generation,
        &last_preview,
        &raw,
        &raw,
        DebugOutcome {
            provider_response: refined.provider_response.as_ref(),
            final_text: &refined.text,
            delivery_result: &delivery_result,
            warning: diagnostic_warning.as_deref(),
            error: delivery_error.as_deref(),
        },
    );
    if let Err(error) = debug_log.persist(config.debug.enabled, &entry).await {
        eprintln!("Milevox could not write debug log: {error:#}");
        notices.push(
            Notice::warning("debug_write_failed", DEBUG_WRITE_FAILED_TEXT)
                .with_detail(format!("{error:#}")),
        );
    }
    let outcome = terminal_outcome(refined.text, delivery, notices);

    PipelineCompletion {
        outcome,
        debug_entry: entry,
    }
}

fn pipeline_notices(
    refinement_warning: Option<&str>,
    capture_warning: Option<&CaptureIssue>,
    delivery: Option<&output::DeliveryResult>,
) -> Vec<Notice> {
    let mut notices = refinement_warning
        .map(|warning| {
            vec![
                Notice::warning("post_processing_fallback", POST_PROCESSING_FALLBACK_TEXT)
                    .with_detail(warning.to_owned()),
            ]
        })
        .unwrap_or_default();
    if let Some(warning) = capture_warning {
        let (code, text) = match warning {
            CaptureIssue::DurationLimitReached => (
                "capture_duration_limit",
                "The first 10 minutes were submitted automatically",
            ),
            CaptureIssue::Device(_) => (
                "capture_device_warning",
                "Microphone capture ended early; buffered audio was submitted",
            ),
            CaptureIssue::WorkerOverflow => (
                "capture_worker_overflow",
                "Microphone capture could not keep up; buffered audio was submitted",
            ),
        };
        notices.push(Notice::warning(code, text).with_detail(warning.to_string()));
    }
    if let Some(delivery) = delivery {
        notices.extend(delivery.notices.clone());
    }
    notices
}

fn terminal_outcome(
    transcript: String,
    delivery: Result<output::DeliveryResult>,
    notices: Vec<Notice>,
) -> TerminalOutcome {
    match delivery {
        Ok(delivery) => TerminalOutcome::Delivered {
            transcript,
            delivery: delivery.method,
            notices,
        },
        Err(error) => TerminalOutcome::DeliveryFailed {
            transcript,
            notices,
            error: format!("{error:#}"),
        },
    }
}

fn format_notice(notice: &Notice) -> String {
    match notice.detail.as_deref() {
        Some(detail) => format!("{}: {detail}", notice.text),
        None => notice.text.clone(),
    }
}

fn diagnostic_warning(notices: &[Notice]) -> Option<String> {
    let warning = notices
        .iter()
        .map(format_notice)
        .collect::<Vec<_>>()
        .join("; ");
    (!warning.is_empty()).then_some(warning)
}

fn delivery_method_name(method: DeliveryMethod) -> &'static str {
    match method {
        DeliveryMethod::Typed => "typed",
        DeliveryMethod::Clipboard => "clipboard",
        DeliveryMethod::ClipboardFallback => "clipboard_fallback",
        DeliveryMethod::None => "none",
    }
}

fn debug_entry(
    generation: u64,
    last_preview: &Option<String>,
    final_raw: &str,
    post_processing_input: &str,
    outcome: DebugOutcome<'_>,
) -> String {
    let last_preview =
        post_processing::escape_diagnostic_text(last_preview.as_deref().unwrap_or("[unavailable]"));
    let provider_response = format_provider_response(outcome.provider_response);
    let warning = post_processing::escape_diagnostic_text(outcome.warning.unwrap_or("[none]"));
    let error = post_processing::escape_diagnostic_text(outcome.error.unwrap_or("[none]"));
    let final_raw = post_processing::escape_diagnostic_text(final_raw);
    let post_processing_input = post_processing::escape_diagnostic_text(post_processing_input);
    let final_text = post_processing::escape_diagnostic_text(outcome.final_text);
    let delivery_result = post_processing::escape_diagnostic_text(outcome.delivery_result);
    format!(
        "=== RECORDING {generation} ===\nLAST PREVIEW:\n{last_preview}\n\nFINAL RAW:\n{final_raw}\n\nPOST-PROCESSING INPUT:\n{post_processing_input}\n\n{provider_response}\n\nFINAL TEXT:\n{final_text}\n\nDELIVERY RESULT:\n{delivery_result}\n\nWARNING:\n{warning}\n\nERROR:\n{error}"
    )
}

fn format_provider_response(response: Option<&post_processing::ProviderAttempt>) -> String {
    let Some(response) = response else {
        return "PROVIDER RESPONSE:\n[unavailable]\n\nPROVIDER VALIDATION:\n[unavailable]"
            .to_owned();
    };
    let validation = response.validation_error.as_deref().map_or_else(
        || "accepted".to_owned(),
        |error| {
            format!(
                "rejected: {}",
                post_processing::escape_diagnostic_text(error)
            )
        },
    );
    format!(
        "PROVIDER RESPONSE:\n{}\n\nPROVIDER VALIDATION:\n{validation}",
        post_processing::escape_diagnostic_text(&response.text)
    )
}

fn failed_debug_entry(generation: u64, last_preview: &Option<String>, error: &str) -> String {
    debug_entry(
        generation,
        last_preview,
        "[unavailable]",
        "[unavailable]",
        DebugOutcome {
            provider_response: None,
            final_text: "[unavailable]",
            delivery_result: "[unavailable]",
            warning: None,
            error: Some(error),
        },
    )
}

fn append_debug_log(path: &Path, entry: &str) -> Result<()> {
    let entry = bounded_debug_entry(entry);
    let entry_size = u64::try_from(entry.len()).unwrap_or(u64::MAX);
    let backup = debug_log_backup(path);
    private_file::cap(&backup, DEBUG_LOG_MAX_BYTES)?;
    let mut file = private_file::open_append(path)?;
    let mut current_size = file
        .metadata()
        .with_context(|| format!("failed to inspect debug log at {}", path.display()))?
        .len();
    if current_size > DEBUG_LOG_MAX_BYTES {
        file.set_len(DEBUG_LOG_MAX_BYTES)
            .with_context(|| format!("failed to cap debug log at {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync debug log at {}", path.display()))?;
        current_size = DEBUG_LOG_MAX_BYTES;
    }
    if current_size > 0 && current_size.saturating_add(entry_size) > DEBUG_LOG_MAX_BYTES {
        drop(file);
        private_file::rotate(path, &backup)?;
        file = private_file::open_append(path)?;
    }
    file.write_all(&entry)
        .with_context(|| format!("failed to write debug log at {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync debug log at {}", path.display()))?;
    Ok(())
}

fn bounded_debug_entry(entry: &str) -> Vec<u8> {
    const SEPARATOR: &[u8] = b"\n\n";

    let maximum = usize::try_from(DEBUG_LOG_MAX_BYTES).expect("debug log limit fits usize");
    if entry.len().saturating_add(SEPARATOR.len()) <= maximum {
        let mut bytes = Vec::with_capacity(entry.len() + SEPARATOR.len());
        bytes.extend_from_slice(entry.as_bytes());
        bytes.extend_from_slice(SEPARATOR);
        return bytes;
    }

    let prefix_limit = maximum - DEBUG_LOG_TRUNCATION_MARKER.len() - SEPARATOR.len();
    let mut prefix_end = prefix_limit;
    while !entry.is_char_boundary(prefix_end) {
        prefix_end -= 1;
    }
    let mut bytes = Vec::with_capacity(maximum);
    bytes.extend_from_slice(&entry.as_bytes()[..prefix_end]);
    bytes.resize(prefix_limit, b' ');
    bytes.extend_from_slice(DEBUG_LOG_TRUNCATION_MARKER.as_bytes());
    bytes.extend_from_slice(SEPARATOR);
    debug_assert_eq!(bytes.len(), maximum);
    bytes
}

fn debug_log_backup(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    name.into()
}

fn clear_debug_logs(path: &Path) -> Result<()> {
    for candidate in [path.to_path_buf(), debug_log_backup(path)] {
        private_file::remove(&candidate)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream};
    use std::process::{Child, ExitStatus, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    use super::*;

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);
    const COPY_LAST_TEST_TRANSCRIPT: &str = "<b>Troy & Abed</b> — cool, cool cool cool.";

    fn test_directory(name: &str) -> PathBuf {
        loop {
            let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("milevox-{name}-{}-{id}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return path,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("failed to create {}: {error}", path.display()),
            }
        }
    }

    fn daemon_for_model_status(
        name: &str,
        model_status: transcription::ModelStatus,
    ) -> (Daemon, PathBuf) {
        let directory = test_directory(name);
        let config_path = directory.join("config.toml");
        let credentials_path = directory.join("credentials.toml");
        let config = Config::default();
        config.save(&config_path).unwrap();
        let credentials = Credentials::default();
        let settings = settings_snapshot(&config, &credentials);
        let initial_event = match &model_status {
            transcription::ModelStatus::Loading => StateEvent::new(State::Loading),
            transcription::ModelStatus::Ready => StateEvent::new(State::Idle),
            transcription::ModelStatus::Unavailable(error) => {
                model_unavailable_event(error.clone())
            }
        }
        .with_settings(settings.clone());
        let (events, _) = watch::channel(initial_event);
        let (levels, _) = watch::channel(0.0);
        let (inbox, _receiver) = mpsc::channel(16);

        let daemon = Daemon {
            transcriber: transcription::ParakeetTranscriber::new(&config.transcription),
            post_processor: post_processing::PostProcessor::new().unwrap(),
            debug_log: DebugLog::new(),
            model_status,
            config,
            config_path,
            credentials,
            credentials_path,
            recording: None,
            pipeline: None,
            canceling_generation: None,
            generation: 0,
            last_preview: None,
            last_debug_entry: LastDebugEntry::default(),
            level: 0.0,
            events,
            levels,
            settings,
            inbox,
            completion_waiters: Vec::new(),
            latest_transcript: None,
        };
        (daemon, directory)
    }

    async fn actor_command(
        inbox: &mpsc::Sender<ActorMessage>,
        command: Command,
    ) -> oneshot::Receiver<StateEvent> {
        let (reply, response) = oneshot::channel();
        inbox
            .send(ActorMessage::Command { command, reply })
            .await
            .unwrap();
        response
    }

    async fn read_json_line(stream: &mut UnixStream) -> serde_json::Value {
        let mut line = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let byte = stream.read_u8().await.unwrap();
                if byte == b'\n' {
                    break;
                }
                line.push(byte);
            }
        })
        .await
        .expect("test client did not receive a complete event");
        serde_json::from_slice(&line).unwrap()
    }

    async fn wait_for_model_ready(transcriber: &transcription::ParakeetTranscriber) {
        let mut status = transcriber.subscribe_status();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if *status.borrow() == transcription::ModelStatus::Ready {
                    return;
                }
                status.changed().await.unwrap();
            }
        })
        .await
        .expect("fake transcription worker did not become ready");
    }

    async fn wait_for_test_path(path: &Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("test barrier was not created at {}", path.display()));
    }

    fn built_milevox_binary() -> PathBuf {
        let test_binary = std::env::current_exe().unwrap();
        test_binary
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .join(format!("milevox{}", std::env::consts::EXE_SUFFIX))
    }

    async fn run_cli_with_response(
        name: &str,
        arguments: &[&str],
        expected_command: serde_json::Value,
        response: StateEvent,
    ) -> std::process::Output {
        let directory = test_directory(name);
        let runtime_directory = directory.join("milevox");
        std::fs::create_dir(&runtime_directory).unwrap();
        let listener = UnixListener::bind(runtime_directory.join("milevox.sock")).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert_eq!(read_json_line(&mut stream).await, expected_command);
            let mut encoded = serde_json::to_vec(&response).unwrap();
            encoded.push(b'\n');
            stream.write_all(&encoded).await.unwrap();
        });
        let binary = built_milevox_binary();
        let owned_arguments = arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect::<Vec<_>>();
        let child_runtime = directory.clone();
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new(binary)
                .args(owned_arguments)
                .env("XDG_RUNTIME_DIR", child_runtime)
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("test CLI did not connect to the Unix socket")
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        output
    }

    async fn respond_to_test_clients(mut receiver: mpsc::Receiver<ActorMessage>) {
        while let Some(message) = receiver.recv().await {
            if let ActorMessage::Command { reply, .. } = message {
                let _ = reply.send(StateEvent::new(State::Idle));
            }
        }
    }

    async fn request_test_status(path: &Path) -> String {
        let mut client = UnixStream::connect(path).await.unwrap();
        client
            .write_all(b"{\"command\":\"status\",\"follow\":false}\n")
            .await
            .unwrap();
        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_string(&mut response))
            .await
            .unwrap()
            .unwrap();
        response
    }

    fn request_test_status_sync(path: &Path) -> std::io::Result<String> {
        let mut client = StdUnixStream::connect(path)?;
        client.set_read_timeout(Some(Duration::from_millis(500)))?;
        client.write_all(b"{\"command\":\"status\",\"follow\":false}\n")?;
        let mut response = String::new();
        client.read_to_string(&mut response)?;
        Ok(response)
    }

    fn spawn_test_subprocess(test: &str, environment: (&str, &Path)) -> Child {
        std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(test)
            .arg("--nocapture")
            .env(environment.0, environment.1)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn wait_for_child(child: &mut Child, timeout: Duration) -> ExitStatus {
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            if started.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait();
                panic!("test subprocess did not exit within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn startup_lock_is_exclusive_and_released_on_drop() {
        let directory = test_directory("startup-lock");
        let path = directory.join("milevox.lock");
        let first = StartupLock::acquire(&path).unwrap();

        let error = StartupLock::acquire(&path).unwrap_err();
        assert!(error.to_string().contains("another Milevox daemon"));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        drop(first);
        let second = StartupLock::acquire(&path).unwrap();
        drop(second);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn startup_lock_rejects_a_final_component_symlink_without_chmod() {
        use std::os::unix::fs::symlink;

        let directory = test_directory("startup-lock-symlink");
        let target = directory.join("unrelated");
        let lock_path = directory.join("milevox.lock");
        std::fs::write(&target, "do not change").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&target, &lock_path).unwrap();

        assert!(StartupLock::acquire(&lock_path).is_err());
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "do not change");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn stale_socket_is_removed_while_startup_lock_is_held() {
        let directory = test_directory("stale-socket");
        let lock = StartupLock::acquire(&directory.join("milevox.lock")).unwrap();
        let socket = directory.join("milevox.sock");
        let listener = StdUnixListener::bind(&socket).unwrap();
        drop(listener);

        crate::ipc::ensure_socket_available_at(&socket)
            .await
            .unwrap();

        assert!(!socket.exists());
        drop(lock);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn serve_until_accepts_several_clients_then_shuts_down() {
        let directory = test_directory("serve-until");
        let socket = directory.join("milevox.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (inbox, receiver) = mpsc::channel(8);
        let actor = tokio::spawn(respond_to_test_clients(receiver));
        let (events, _) = watch::channel(StateEvent::new(State::Idle));
        let (levels, _) = watch::channel(0.0);
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let server = tokio::spawn(serve_until(listener, inbox, events, levels, async move {
            shutdown_receiver
                .await
                .context("test shutdown sender was dropped")?;
            Ok(())
        }));

        for _ in 0..4 {
            let response = request_test_status(&socket).await;
            assert!(response.contains("\"state\":\"idle\""));
        }
        shutdown_sender.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        actor.abort();
        std::fs::remove_file(&socket).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[tokio::test]
    async fn parked_stop_fails_when_another_client_cancels() {
        let (mut daemon, directory) =
            daemon_for_model_status("parked-stop-cancel", transcription::ModelStatus::Ready);
        daemon.pipeline = Some(Pipeline {
            generation: 7,
            output_target: Some("0xdecaf".to_owned()),
            task: tokio::spawn(async {}),
        });
        daemon.events.send_replace(
            StateEvent::new(State::Transcribing).with_settings(daemon.settings.clone()),
        );
        let events = daemon.events.subscribe();
        let (inbox, receiver) = mpsc::channel(16);
        daemon.inbox = inbox.clone();
        let actor = tokio::spawn(daemon.run(receiver));

        let start = actor_command(
            &inbox,
            Command::Start {
                output_target: Some("0xcafe".to_owned()),
            },
        )
        .await
        .await
        .unwrap();
        assert_eq!(start.state, State::Transcribing);
        assert!(
            start
                .notices
                .iter()
                .any(|notice| notice.code == "recording_busy")
        );
        assert!(!events.has_changed().unwrap());

        let mut stopped = actor_command(&inbox, Command::Stop).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut stopped)
                .await
                .is_err(),
            "stop returned before the active pipeline reached a terminal outcome"
        );

        let canceled = actor_command(&inbox, Command::Cancel).await.await.unwrap();
        assert_eq!(canceled.state, State::Idle);
        assert!(canceled.notices.iter().any(
            |notice| notice.code == "canceled" && notice.level == crate::ipc::NoticeLevel::Info
        ));
        assert!(
            canceled
                .notices
                .iter()
                .all(|notice| notice.level != crate::ipc::NoticeLevel::Error)
        );

        let stopped = stopped.await.unwrap();
        assert_eq!(stopped.state, State::Idle);
        assert!(stopped.notices.iter().any(|notice| {
            notice.code == "dictation_canceled" && notice.level == crate::ipc::NoticeLevel::Error
        }));

        let (reply, response) = oneshot::channel();
        inbox.send(ActorMessage::Shutdown { reply }).await.unwrap();
        response.await.unwrap();
        actor.await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn automatic_duration_pipeline_completion_resolves_a_parked_stop() {
        let (mut daemon, directory) = daemon_for_model_status(
            "automatic-duration-waiter",
            transcription::ModelStatus::Ready,
        );
        let request_started = directory.join("duration-request-started");
        let transcriber = transcription::supervised_fake_for_daemon([format!(
            "block_after:{}",
            request_started.display()
        )]);
        wait_for_model_ready(&transcriber).await;
        daemon.transcriber = transcriber.clone();
        daemon.recording = Some(ActiveRecording {
            generation: 11,
            output_target: None,
            recording: Recording::test_capture(
                vec![0.25; 160],
                Some(CaptureIssue::DurationLimitReached),
            ),
            preview_task: tokio::spawn(std::future::pending()),
            level_task: tokio::spawn(std::future::pending()),
        });
        daemon
            .events
            .send_replace(StateEvent::new(State::Recording).with_settings(daemon.settings.clone()));
        let (inbox, receiver) = mpsc::channel(16);
        daemon.inbox = inbox.clone();
        let actor = tokio::spawn(daemon.run(receiver));

        inbox
            .send(ActorMessage::CaptureEnded { generation: 11 })
            .await
            .unwrap();
        wait_for_test_path(&request_started).await;
        let status = actor_command(
            &inbox,
            Command::Status {
                follow: false,
                levels: false,
            },
        )
        .await
        .await
        .unwrap();
        assert_eq!(status.state, State::Transcribing);

        let mut stopped = actor_command(&inbox, Command::Stop).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut stopped)
                .await
                .is_err()
        );
        inbox
            .send(ActorMessage::Completed {
                generation: 11,
                output_target: None,
                completion: PipelineCompletion {
                    outcome: TerminalOutcome::Delivered {
                        transcript: "The Greendale Human Being".to_owned(),
                        delivery: DeliveryMethod::Clipboard,
                        notices: vec![Notice::warning(
                            "capture_duration_limit",
                            "The first 10 minutes were submitted automatically",
                        )],
                    },
                    debug_entry: "automatic duration test".to_owned(),
                },
            })
            .await
            .unwrap();

        let stopped = stopped.await.unwrap();
        assert_eq!(stopped.state, State::Idle);
        assert_eq!(
            stopped.transcript.as_deref(),
            Some("The Greendale Human Being")
        );
        assert_eq!(stopped.delivery, Some(DeliveryMethod::Clipboard));
        assert!(
            stopped
                .notices
                .iter()
                .any(|notice| notice.code == "capture_duration_limit")
        );

        let (reply, response) = oneshot::channel();
        inbox.send(ActorMessage::Shutdown { reply }).await.unwrap();
        response.await.unwrap();
        actor.await.unwrap();
        assert!(transcriber.cancel(11).await);
        transcriber.wait_until_available().await;
        drop(transcriber);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn active_cancellation_rejects_start_and_ignores_stale_completion() {
        let (mut daemon, directory) = daemon_for_model_status(
            "active-canceling-generation",
            transcription::ModelStatus::Ready,
        );
        let request_started = directory.join("request-started");
        let restart_released = directory.join("restart-released");
        let transcriber = transcription::supervised_fake_for_daemon([
            format!("block_after:{}", request_started.display()),
            format!("normal_after:{}", restart_released.display()),
        ]);
        wait_for_model_ready(&transcriber).await;
        daemon.transcriber = transcriber.clone();
        let active_transcriber = transcriber.clone();
        let pipeline_task = tokio::spawn(async move {
            let _ = active_transcriber
                .transcribe(
                    19,
                    CapturedAudio {
                        samples: vec![0.25; 160],
                        sample_rate: 16_000,
                    },
                )
                .await;
        });
        wait_for_test_path(&request_started).await;
        daemon.pipeline = Some(Pipeline {
            generation: 19,
            output_target: Some("0xdecaf".to_owned()),
            task: pipeline_task,
        });
        daemon.events.send_replace(
            StateEvent::new(State::Transcribing).with_settings(daemon.settings.clone()),
        );
        let mut events = daemon.events.subscribe();
        let (inbox, receiver) = mpsc::channel(16);
        daemon.inbox = inbox.clone();
        let actor = tokio::spawn(daemon.run(receiver));

        let canceled = actor_command(&inbox, Command::Cancel).await.await.unwrap();
        assert_eq!(canceled.state, State::Canceling);
        assert_eq!(events.borrow_and_update().state, State::Canceling);
        assert!(!restart_released.exists());

        let start = actor_command(
            &inbox,
            Command::Start {
                output_target: None,
            },
        )
        .await
        .await
        .unwrap();
        assert_eq!(start.state, State::Canceling);
        assert!(
            start
                .notices
                .iter()
                .any(|notice| notice.code == "recording_busy")
        );

        inbox
            .send(ActorMessage::Completed {
                generation: 19,
                output_target: Some("0xdecaf".to_owned()),
                completion: PipelineCompletion {
                    outcome: TerminalOutcome::Delivered {
                        transcript: "stale Greendale transcript".to_owned(),
                        delivery: DeliveryMethod::Clipboard,
                        notices: Vec::new(),
                    },
                    debug_entry: "stale completion".to_owned(),
                },
            })
            .await
            .unwrap();
        let status = actor_command(
            &inbox,
            Command::Status {
                follow: false,
                levels: false,
            },
        )
        .await
        .await
        .unwrap();
        assert_eq!(status.state, State::Canceling);
        assert!(status.transcript.is_none());

        std::fs::write(&restart_released, []).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                events.changed().await.unwrap();
                if events.borrow_and_update().state == State::Idle {
                    return;
                }
            }
        })
        .await
        .expect("canceling state did not resolve after the worker restarted");
        let status = actor_command(
            &inbox,
            Command::Status {
                follow: false,
                levels: false,
            },
        )
        .await
        .await
        .unwrap();
        assert_eq!(status.state, State::Idle);
        assert!(status.transcript.is_none());

        let (reply, response) = oneshot::channel();
        inbox.send(ActorMessage::Shutdown { reply }).await.unwrap();
        response.await.unwrap();
        actor.await.unwrap();
        drop(transcriber);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn followers_separate_levels_and_send_the_latest_settings_snapshot() {
        let config = Config::default();
        let credentials = Credentials::default();
        let initial_settings = settings_snapshot(&config, &credentials);
        let (events, _) =
            watch::channel(StateEvent::new(State::Idle).with_settings(initial_settings.clone()));
        let (levels, _) = watch::channel(0.0);
        let (inbox, _receiver) = mpsc::channel(4);

        let (mut ordinary, ordinary_server) = UnixStream::pair().unwrap();
        let ordinary_task = tokio::spawn(handle_client(
            ordinary_server,
            inbox.clone(),
            events.subscribe(),
            levels.subscribe(),
        ));
        ordinary
            .write_all(b"{\"command\":\"status\",\"follow\":true}\n")
            .await
            .unwrap();

        let (mut metered, metered_server) = UnixStream::pair().unwrap();
        let metered_task = tokio::spawn(handle_client(
            metered_server,
            inbox.clone(),
            events.subscribe(),
            levels.subscribe(),
        ));
        metered
            .write_all(b"{\"command\":\"status\",\"follow\":true,\"levels\":true}\n")
            .await
            .unwrap();

        let ordinary_initial = read_json_line(&mut ordinary).await;
        assert_eq!(ordinary_initial["type"], "state");
        assert!(ordinary_initial.get("settings").is_some());
        let metered_initial = read_json_line(&mut metered).await;
        assert_eq!(metered_initial["type"], "state");
        let initial_level = read_json_line(&mut metered).await;
        assert_eq!(
            initial_level,
            serde_json::json!({"type": "level", "level": 0.0})
        );

        levels.send_replace(0.75);
        let meter_update = read_json_line(&mut metered).await;
        assert_eq!(meter_update["type"], "level");
        assert_eq!(meter_update["level"], 0.75);
        assert_eq!(meter_update.as_object().unwrap().len(), 2);
        assert!(serde_json::to_vec(&meter_update).unwrap().len() <= 48);

        let mut latest_config = config.clone();
        latest_config.debug.enabled = true;
        latest_config.post_processing.enabled = true;
        let latest_settings = settings_snapshot(&latest_config, &credentials);
        events.send_replace(
            StateEvent::active(State::Recording, Some("Troy and Abed".to_owned()), None)
                .with_settings(latest_settings),
        );
        let ordinary_state = read_json_line(&mut ordinary).await;
        assert_eq!(ordinary_state["type"], "state");
        assert_eq!(ordinary_state["state"], "recording");
        assert_eq!(ordinary_state["settings"]["debug"]["enabled"], true);
        let metered_state = read_json_line(&mut metered).await;
        assert_eq!(metered_state["type"], "state");
        assert_eq!(metered_state["state"], "recording");

        levels.send_replace(0.9);
        let latest_level = read_json_line(&mut metered).await;
        assert_eq!(
            latest_level,
            serde_json::json!({"type": "level", "level": 0.9})
        );

        let (mut newcomer, newcomer_server) = UnixStream::pair().unwrap();
        let newcomer_task = tokio::spawn(handle_client(
            newcomer_server,
            inbox,
            events.subscribe(),
            levels.subscribe(),
        ));
        newcomer
            .write_all(b"{\"command\":\"status\",\"follow\":true,\"levels\":true}\n")
            .await
            .unwrap();
        let newcomer_state = read_json_line(&mut newcomer).await;
        assert_eq!(newcomer_state["state"], "recording");
        assert_eq!(newcomer_state["settings"]["debug"]["enabled"], true);
        assert_eq!(
            newcomer_state["settings"]["post_processing"]["enabled"],
            true
        );
        assert_eq!(
            read_json_line(&mut newcomer).await,
            serde_json::json!({"type": "level", "level": 0.9})
        );

        ordinary_task.abort();
        metered_task.abort();
        newcomer_task.abort();
    }

    #[tokio::test]
    async fn unix_socket_cli_reports_cancel_success_and_parked_stop_failure() {
        let canceled = run_cli_with_response(
            "cli-cancel-success",
            &["record", "cancel"],
            serde_json::json!({"command": "cancel"}),
            StateEvent::new(State::Idle)
                .with_notice(Notice::info("canceled", "Dictation canceled")),
        )
        .await;
        assert!(canceled.status.success());
        assert!(canceled.stdout.is_empty());

        let stopped = run_cli_with_response(
            "cli-stop-canceled",
            &["record", "stop"],
            serde_json::json!({"command": "stop"}),
            StateEvent::error(
                State::Idle,
                "dictation_canceled",
                "Dictation was canceled before delivery finished",
            ),
        )
        .await;
        assert!(!stopped.status.success());
        assert!(
            String::from_utf8(stopped.stderr)
                .unwrap()
                .contains("Dictation was canceled before delivery finished")
        );

        let delivery_failed = run_cli_with_response(
            "cli-stop-delivery-failed",
            &["record", "stop"],
            serde_json::json!({"command": "stop"}),
            StateEvent::new(State::Error)
                .with_transcript("Troy and Abed in the morning".to_owned())
                .with_notice(
                    Notice::error(
                        "delivery_failed",
                        "The final transcript could not be delivered",
                    )
                    .with_detail("wl-copy exited with status 1"),
                ),
        )
        .await;
        assert!(!delivery_failed.status.success());
        assert_eq!(
            String::from_utf8(delivery_failed.stdout).unwrap(),
            "Troy and Abed in the morning\n"
        );
        assert!(
            String::from_utf8(delivery_failed.stderr)
                .unwrap()
                .contains("wl-copy exited with status 1")
        );

        let copied = run_cli_with_response(
            "cli-copy-recovered-transcript",
            &["record", "copy"],
            serde_json::json!({"command": "copy_last"}),
            StateEvent::new(State::Error)
                .with_notice(Notice::info("transcript_copied", "Transcript copied")),
        )
        .await;
        assert!(copied.status.success());
        assert!(copied.stdout.is_empty());
        assert!(copied.stderr.is_empty());
    }

    #[test]
    fn sigterm_server_subprocess() {
        let Some(socket) = std::env::var_os("MILEVOX_SIGTERM_TEST_SOCKET") else {
            return;
        };
        let socket = PathBuf::from(socket);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let listener = UnixListener::bind(&socket).unwrap();
            let (inbox, receiver) = mpsc::channel(32);
            let actor = tokio::spawn(respond_to_test_clients(receiver));
            let (events, _) = watch::channel(StateEvent::new(State::Idle));
            let (levels, _) = watch::channel(0.0);

            serve(listener, &socket, inbox, events, levels)
                .await
                .unwrap();

            actor.abort();
        });
    }

    #[test]
    fn churn_client_subprocess() {
        let Some(socket) = std::env::var_os("MILEVOX_CHURN_TEST_SOCKET") else {
            return;
        };
        let socket = PathBuf::from(socket);
        for _ in 0..250 {
            let Ok(response) = request_test_status_sync(&socket) else {
                return;
            };
            assert!(response.contains("\"state\":\"idle\""));
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn sigterm_during_subprocess_client_churn_cleans_up_socket() {
        let directory = test_directory("sigterm");
        let socket = directory.join("milevox.sock");
        let mut server = spawn_test_subprocess(
            "daemon::tests::sigterm_server_subprocess",
            ("MILEVOX_SIGTERM_TEST_SOCKET", &socket),
        );

        let started = Instant::now();
        loop {
            if let Ok(response) = request_test_status_sync(&socket)
                && response.contains("\"state\":\"idle\"")
            {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }

        let mut clients = Vec::new();
        for _ in 0..4 {
            clients.push(spawn_test_subprocess(
                "daemon::tests::churn_client_subprocess",
                ("MILEVOX_CHURN_TEST_SOCKET", &socket),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));

        // SAFETY: `server.id()` is the live child created above, and SIGTERM is
        // handled by the server's shutdown future.
        assert_eq!(unsafe { libc::kill(server.id() as i32, libc::SIGTERM) }, 0);
        assert!(wait_for_child(&mut server, Duration::from_secs(5)).success());
        for client in &mut clients {
            assert!(wait_for_child(client, Duration::from_secs(5)).success());
        }

        assert!(!socket.exists());
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn startup_race_subprocess() {
        let Some(directory) = std::env::var_os("MILEVOX_STARTUP_RACE_DIR") else {
            return;
        };
        let directory = PathBuf::from(directory);
        let guard = match StartupLock::acquire(&directory.join("milevox.lock")) {
            Ok(guard) => guard,
            Err(error) if error.to_string().contains("another Milevox daemon") => return,
            Err(error) => panic!("could not acquire startup lock: {error:#}"),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let socket = directory.join("milevox.sock");
            crate::ipc::ensure_socket_available_at(&socket)
                .await
                .unwrap();
            let listener = UnixListener::bind(&socket).unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut byte = [0];
            stream.read_exact(&mut byte).await.unwrap();
            std::fs::remove_file(socket).unwrap();
        });
        drop(guard);
    }

    #[test]
    fn racing_startups_leave_one_reachable_daemon() {
        let directory = test_directory("startup-race");
        let mut first = spawn_test_subprocess(
            "daemon::tests::startup_race_subprocess",
            ("MILEVOX_STARTUP_RACE_DIR", &directory),
        );
        let mut second = spawn_test_subprocess(
            "daemon::tests::startup_race_subprocess",
            ("MILEVOX_STARTUP_RACE_DIR", &directory),
        );
        let socket = directory.join("milevox.sock");

        let started = Instant::now();
        loop {
            let exited = usize::from(first.try_wait().unwrap().is_some())
                + usize::from(second.try_wait().unwrap().is_some());
            if exited == 1 && socket.exists() {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }

        let mut client = StdUnixStream::connect(&socket).unwrap();
        client.write_all(b"x").unwrap();
        assert!(wait_for_child(&mut first, Duration::from_secs(5)).success());
        assert!(wait_for_child(&mut second, Duration::from_secs(5)).success());
        assert!(!socket.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

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
        let last_preview = Some("First sentence".to_owned());
        let final_decode = "First sentence. A final sentence.";
        let entry = debug_entry(
            1,
            &last_preview,
            final_decode,
            final_decode,
            DebugOutcome {
                provider_response: None,
                final_text: final_decode,
                delivery_result: "typed",
                warning: None,
                error: None,
            },
        );

        assert!(entry.contains("POST-PROCESSING INPUT:\nFirst sentence. A final sentence."));
        assert!(entry.contains("FINAL TEXT:\nFirst sentence. A final sentence."));
        assert!(entry.contains("DELIVERY RESULT:\ntyped"));
    }

    #[test]
    fn diagnostic_compares_every_transcription_stage() {
        let last_preview = Some("Troy and Abed in the morning".to_owned());
        let provider_response = post_processing::ProviderAttempt {
            text: "Troy and Abed in the morning.".to_owned(),
            validation_error: None,
        };
        let comparison = debug_entry(
            7,
            &last_preview,
            "Troy and a bed in the morning",
            "Troy and a bed in the morning",
            DebugOutcome {
                provider_response: Some(&provider_response),
                final_text: "Troy and Abed in the morning.",
                delivery_result: "typed",
                warning: None,
                error: None,
            },
        );

        assert_eq!(
            comparison,
            "=== RECORDING 7 ===\n\
             LAST PREVIEW:\nTroy and Abed in the morning\n\n\
             FINAL RAW:\nTroy and a bed in the morning\n\n\
             POST-PROCESSING INPUT:\nTroy and a bed in the morning\n\n\
             PROVIDER RESPONSE:\nTroy and Abed in the morning.\n\n\
             PROVIDER VALIDATION:\naccepted\n\n\
             FINAL TEXT:\nTroy and Abed in the morning.\n\n\
             DELIVERY RESULT:\ntyped\n\n\
             WARNING:\n[none]\n\n\
             ERROR:\n[none]"
        );
    }

    #[test]
    fn diagnostic_records_a_rejected_provider_response() {
        let response = post_processing::ProviderAttempt {
            text: "This is a test.\nThis is another test.".to_owned(),
            validation_error: Some(
                "output changes dictated word 5 (`new` became `this`)".to_owned(),
            ),
        };

        assert_eq!(
            format_provider_response(Some(&response)),
            "PROVIDER RESPONSE:\n\
             This is a test.\nThis is another test.\n\n\
             PROVIDER VALIDATION:\n\
             rejected: output changes dictated word 5 (`new` became `this`)"
        );
        assert_eq!(
            format_provider_response(None),
            "PROVIDER RESPONSE:\n[unavailable]\n\nPROVIDER VALIDATION:\n[unavailable]"
        );

        let unsafe_response = post_processing::ProviderAttempt {
            text: "Greendale\u{1b}]52;secret\u{7}".to_owned(),
            validation_error: Some("rejected\u{202e}".to_owned()),
        };
        let rendered = format_provider_response(Some(&unsafe_response));
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{7}'));
        assert!(!rendered.contains('\u{202e}'));
        assert!(rendered.contains("\\u{001b}]52;secret\\u{0007}"));
        assert!(rendered.contains("rejected\\u{202e}"));
    }

    #[test]
    fn stale_pipeline_identity_cannot_replace_the_active_target() {
        assert!(pipeline_identity_matches(
            7,
            Some("0xdecaf"),
            7,
            Some("0xdecaf")
        ));
        assert!(!pipeline_identity_matches(
            8,
            Some("0xdecaf"),
            7,
            Some("0xdecaf")
        ));
        assert!(!pipeline_identity_matches(
            7,
            Some("0xdecaf"),
            7,
            Some("0xcafe")
        ));
    }

    #[test]
    fn delivery_failure_event_retains_the_recoverable_transcript() {
        let event = delivery_failure_event(
            "Cool. Cool cool cool.".into(),
            vec![
                Notice::warning("post_processing_fallback", POST_PROCESSING_FALLBACK_TEXT),
                Notice::warning("debug_write_failed", DEBUG_WRITE_FAILED_TEXT),
            ],
            "the active output target changed during dictation".into(),
        );

        assert_eq!(event.state, State::Error);
        assert_eq!(event.transcript.as_deref(), Some("Cool. Cool cool cool."));
        let errors = event
            .notices
            .iter()
            .filter(|notice| notice.level == crate::ipc::NoticeLevel::Error)
            .collect::<Vec<_>>();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, "delivery_failed");
        assert!(
            errors[0]
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("target changed"))
        );
        assert!(
            event
                .notices
                .iter()
                .filter(|notice| notice.code != "delivery_failed")
                .all(|notice| !notice.text.to_lowercase().contains("delivered"))
        );
    }

    #[test]
    fn pipeline_delivery_results_become_typed_terminal_outcomes() {
        let transcript = "Troy and Abed in the morning";
        let delivery_result = output::DeliveryResult {
            method: DeliveryMethod::Typed,
            notices: vec![Notice::info("delivery_notice", "Delivery succeeded")],
        };
        let notices = pipeline_notices(
            None,
            Some(&CaptureIssue::DurationLimitReached),
            Some(&delivery_result),
        );
        let delivered = terminal_outcome(transcript.to_owned(), Ok(delivery_result), notices);
        let TerminalOutcome::Delivered {
            transcript: delivered_transcript,
            delivery,
            notices,
        } = delivered
        else {
            panic!("successful delivery did not produce a delivered outcome");
        };
        assert_eq!(delivered_transcript, transcript);
        assert_eq!(delivery, DeliveryMethod::Typed);
        assert!(
            notices
                .iter()
                .any(|notice| notice.code == "capture_duration_limit")
        );
        assert!(
            notices
                .iter()
                .any(|notice| notice.code == "delivery_notice")
        );

        for error in [
            "wtype failed",
            "wl-copy failed",
            "typing failed; clipboard fallback also failed",
        ] {
            let outcome = terminal_outcome(
                transcript.to_owned(),
                Err(anyhow::anyhow!(error)),
                Vec::new(),
            );
            let TerminalOutcome::DeliveryFailed {
                transcript: retained,
                error: retained_error,
                ..
            } = outcome
            else {
                panic!("{error} did not produce a recoverable delivery failure");
            };
            assert_eq!(retained, transcript);
            assert_eq!(retained_error, error);
        }
    }

    #[test]
    fn pipeline_notices_propagate_capture_and_delivery_warnings() {
        let delivery = output::DeliveryResult {
            method: DeliveryMethod::ClipboardFallback,
            notices: vec![Notice::warning(
                "clipboard_fallback",
                "Typing failed; transcript copied to the clipboard",
            )],
        };

        let notices = pipeline_notices(
            Some("provider returned an invalid response"),
            Some(&CaptureIssue::Device(
                "USB microphone disconnected".to_owned(),
            )),
            Some(&delivery),
        );

        assert_eq!(notices.len(), 3);
        assert!(notices.iter().any(|notice| {
            notice.code == "post_processing_fallback"
                && notice.detail.as_deref() == Some("provider returned an invalid response")
        }));
        assert!(notices.iter().any(|notice| {
            notice.code == "capture_device_warning"
                && notice.detail.as_deref()
                    == Some("microphone device failed: USB microphone disconnected")
        }));
        assert!(
            notices
                .iter()
                .any(|notice| notice.code == "clipboard_fallback")
        );
    }

    #[test]
    fn provider_failure_detail_reaches_the_generated_diagnostic() {
        let notices = pipeline_notices(
            Some("OpenRouter returned HTTP 503: Greendale is unavailable"),
            None,
            None,
        );
        let warning = diagnostic_warning(&notices);

        let entry = debug_entry(
            9,
            &None,
            "Troy and Abed in the morning",
            "Troy and Abed in the morning",
            DebugOutcome {
                provider_response: None,
                final_text: "Troy and Abed in the morning",
                delivery_result: "clipboard",
                warning: warning.as_deref(),
                error: None,
            },
        );

        assert!(entry.contains(
            "WARNING:\nPost-processing failed; using the original transcript: \
             OpenRouter returned HTTP 503: Greendale is unavailable"
        ));
    }

    #[tokio::test]
    async fn copy_last_subprocess() {
        if std::env::var_os("MILEVOX_COPY_LAST_TEST_RUN").is_none() {
            return;
        }
        let (mut daemon, directory) =
            daemon_for_model_status("copy-last", transcription::ModelStatus::Ready);
        daemon.latest_transcript = Some(COPY_LAST_TEST_TRANSCRIPT.to_owned());
        daemon.publish(delivery_failure_event(
            COPY_LAST_TEST_TRANSCRIPT.to_owned(),
            Vec::new(),
            "the original delivery failed".to_owned(),
        ));

        let event = daemon.handle_command(Command::CopyLast).await;

        assert_eq!(event.state, State::Error);
        assert!(event.transcript.is_none());
        assert_eq!(event.notices.len(), 1);
        assert!(event.notices.iter().any(|notice| {
            notice.code == "transcript_copied" && notice.level == crate::ipc::NoticeLevel::Info
        }));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn copy_last_command_copies_the_exact_recoverable_transcript() {
        let directory = test_directory("copy-last-helper");
        let copied = directory.join("copied");
        let helper = directory.join("wl-copy");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\n# Fake clipboard helper for the CopyLast test.\n/bin/cat > '{}'\n",
                copied.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = std::env::join_paths(std::iter::once(directory.clone()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("daemon::tests::copy_last_subprocess")
            .arg("--nocapture")
            .env("MILEVOX_COPY_LAST_TEST_RUN", "1")
            .env("PATH", path)
            .status()
            .unwrap();

        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(&copied).unwrap(),
            COPY_LAST_TEST_TRANSCRIPT
        );
        std::fs::remove_dir_all(directory).unwrap();
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
        let last_preview = Some("Dean-a-ling".to_owned());
        let mut last = LastDebugEntry::default();
        last.remember("previous transcription".to_owned());

        last.remember(failed_debug_entry(
            9,
            &last_preview,
            "Parakeet transcription failed",
        ));

        let entry = last.event(State::Error).debug_entry.unwrap();
        assert!(entry.contains("LAST PREVIEW:\nDean-a-ling"));
        assert!(entry.contains("ERROR:\nParakeet transcription failed"));
        assert!(!entry.contains("previous transcription"));
    }

    #[test]
    fn preview_updates_move_one_owned_transcript_into_the_actor_message() {
        let transcript = "Troy and Abed in the morning".to_owned();
        let allocation = transcript.as_ptr();
        let message = ActorMessage::Preview {
            generation: 7,
            transcript,
        };

        let ActorMessage::Preview { transcript, .. } = message else {
            unreachable!();
        };
        assert_eq!(transcript.as_ptr(), allocation);
    }

    #[test]
    fn preview_windows_use_one_second_steps_and_half_second_overlap() {
        for rate in [16_000, 48_000, 96_000, 192_000] {
            let rate = rate as usize;
            assert_eq!(preview_window(0, rate - 1, rate as u32), None);
            assert_eq!(preview_window(0, rate, rate as u32), Some(0..rate));
            assert_eq!(
                preview_window(rate, rate * 2, rate as u32),
                Some(rate / 2..rate * 2)
            );
            assert_eq!(
                preview_window(rate * 2, rate * 2 + rate / 2, rate as u32),
                None
            );
        }
    }

    #[test]
    fn sixty_seconds_of_preview_input_stays_below_ninety_seconds() {
        for rate in [16_000_u32, 48_000, 96_000, 192_000] {
            let mut last = 0;
            let mut total = 0;
            for second in 1..=60 {
                let current = second * rate as usize;
                let window = preview_window(last, current, rate).unwrap();
                total += window.len();
                last = window.end;
            }

            assert_eq!(total, rate as usize * 179 / 2);
            assert!(total <= rate as usize * 90);
        }
    }

    #[test]
    fn ten_minutes_of_preview_input_stays_below_fifteen_minutes() {
        const TEN_MINUTES: usize = 10 * 60;

        for rate in [16_000_u32, 48_000, 96_000, 192_000] {
            let mut last = 0;
            let mut total = 0;
            for second in 1..=TEN_MINUTES {
                let current = second * rate as usize;
                let window = preview_window(last, current, rate).unwrap();
                total += window.len();
                last = window.end;
            }

            assert_eq!(total, rate as usize * 1_799 / 2);
            assert!(total <= rate as usize * 15 * 60);
        }
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
        let directory = test_directory("debug-log-rotation");
        let path = directory.join("debug.log");
        let previous_size = DEBUG_LOG_MAX_BYTES as usize - 8;
        std::fs::write(&path, vec![b'x'; previous_size]).unwrap();

        append_debug_log(&path, "next recording").unwrap();

        let backup = debug_log_backup(&path);
        assert_eq!(
            std::fs::metadata(&backup).unwrap().len(),
            previous_size as u64
        );
        assert!(std::fs::metadata(&path).unwrap().len() <= DEBUG_LOG_MAX_BYTES);
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

    #[test]
    fn debug_log_caps_legacy_live_and_backup_files_before_rotation() {
        let directory = test_directory("debug-log-legacy-oversize");
        let path = directory.join("debug.log");
        let backup = debug_log_backup(&path);
        std::fs::write(&path, vec![b'x'; DEBUG_LOG_MAX_BYTES as usize + 256]).unwrap();
        std::fs::write(&backup, vec![b'y'; DEBUG_LOG_MAX_BYTES as usize + 512]).unwrap();

        append_debug_log(&path, "next recording").unwrap();

        assert_eq!(
            std::fs::metadata(&backup).unwrap().len(),
            DEBUG_LOG_MAX_BYTES
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "next recording\n\n"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn debug_log_caps_an_oversized_backup_even_without_rotation() {
        let directory = test_directory("debug-log-backup-oversize");
        let path = directory.join("debug.log");
        let backup = debug_log_backup(&path);
        std::fs::write(&path, "previous\n\n").unwrap();
        std::fs::write(&backup, vec![b'y'; DEBUG_LOG_MAX_BYTES as usize + 512]).unwrap();

        append_debug_log(&path, "next").unwrap();

        assert_eq!(
            std::fs::metadata(&backup).unwrap().len(),
            DEBUG_LOG_MAX_BYTES
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "previous\n\nnext\n\n"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn debug_clear_is_ordered_after_every_submitted_append() {
        let directory = test_directory("debug-log-ordered-clear");
        let path = directory.join("debug.log");
        let debug_log = DebugLog::new();
        let append = debug_log
            .submit_append(path.clone(), "Greendale transcript".to_owned())
            .unwrap();

        debug_log.clear(path.clone()).await.unwrap();
        append.await.unwrap().unwrap();

        assert!(!path.exists());
        assert!(!debug_log_backup(&path).exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn cancel_waits_until_an_aborted_pipeline_cannot_submit_a_late_debug_append() {
        let (mut daemon, directory) =
            daemon_for_model_status("debug-log-cancel-clear", transcription::ModelStatus::Ready);
        let path = directory.join("debug.log");
        let debug_log = daemon.debug_log.clone();
        let late_path = path.clone();
        let (release, wait) = oneshot::channel();
        daemon.pipeline = Some(Pipeline {
            generation: 31,
            output_target: None,
            task: tokio::spawn(async move {
                if wait.await.is_ok() {
                    let _ = debug_log
                        .persist_at(late_path, "late Greendale transcript")
                        .await;
                }
            }),
        });
        daemon.events.send_replace(
            StateEvent::new(State::Transcribing).with_settings(daemon.settings.clone()),
        );

        daemon.cancel().await;
        assert!(release.send(()).is_err());
        daemon.canceling_generation = None;
        daemon.clear_debug_at(&path).await;

        assert!(!path.exists());
        assert!(!debug_log_backup(&path).exists());
        drop(daemon);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn oversized_multibyte_debug_entry_is_valid_utf8_at_the_exact_limit() {
        let directory = test_directory("debug-log-truncation");
        let path = directory.join("debug.log");
        let entry = "é".repeat(DEBUG_LOG_MAX_BYTES as usize / 2 + 100);

        append_debug_log(&path, &entry).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), DEBUG_LOG_MAX_BYTES as usize);
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.ends_with(&format!("{DEBUG_LOG_TRUNCATION_MARKER}\n\n")));
        assert!(!debug_log_backup(&path).exists());
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

    #[tokio::test]
    async fn successful_settings_save_updates_the_in_memory_snapshot() {
        let (mut daemon, directory) =
            daemon_for_model_status("settings-snapshot", transcription::ModelStatus::Ready);

        let event = daemon.update_settings(Some(true), None, None);

        assert!(
            Config::load(&daemon.config_path)
                .unwrap()
                .post_processing
                .enabled
        );
        assert!(daemon.config.post_processing.enabled);
        assert!(daemon.settings.post_processing.enabled);
        assert!(event.settings.unwrap().post_processing.enabled);
        drop(daemon);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn settings_models_reuses_the_cached_snapshot() {
        let (mut daemon, directory) =
            daemon_for_model_status("cached-model-settings", transcription::ModelStatus::Ready);
        daemon.settings.debug.enabled = true;

        let event = daemon
            .handle_command(Command::SettingsModels {
                provider: Some(PostProcessingProvider::OpencodeZen),
                json: true,
            })
            .await;

        assert!(event.settings.unwrap().debug.enabled);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn start_is_rejected_while_the_model_is_loading() {
        let (mut daemon, directory) =
            daemon_for_model_status("start-loading", transcription::ModelStatus::Loading);

        let event = daemon
            .handle_command(Command::Start {
                output_target: Some("0xdecaf".to_owned()),
            })
            .await;

        assert_eq!(event.state, State::Loading);
        assert!(
            event
                .notices
                .iter()
                .any(|notice| notice.code == "recording_busy")
        );
        assert!(daemon.recording.is_none());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn cancel_and_configuration_mutations_never_publish_idle_before_readiness() {
        let cases = [
            (
                "mutations-loading",
                transcription::ModelStatus::Loading,
                State::Loading,
            ),
            (
                "mutations-unavailable",
                transcription::ModelStatus::Unavailable(
                    "cached Greendale model failure".to_owned(),
                ),
                State::Error,
            ),
        ];

        for (name, model_status, want_state) in cases {
            let (mut daemon, directory) = daemon_for_model_status(name, model_status);
            let debug_path = directory.join("debug.log");
            let events = [
                daemon.cancel().await,
                daemon.update_settings(Some(true), None, None),
                daemon.update_token(
                    Some(PostProcessingProvider::Openrouter),
                    "greendale-openrouter-token".to_owned(),
                ),
                daemon.remove_token(Some(PostProcessingProvider::Openrouter)),
                daemon.update_debug(true),
                daemon.clear_debug_at(&debug_path).await,
            ];

            for event in events {
                assert_eq!(event.state, want_state, "{name}");
                assert_ne!(event.state, State::Idle, "{name}");
                if want_state == State::Error {
                    assert!(event.notices.iter().any(|notice| {
                        notice.code == "model_unavailable"
                            && notice.detail.as_deref() == Some("cached Greendale model failure")
                    }));
                }
            }
            assert_eq!(daemon.current_state(), want_state);
            drop(daemon);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[tokio::test]
    async fn configuration_mutations_preserve_canceling_and_clear_terminal_errors_when_ready() {
        let (mut canceling, canceling_directory) =
            daemon_for_model_status("mutations-canceling", transcription::ModelStatus::Ready);
        canceling.canceling_generation = Some(17);
        canceling.events.send_replace(
            StateEvent::new(State::Canceling).with_settings(canceling.settings.clone()),
        );
        let debug_path = canceling_directory.join("debug.log");
        let events = [
            canceling.update_settings(Some(true), None, None),
            canceling.update_token(
                Some(PostProcessingProvider::Openrouter),
                "greendale-openrouter-token".to_owned(),
            ),
            canceling.remove_token(Some(PostProcessingProvider::Openrouter)),
            canceling.update_debug(true),
            canceling.clear_debug_at(&debug_path).await,
        ];
        for event in events {
            assert_eq!(event.state, State::Canceling);
            assert!(event.notices.iter().any(|notice| {
                matches!(
                    notice.code.as_str(),
                    "settings_busy" | "token_busy" | "debug_busy"
                )
            }));
        }
        assert_eq!(canceling.current_state(), State::Canceling);
        drop(canceling);
        std::fs::remove_dir_all(canceling_directory).unwrap();

        let (mut ready, ready_directory) =
            daemon_for_model_status("mutations-ready-error", transcription::ModelStatus::Ready);
        ready.events.send_replace(
            StateEvent::error(State::Error, "pipeline_failed", "previous failure")
                .with_settings(ready.settings.clone()),
        );

        let event = ready.update_debug(true);

        assert_eq!(event.state, State::Idle);
        assert!(event.notices.is_empty());
        assert!(event.message.is_none());
        drop(ready);
        std::fs::remove_dir_all(ready_directory).unwrap();
    }

    #[tokio::test]
    async fn completion_uses_model_status_suppressed_while_pipeline_was_active() {
        let cases = [
            (
                "completion-ready",
                transcription::ModelStatus::Ready,
                State::Idle,
            ),
            (
                "completion-unavailable",
                transcription::ModelStatus::Unavailable(
                    "worker exited after final inference".to_owned(),
                ),
                State::Error,
            ),
        ];

        for (name, final_status, want_state) in cases {
            let (mut daemon, directory) =
                daemon_for_model_status(name, transcription::ModelStatus::Ready);
            daemon.pipeline = Some(Pipeline {
                generation: 23,
                output_target: Some("0xdecaf".to_owned()),
                task: tokio::spawn(std::future::pending()),
            });
            daemon.events.send_replace(
                StateEvent::new(State::Transcribing).with_settings(daemon.settings.clone()),
            );

            daemon.update_model_status(transcription::ModelStatus::Loading);
            assert_eq!(daemon.current_state(), State::Transcribing);
            daemon.pipeline.take().unwrap().task.abort();
            let event = daemon.delivered_event(
                "Troy and Abed in the morning".to_owned(),
                DeliveryMethod::Clipboard,
                vec![Notice::warning(
                    "post_processing_fallback",
                    POST_PROCESSING_FALLBACK_TEXT,
                )],
            );
            assert_eq!(event.state, State::Loading);
            daemon.publish(event);
            daemon.update_model_status(final_status);
            let event = daemon.current_event();

            assert_eq!(event.state, want_state);
            assert_eq!(
                event.transcript.as_deref(),
                Some("Troy and Abed in the morning")
            );
            assert_eq!(event.delivery, Some(DeliveryMethod::Clipboard));
            assert!(
                event
                    .notices
                    .iter()
                    .any(|notice| notice.code == "post_processing_fallback")
            );
            if want_state == State::Error {
                assert!(event.notices.iter().any(|notice| {
                    notice.code == "model_unavailable"
                        && notice.detail.as_deref() == Some("worker exited after final inference")
                }));
            }
            drop(daemon);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }
}
