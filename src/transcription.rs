use std::collections::VecDeque;
use std::env;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use parakeet_rs::{ParakeetTDT, Transcriber as ParakeetTranscriberTrait};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, watch};

use crate::audio::CapturedAudio;
use crate::config::TranscriptionConfig;
use crate::paths;

const PARAKEET_SAMPLE_RATE: u32 = 16_000;
const RESAMPLER_CHUNK_SIZE: usize = 1024;
const MAX_METADATA_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_SAMPLE_BYTES: usize = 64 * 1024 * 1024;
const MODEL_LOAD_DEADLINE: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelStatus {
    Loading,
    Ready,
    Unavailable(String),
}

#[derive(Clone)]
pub struct ParakeetTranscriber {
    commands: mpsc::Sender<SupervisorCommand>,
    status: watch::Receiver<ModelStatus>,
}

enum SupervisorCommand {
    Request(TranscriptionRequest),
    Cancel {
        generation: u64,
        reply: oneshot::Sender<bool>,
    },
}

#[derive(Default)]
struct RequestQueue {
    finals: VecDeque<TranscriptionRequest>,
    preview: Option<TranscriptionRequest>,
    canceled_generation: u64,
}

struct TranscriptionRequest {
    generation: u64,
    audio: CapturedAudio,
    allow_empty: bool,
    reply: oneshot::Sender<std::result::Result<Option<String>, String>>,
}

struct WorkerSpec {
    executable: PathBuf,
    model_path: PathBuf,
    fake_behaviors: VecDeque<String>,
    deadline_override: Option<Duration>,
}

struct WorkerProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

#[derive(Debug, Deserialize, Serialize)]
struct WorkerRequest {
    request_id: u64,
    generation: u64,
    sample_rate: u32,
    sample_count: usize,
    allow_empty: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkerMessage {
    Ready,
    LoadError {
        error: String,
    },
    Result {
        request_id: u64,
        transcript: Option<String>,
        error: Option<String>,
    },
}

impl ParakeetTranscriber {
    pub fn new(config: &TranscriptionConfig) -> Self {
        let model_path = config
            .model_path
            .clone()
            .unwrap_or_else(paths::default_model_path);
        let (commands, receiver) = mpsc::channel(32);
        let (status_sender, status) = watch::channel(ModelStatus::Loading);
        let executable = env::current_exe().unwrap_or_else(|_| PathBuf::from("milevox"));
        tokio::spawn(supervise_worker(
            receiver,
            status_sender,
            WorkerSpec {
                executable,
                model_path,
                fake_behaviors: VecDeque::new(),
                deadline_override: None,
            },
        ));
        Self { commands, status }
    }

    pub async fn transcribe(&self, generation: u64, audio: CapturedAudio) -> Result<String> {
        self.enqueue(generation, audio, false)
            .await?
            .context("transcription was canceled")
    }

    pub async fn transcribe_preview(
        &self,
        generation: u64,
        audio: CapturedAudio,
    ) -> Result<Option<String>> {
        self.enqueue(generation, audio, true).await
    }

    pub async fn cancel(&self, generation: u64) -> bool {
        let (reply, response) = oneshot::channel();
        if self
            .commands
            .send(SupervisorCommand::Cancel { generation, reply })
            .await
            .is_err()
        {
            return true;
        }
        response.await.unwrap_or(true)
    }

    pub async fn wait_until_available(&self) {
        let mut status = self.status.clone();
        while matches!(*status.borrow(), ModelStatus::Loading) && status.changed().await.is_ok() {}
    }

    pub fn subscribe_status(&self) -> watch::Receiver<ModelStatus> {
        self.status.clone()
    }

    async fn enqueue(
        &self,
        generation: u64,
        audio: CapturedAudio,
        allow_empty: bool,
    ) -> Result<Option<String>> {
        let (reply, response) = oneshot::channel();
        let request = TranscriptionRequest {
            generation,
            audio,
            allow_empty,
            reply,
        };
        self.commands
            .send(SupervisorCommand::Request(request))
            .await
            .context("transcription supervisor stopped unexpectedly")?;
        response
            .await
            .context("transcription worker stopped unexpectedly")?
            .map_err(anyhow::Error::msg)
    }
}

#[cfg(test)]
pub(crate) fn supervised_fake_for_daemon(
    fake_behaviors: impl IntoIterator<Item = String>,
) -> ParakeetTranscriber {
    let test_binary = env::current_exe().expect("the test executable path must be available");
    let executable = test_binary
        .parent()
        .and_then(Path::parent)
        .expect("the test executable must be under target/*/deps")
        .join(format!("milevox{}", env::consts::EXE_SUFFIX));
    assert!(
        executable.is_file(),
        "cargo did not build {} for the daemon supervisor test",
        executable.display()
    );
    let (commands, receiver) = mpsc::channel(32);
    let (status_sender, status) = watch::channel(ModelStatus::Loading);
    tokio::spawn(supervise_worker(
        receiver,
        status_sender,
        WorkerSpec {
            executable,
            model_path: PathBuf::from("/tmp/greendale-model"),
            fake_behaviors: fake_behaviors.into_iter().collect(),
            deadline_override: None,
        },
    ));
    ParakeetTranscriber { commands, status }
}

impl RequestQueue {
    fn push(&mut self, request: TranscriptionRequest) {
        if request.generation <= self.canceled_generation {
            let _ = request.reply.send(Ok(None));
            return;
        }
        if request.allow_empty {
            if let Some(replaced) = self.preview.replace(request) {
                let _ = replaced.reply.send(Ok(None));
            }
            return;
        }
        if self.preview.as_ref().map(|preview| preview.generation) == Some(request.generation)
            && let Some(preview) = self.preview.take()
        {
            let _ = preview.reply.send(Ok(None));
        }
        self.finals.push_back(request);
    }

    fn pop(&mut self) -> Option<TranscriptionRequest> {
        self.finals.pop_front().or_else(|| self.preview.take())
    }

    fn cancel(&mut self, generation: u64) {
        self.canceled_generation = self.canceled_generation.max(generation);
        let mut retained = VecDeque::new();
        while let Some(request) = self.finals.pop_front() {
            if request.generation <= self.canceled_generation {
                let _ = request.reply.send(Ok(None));
            } else {
                retained.push_back(request);
            }
        }
        self.finals = retained;
        if self
            .preview
            .as_ref()
            .is_some_and(|request| request.generation <= self.canceled_generation)
            && let Some(request) = self.preview.take()
        {
            let _ = request.reply.send(Ok(None));
        }
    }
}

async fn supervise_worker(
    mut commands: mpsc::Receiver<SupervisorCommand>,
    status: watch::Sender<ModelStatus>,
    mut spec: WorkerSpec,
) {
    let mut queue = RequestQueue::default();
    let mut request_id = 0_u64;
    let mut process = match start_worker(&mut spec, &status).await {
        Ok(process) => process,
        Err(error) => {
            serve_fatal_worker(&mut commands, &status, format!("{error:#}")).await;
            return;
        }
    };

    loop {
        let Some(request) = queue.pop() else {
            tokio::select! {
                biased;
                result = process.child.wait() => {
                    status.send_replace(ModelStatus::Loading);
                    if let Err(error) = result {
                        eprintln!("Milevox could not wait for an exited transcription worker: {error}");
                    }
                    match start_worker(&mut spec, &status).await {
                        Ok(restarted) => process = restarted,
                        Err(error) => {
                            serve_fatal_worker(&mut commands, &status, format!("{error:#}")).await;
                            return;
                        }
                    }
                }
                command = commands.recv() => {
                    let Some(command) = command else {
                        let _ = kill_worker(&mut process).await;
                        return;
                    };
                    handle_idle_command(command, &mut queue);
                }
            }
            continue;
        };
        if request.generation <= queue.canceled_generation {
            let _ = request.reply.send(Ok(None));
            continue;
        }

        request_id = request_id.wrapping_add(1);
        let deadline = spec
            .deadline_override
            .unwrap_or_else(|| inference_deadline(&request.audio));
        match run_supervised_request(
            &mut process,
            &mut commands,
            &mut queue,
            request_id,
            request,
            deadline,
            &status,
        )
        .await
        {
            RequestControl::Continue => {}
            RequestControl::Restart => match start_worker(&mut spec, &status).await {
                Ok(restarted) => process = restarted,
                Err(error) => {
                    serve_fatal_worker(&mut commands, &status, format!("{error:#}")).await;
                    return;
                }
            },
            RequestControl::Stop => return,
        }
    }
}

fn handle_idle_command(command: SupervisorCommand, queue: &mut RequestQueue) {
    match command {
        SupervisorCommand::Request(request) => queue.push(request),
        SupervisorCommand::Cancel { generation, reply } => {
            queue.cancel(generation);
            let _ = reply.send(false);
        }
    }
}

enum RequestControl {
    Continue,
    Restart,
    Stop,
}

async fn run_supervised_request(
    process: &mut WorkerProcess,
    commands: &mut mpsc::Receiver<SupervisorCommand>,
    queue: &mut RequestQueue,
    request_id: u64,
    request: TranscriptionRequest,
    deadline: Duration,
    status: &watch::Sender<ModelStatus>,
) -> RequestControl {
    if let Err(error) = send_worker_request(process, request_id, &request).await {
        status.send_replace(ModelStatus::Loading);
        let _ = request
            .reply
            .send(Err(format!("worker request failed: {error:#}")));
        let _ = kill_worker(process).await;
        return RequestControl::Restart;
    }

    let generation = request.generation;
    let reply = request.reply;
    let mut response = Box::pin(read_worker_message(&mut process.stdout));
    let timeout = tokio::time::sleep(deadline);
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            result = &mut response => {
                let (result, restart) = parse_worker_result(result, request_id);
                drop(response);
                if restart {
                    status.send_replace(ModelStatus::Loading);
                    let _ = reply.send(result);
                    let _ = kill_worker(process).await;
                    return RequestControl::Restart;
                }
                let _ = reply.send(result);
                return RequestControl::Continue;
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    drop(response);
                    let _ = reply.send(Ok(None));
                    let _ = kill_worker(process).await;
                    return RequestControl::Stop;
                };
                match command {
                    SupervisorCommand::Request(request) => queue.push(request),
                    SupervisorCommand::Cancel { generation: canceled, reply: cancel_reply } => {
                        queue.cancel(canceled);
                        if generation <= canceled {
                            drop(response);
                            status.send_replace(ModelStatus::Loading);
                            let _ = reply.send(Ok(None));
                            let _ = cancel_reply.send(true);
                            let _ = kill_worker(process).await;
                            return RequestControl::Restart;
                        }
                        let _ = cancel_reply.send(false);
                    }
                }
            }
            _ = &mut timeout => {
                drop(response);
                status.send_replace(ModelStatus::Loading);
                let _ = reply.send(Err(format!(
                    "transcription worker exceeded its {:.1}-second deadline",
                    deadline.as_secs_f64()
                )));
                let _ = kill_worker(process).await;
                return RequestControl::Restart;
            }
        }
    }
}

fn parse_worker_result(
    result: Result<WorkerMessage>,
    request_id: u64,
) -> (std::result::Result<Option<String>, String>, bool) {
    match result {
        Ok(WorkerMessage::Result {
            request_id: returned,
            transcript,
            error,
        }) if returned == request_id => match (transcript, error) {
            (transcript, None) => (Ok(transcript), false),
            (None, Some(error)) => (Err(error), false),
            _ => (
                Err("transcription worker returned an ambiguous result".into()),
                true,
            ),
        },
        Ok(WorkerMessage::Result {
            request_id: returned,
            ..
        }) => (
            Err(format!(
                "transcription worker returned request {returned} while waiting for {request_id}"
            )),
            true,
        ),
        Ok(_) => (
            Err("transcription worker returned an unexpected message".into()),
            true,
        ),
        Err(error) => (
            Err(format!("transcription worker protocol failed: {error:#}")),
            true,
        ),
    }
}

async fn serve_fatal_worker(
    commands: &mut mpsc::Receiver<SupervisorCommand>,
    status: &watch::Sender<ModelStatus>,
    error: String,
) {
    status.send_replace(ModelStatus::Unavailable(error.clone()));
    while let Some(command) = commands.recv().await {
        match command {
            SupervisorCommand::Request(request) => {
                let _ = request
                    .reply
                    .send(Err(format!("model unavailable: {error}")));
            }
            SupervisorCommand::Cancel { reply, .. } => {
                let _ = reply.send(false);
            }
        }
    }
}

async fn start_worker(
    spec: &mut WorkerSpec,
    status: &watch::Sender<ModelStatus>,
) -> Result<WorkerProcess> {
    status.send_replace(ModelStatus::Loading);
    let mut command = worker_command(spec);
    let mut child = command.spawn().with_context(|| {
        format!(
            "failed to start transcription worker at {}",
            spec.executable.display()
        )
    })?;
    let stdin = child
        .stdin
        .take()
        .context("transcription worker stdin is unavailable")?;
    let mut stdout = child
        .stdout
        .take()
        .context("transcription worker stdout is unavailable")?;
    let readiness =
        match tokio::time::timeout(MODEL_LOAD_DEADLINE, read_worker_message(&mut stdout)).await {
            Ok(Ok(readiness)) => readiness,
            Ok(Err(error)) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(error).context("transcription worker readiness failed");
            }
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                bail!("transcription model load timed out");
            }
        };
    match readiness {
        WorkerMessage::Ready => {
            status.send_replace(ModelStatus::Ready);
            Ok(WorkerProcess {
                child,
                stdin,
                stdout,
            })
        }
        WorkerMessage::LoadError { error } => {
            let _ = child.wait().await;
            bail!("{error}")
        }
        _ => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            bail!("transcription worker did not send a readiness message")
        }
    }
}

fn worker_command(spec: &mut WorkerSpec) -> Command {
    let mut command = Command::new(&spec.executable);
    command
        .arg("__transcription-worker")
        .arg("--model-path")
        .arg(&spec.model_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .env_clear();
    if let Some(value) = env::var_os("LD_LIBRARY_PATH") {
        command.env("LD_LIBRARY_PATH", value);
    }
    let fake = if spec.fake_behaviors.len() > 1 {
        spec.fake_behaviors.pop_front()
    } else {
        spec.fake_behaviors.front().cloned()
    };
    if let Some(fake) = fake {
        command.arg("--fake").arg(fake);
    }
    command
}

async fn send_worker_request(
    process: &mut WorkerProcess,
    request_id: u64,
    request: &TranscriptionRequest,
) -> Result<()> {
    let metadata = WorkerRequest {
        request_id,
        generation: request.generation,
        sample_rate: request.audio.sample_rate,
        sample_count: request.audio.samples.len(),
        allow_empty: request.allow_empty,
    };
    let metadata = serde_json::to_vec(&metadata)?;
    write_frame_async(&mut process.stdin, &metadata).await?;
    let mut samples = Vec::with_capacity(request.audio.samples.len().saturating_mul(4));
    for sample in &request.audio.samples {
        samples.extend_from_slice(&sample.to_le_bytes());
    }
    write_frame_async(&mut process.stdin, &samples).await?;
    process
        .stdin
        .flush()
        .await
        .context("failed to flush transcription request")
}

async fn read_worker_message(reader: &mut ChildStdout) -> Result<WorkerMessage> {
    let frame = read_frame_async(reader, MAX_RESPONSE_BYTES).await?;
    serde_json::from_slice(&frame).context("transcription worker returned malformed JSON")
}

async fn kill_worker(process: &mut WorkerProcess) -> Result<()> {
    match process.child.kill().await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {}
        Err(error) => return Err(error).context("failed to stop transcription worker"),
    }
    let _ = process.child.wait().await;
    Ok(())
}

fn inference_deadline(audio: &CapturedAudio) -> Duration {
    let seconds = if audio.sample_rate == 0 {
        0.0
    } else {
        audio.samples.len() as f64 / f64::from(audio.sample_rate)
    };
    Duration::from_secs_f64((20.0 + seconds * 2.0).min(20.0 * 60.0))
}

pub fn run_worker_mode(model_path: &Path, fake: Option<&str>) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    if let Some(fake) = fake {
        return run_fake_worker(&mut reader, &mut writer, fake);
    }

    let mut model = match load_model(model_path) {
        Ok(model) => model,
        Err(error) => {
            write_worker_message(
                &mut writer,
                &WorkerMessage::LoadError {
                    error: format!("{error:#}"),
                },
            )?;
            return Ok(());
        }
    };
    write_worker_message(&mut writer, &WorkerMessage::Ready)?;
    run_worker_requests(&mut reader, &mut writer, |metadata, samples| {
        transcribe_loaded(&mut model, metadata, samples)
    })
}

fn load_model(model_path: &Path) -> Result<ParakeetTDT> {
    validate_model_path(model_path)?;
    ParakeetTDT::from_pretrained(model_path, None)
        .with_context(|| format!("failed to load Parakeet model at {}", model_path.display()))
}

fn run_worker_requests<R, W, F>(reader: &mut R, writer: &mut W, mut transcribe: F) -> Result<()>
where
    R: Read,
    W: Write,
    F: FnMut(&WorkerRequest, Vec<f32>) -> Result<Option<String>>,
{
    loop {
        let metadata = match read_frame(reader, MAX_METADATA_BYTES) {
            Ok(frame) => serde_json::from_slice::<WorkerRequest>(&frame)
                .context("worker received malformed request metadata")?,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::UnexpectedEof) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let bytes = read_frame(reader, MAX_SAMPLE_BYTES)?;
        if bytes.len() != metadata.sample_count.saturating_mul(4) {
            bail!("worker received a sample frame with the wrong length");
        }
        let (chunks, remainder) = bytes.as_chunks::<4>();
        debug_assert!(remainder.is_empty());
        let samples = chunks
            .iter()
            .map(|bytes| f32::from_le_bytes(*bytes))
            .collect();
        let (transcript, error) = match transcribe(&metadata, samples) {
            Ok(transcript) => (transcript, None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        write_worker_message(
            writer,
            &WorkerMessage::Result {
                request_id: metadata.request_id,
                transcript,
                error,
            },
        )?;
    }
}

fn transcribe_loaded(
    model: &mut ParakeetTDT,
    metadata: &WorkerRequest,
    samples: Vec<f32>,
) -> Result<Option<String>> {
    let samples = resample_mono(samples, metadata.sample_rate, PARAKEET_SAMPLE_RATE)?;
    let result = model
        .transcribe_samples(samples, PARAKEET_SAMPLE_RATE, 1, None)
        .context("Parakeet transcription failed")?;
    let transcript = result.text.trim().to_owned();
    if transcript.is_empty() {
        if metadata.allow_empty {
            return Ok(None);
        }
        bail!("Parakeet returned no text");
    }
    Ok(Some(transcript))
}

fn run_fake_worker<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    behavior: &str,
) -> Result<()> {
    if behavior == "load_error" {
        return write_worker_message(
            writer,
            &WorkerMessage::LoadError {
                error: "fake model load failed".into(),
            },
        );
    }
    let behavior = match behavior.strip_prefix("normal_after:") {
        Some(signal) => {
            wait_for_fake_signal(Path::new(signal));
            "normal"
        }
        None => behavior,
    };
    write_worker_message(writer, &WorkerMessage::Ready)?;
    if let Some(signal) = behavior.strip_prefix("idle_exit_after:") {
        wait_for_fake_signal(Path::new(signal));
        return Ok(());
    }
    if matches!(behavior, "crash" | "malformed") {
        let metadata = read_frame(reader, MAX_METADATA_BYTES)?;
        let _metadata: WorkerRequest = serde_json::from_slice(&metadata)?;
        let _ = read_frame(reader, MAX_SAMPLE_BYTES)?;
        if behavior == "crash" {
            std::process::exit(70);
        }
        write_frame(writer, b"{malformed")?;
        writer.flush()?;
        return Ok(());
    }
    if let Some(signal) = behavior.strip_prefix("block_after:") {
        let signal = PathBuf::from(signal);
        return run_worker_requests(reader, writer, move |_, _| {
            std::fs::write(&signal, [])
                .with_context(|| format!("failed to signal {}", signal.display()))?;
            loop {
                std::thread::park();
            }
        });
    }
    run_worker_requests(reader, writer, |_, _| match behavior {
        "normal" => Ok(Some("Troy and Abed in the morning".into())),
        "environment" => Ok(Some(
            env::vars()
                .map(|(key, _)| key)
                .collect::<Vec<_>>()
                .join(","),
        )),
        "block" => loop {
            std::thread::park();
        },
        other => bail!("unknown fake worker behavior: {other}"),
    })
}

fn wait_for_fake_signal(path: &Path) {
    while !path.exists() {
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn write_worker_message<W: Write>(writer: &mut W, message: &WorkerMessage) -> Result<()> {
    let bytes = serde_json::to_vec(message)?;
    write_frame(writer, &bytes)?;
    writer.flush().context("failed to flush worker response")
}

fn write_frame<W: Write>(writer: &mut W, bytes: &[u8]) -> Result<()> {
    let length = u32::try_from(bytes.len()).context("worker frame is too large")?;
    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(bytes)?;
    Ok(())
}

fn read_frame<R: Read>(reader: &mut R, limit: usize) -> Result<Vec<u8>> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > limit {
        bail!("worker frame exceeds {limit} bytes");
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

async fn write_frame_async<W: AsyncWrite + Unpin>(writer: &mut W, bytes: &[u8]) -> Result<()> {
    let length = u32::try_from(bytes.len()).context("worker frame is too large")?;
    writer.write_all(&length.to_le_bytes()).await?;
    writer.write_all(bytes).await?;
    Ok(())
}

async fn read_frame_async<R: AsyncRead + Unpin>(reader: &mut R, limit: usize) -> Result<Vec<u8>> {
    let length = reader.read_u32_le().await? as usize;
    if length > limit {
        bail!("worker frame exceeds {limit} bytes");
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

fn validate_model_path(path: &Path) -> Result<()> {
    if !path.is_dir() {
        bail!(
            "Parakeet model directory does not exist at {}; set transcription.model_path or install the default model",
            path.display()
        );
    }
    require_one(
        path,
        &[
            "encoder-model.int8.onnx",
            "encoder-model.onnx",
            "encoder.onnx",
        ],
        "encoder model",
    )?;
    require_one(
        path,
        &[
            "decoder_joint-model.int8.onnx",
            "decoder_joint-model.onnx",
            "decoder_joint.onnx",
        ],
        "decoder model",
    )?;
    if !path.join("vocab.txt").is_file() {
        bail!("Parakeet model at {} is missing vocab.txt", path.display());
    }
    Ok(())
}

fn require_one(path: &Path, filenames: &[&str], description: &str) -> Result<()> {
    if filenames
        .iter()
        .any(|filename| path.join(filename).is_file())
    {
        return Ok(());
    }
    bail!(
        "Parakeet model at {} is missing its {description}",
        path.display()
    )
}

fn resample_mono(samples: Vec<f32>, source_rate: u32, target_rate: u32) -> Result<Vec<f32>> {
    if samples.is_empty() {
        bail!("cannot transcribe an empty audio buffer");
    }
    if source_rate == 0 || target_rate == 0 {
        bail!("audio sample rates must be greater than zero");
    }
    if source_rate == target_rate {
        return Ok(samples);
    }

    let input_len = samples.len();
    let input = InterleavedSlice::new(&samples, 1, input_len)
        .context("failed to prepare audio for resampling")?;
    let mut resampler = Fft::<f32>::new(
        source_rate as usize,
        target_rate as usize,
        RESAMPLER_CHUNK_SIZE,
        1,
        FixedSync::Both,
    )
    .context("failed to create the audio resampler")?;
    let output = resampler
        .process_all(&input, input_len, None)
        .context("failed to resample microphone audio")?;
    let mut samples = output.take_data();
    let expected_len =
        ((input_len as u64 * u64::from(target_rate)).div_ceil(u64::from(source_rate))) as usize;
    if samples.len() > expected_len {
        samples.truncate(expected_len);
    } else if samples.len() < expected_len {
        samples.resize(expected_len, samples.last().copied().unwrap_or(0.0));
    }
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

    fn built_milevox_binary() -> PathBuf {
        let test_binary = std::env::current_exe().unwrap();
        test_binary
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .join(format!("milevox{}", std::env::consts::EXE_SUFFIX))
    }

    fn supervised_fake(
        behaviors: &[&str],
        deadline_override: Option<Duration>,
    ) -> ParakeetTranscriber {
        supervised_test_transcriber(
            PathBuf::from("/tmp/greendale-model"),
            behaviors.iter().map(|behavior| (*behavior).to_owned()),
            deadline_override,
        )
    }

    fn supervised_test_transcriber(
        model_path: PathBuf,
        fake_behaviors: impl IntoIterator<Item = String>,
        deadline_override: Option<Duration>,
    ) -> ParakeetTranscriber {
        let executable = built_milevox_binary();
        assert!(
            executable.is_file(),
            "cargo did not build {} for the supervisor test",
            executable.display()
        );
        let (commands, receiver) = mpsc::channel(32);
        let (status_sender, status) = watch::channel(ModelStatus::Loading);
        tokio::spawn(supervise_worker(
            receiver,
            status_sender,
            WorkerSpec {
                executable,
                model_path,
                fake_behaviors: fake_behaviors.into_iter().collect(),
                deadline_override,
            },
        ));
        ParakeetTranscriber { commands, status }
    }

    async fn wait_for_model_status(
        transcriber: &ParakeetTranscriber,
        predicate: impl Fn(&ModelStatus) -> bool,
    ) -> ModelStatus {
        let mut status = transcriber.subscribe_status();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let current = status.borrow().clone();
                if predicate(&current) {
                    return current;
                }
                status.changed().await.unwrap();
            }
        })
        .await
        .expect("transcription model status did not settle")
    }

    fn short_audio() -> CapturedAudio {
        CapturedAudio {
            samples: vec![0.25; 160],
            sample_rate: PARAKEET_SAMPLE_RATE,
        }
    }

    fn tone(rate: u32, frequency: f32, seconds: f32) -> Vec<f32> {
        let len = (rate as f32 * seconds) as usize;
        (0..len)
            .map(|index| (TAU * frequency * index as f32 / rate as f32).sin())
            .collect()
    }

    fn component(samples: &[f32], rate: u32, frequency: f32) -> f32 {
        let (sin, cos) =
            samples
                .iter()
                .enumerate()
                .fold((0.0, 0.0), |(sin, cos), (index, sample)| {
                    let phase = TAU * frequency * index as f32 / rate as f32;
                    (sin + sample * phase.sin(), cos + sample * phase.cos())
                });
        2.0 * (sin * sin + cos * cos).sqrt() / samples.len() as f32
    }

    #[test]
    fn leaves_16khz_audio_unchanged() {
        let samples = vec![-1.0, 0.0, 1.0];
        let pointer = samples.as_ptr();
        let capacity = samples.capacity();
        let output = resample_mono(samples, 16_000, 16_000).unwrap();

        assert_eq!(output, [-1.0, 0.0, 1.0]);
        assert_eq!(output.as_ptr(), pointer);
        assert_eq!(output.capacity(), capacity);
    }

    #[test]
    fn preserves_duration_within_one_target_sample() {
        for (source, target, len) in [(48_000, 16_000, 48_001), (44_100, 16_000, 44_123)] {
            let output = resample_mono(vec![0.25; len], source, target).unwrap();
            let expected = (len as f64 * target as f64 / source as f64).ceil() as usize;
            assert!(output.len().abs_diff(expected) <= 1);
        }
    }

    #[test]
    fn keeps_a_1khz_tone_and_rejects_10khz_aliasing() {
        let passband = resample_mono(tone(48_000, 1_000.0, 0.25), 48_000, 16_000).unwrap();
        let stopband = resample_mono(tone(48_000, 10_000.0, 0.25), 48_000, 16_000).unwrap();

        assert!(component(&passband, 16_000, 1_000.0) > 0.8);
        assert!(component(&stopband, 16_000, 6_000.0) < 0.1);
    }

    #[test]
    fn handles_dc_impulses_and_empty_input() {
        let dc = resample_mono(vec![0.5; 48_000], 48_000, 16_000).unwrap();
        assert!((dc.iter().sum::<f32>() / dc.len() as f32 - 0.5).abs() < 0.01);
        let mut impulse = vec![0.0; 4_800];
        impulse[2_400] = 1.0;
        assert_eq!(resample_mono(impulse, 48_000, 16_000).unwrap().len(), 1_600);
        assert!(resample_mono(Vec::new(), 48_000, 16_000).is_err());
    }

    #[test]
    fn validates_a_quantized_tdt_model_layout() {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "milevox-parakeet-model-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        for filename in [
            "encoder-model.int8.onnx",
            "decoder_joint-model.int8.onnx",
            "vocab.txt",
        ] {
            std::fs::write(path.join(filename), []).unwrap();
        }
        validate_model_path(&path).unwrap();
        std::fs::remove_dir_all(path).unwrap();
    }

    fn request(
        generation: u64,
        allow_empty: bool,
    ) -> (
        TranscriptionRequest,
        oneshot::Receiver<std::result::Result<Option<String>, String>>,
    ) {
        let (reply, response) = oneshot::channel();
        (
            TranscriptionRequest {
                generation,
                audio: CapturedAudio {
                    samples: vec![0.25; 160],
                    sample_rate: 16_000,
                },
                allow_empty,
                reply,
            },
            response,
        )
    }

    #[tokio::test]
    async fn canceled_generations_reject_queued_and_late_requests() {
        let mut queue = RequestQueue::default();
        let (queued, queued_response) = request(1, false);
        queue.push(queued);

        queue.cancel(1);
        let (late, late_response) = request(1, true);
        queue.push(late);

        assert!(queued_response.await.unwrap().unwrap().is_none());
        assert!(late_response.await.unwrap().unwrap().is_none());
        assert!(queue.pop().is_none());
    }

    #[tokio::test]
    async fn supervised_worker_loads_quickly_and_stays_resident_for_requests() {
        let transcriber = supervised_fake(&["normal"], None);
        assert_eq!(
            wait_for_model_status(&transcriber, |status| *status == ModelStatus::Ready).await,
            ModelStatus::Ready
        );

        for generation in [1, 2] {
            assert_eq!(
                transcriber
                    .transcribe(generation, short_audio())
                    .await
                    .unwrap(),
                "Troy and Abed in the morning"
            );
        }
    }

    #[tokio::test]
    async fn idle_worker_exit_publishes_loading_and_restarts_before_the_next_request() {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "milevox-idle-worker-exit-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let exit_signal = directory.join("exit");
        let ready_signal = directory.join("ready");
        let behaviors = [
            format!("idle_exit_after:{}", exit_signal.display()),
            format!("normal_after:{}", ready_signal.display()),
        ];
        let transcriber =
            supervised_test_transcriber(PathBuf::from("/tmp/greendale-model"), behaviors, None);
        wait_for_model_status(&transcriber, |status| *status == ModelStatus::Ready).await;

        std::fs::write(&exit_signal, []).unwrap();
        assert_eq!(
            wait_for_model_status(&transcriber, |status| *status == ModelStatus::Loading).await,
            ModelStatus::Loading
        );
        let request_transcriber = transcriber.clone();
        let request =
            tokio::spawn(async move { request_transcriber.transcribe(1, short_audio()).await });
        tokio::task::yield_now().await;
        assert!(!request.is_finished());

        std::fs::write(&ready_signal, []).unwrap();
        assert_eq!(
            wait_for_model_status(&transcriber, |status| *status == ModelStatus::Ready).await,
            ModelStatus::Ready
        );
        assert_eq!(
            request.await.unwrap().unwrap(),
            "Troy and Abed in the morning"
        );

        drop(transcriber);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn canceling_a_blocked_worker_restarts_it_for_the_next_generation() {
        let transcriber = supervised_fake(&["block", "normal"], None);
        wait_for_model_status(&transcriber, |status| *status == ModelStatus::Ready).await;
        let active_transcriber = transcriber.clone();
        let active =
            tokio::spawn(async move { active_transcriber.transcribe(1, short_audio()).await });
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(transcriber.cancel(1).await);
        assert!(
            active
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("canceled")
        );
        transcriber.wait_until_available().await;
        assert_eq!(
            transcriber.transcribe(2, short_audio()).await.unwrap(),
            "Troy and Abed in the morning"
        );
    }

    #[tokio::test]
    async fn crash_and_malformed_output_fail_the_request_then_restart() {
        for behavior in ["crash", "malformed"] {
            let transcriber = supervised_fake(&[behavior, "normal"], None);
            wait_for_model_status(&transcriber, |status| *status == ModelStatus::Ready).await;

            let error = transcriber
                .transcribe(1, short_audio())
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("worker"), "{behavior}: {error}");
            transcriber.wait_until_available().await;
            assert_eq!(
                transcriber.transcribe(2, short_audio()).await.unwrap(),
                "Troy and Abed in the morning",
                "{behavior}"
            );
        }
    }

    #[tokio::test]
    async fn a_worker_deadline_fails_the_request_and_restarts() {
        let transcriber = supervised_fake(&["block", "normal"], Some(Duration::from_millis(100)));
        wait_for_model_status(&transcriber, |status| *status == ModelStatus::Ready).await;

        let error = transcriber
            .transcribe(1, short_audio())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("deadline"), "{error}");
        transcriber.wait_until_available().await;
        assert_eq!(
            transcriber.transcribe(2, short_audio()).await.unwrap(),
            "Troy and Abed in the morning"
        );
    }

    #[tokio::test]
    async fn fatal_loader_errors_are_cached_and_never_publish_ready() {
        let transcriber = supervised_fake(&["load_error"], None);
        let status = wait_for_model_status(&transcriber, |status| {
            matches!(status, ModelStatus::Unavailable(_))
        })
        .await;
        assert!(
            matches!(status, ModelStatus::Unavailable(ref error) if error.contains("fake model load failed"))
        );

        let error = transcriber
            .transcribe(1, short_audio())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("model unavailable"));
        assert!(matches!(
            *transcriber.subscribe_status().borrow(),
            ModelStatus::Unavailable(_)
        ));
    }

    #[tokio::test]
    async fn real_worker_process_receives_only_the_reviewed_environment() {
        let transcriber = supervised_fake(&["environment"], None);
        wait_for_model_status(&transcriber, |status| *status == ModelStatus::Ready).await;

        let environment = transcriber.transcribe(1, short_audio()).await.unwrap();
        assert!(!environment.contains("OPENROUTER_API_KEY"));
        assert!(!environment.contains("OPENCODE_ZEN_API_KEY"));
        assert!(
            !environment
                .split(',')
                .any(|key| matches!(key, "HOME" | "PATH"))
        );
    }

    #[tokio::test]
    async fn missing_and_corrupt_models_remain_unavailable() {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "milevox-invalid-models-{}-{id}",
            std::process::id()
        ));
        let missing = root.join("missing");
        let corrupt = root.join("corrupt");
        std::fs::create_dir_all(&corrupt).unwrap();
        for filename in [
            "encoder-model.int8.onnx",
            "decoder_joint-model.int8.onnx",
            "vocab.txt",
        ] {
            std::fs::write(corrupt.join(filename), b"not a model").unwrap();
        }

        for model_path in [missing, corrupt] {
            let transcriber = supervised_test_transcriber(model_path, [], None);
            let status = wait_for_model_status(&transcriber, |status| {
                matches!(status, ModelStatus::Unavailable(_))
            })
            .await;
            assert!(matches!(status, ModelStatus::Unavailable(_)));
            assert!(!matches!(
                *transcriber.subscribe_status().borrow(),
                ModelStatus::Ready
            ));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn framed_fake_worker_returns_ready_and_little_endian_samples() {
        let metadata = WorkerRequest {
            request_id: 7,
            generation: 3,
            sample_rate: 16_000,
            sample_count: 2,
            allow_empty: false,
        };
        let mut input = Vec::new();
        write_frame(&mut input, &serde_json::to_vec(&metadata).unwrap()).unwrap();
        let samples = [0.25_f32, -0.5_f32]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        write_frame(&mut input, &samples).unwrap();
        let mut output = Vec::new();

        run_fake_worker(&mut std::io::Cursor::new(input), &mut output, "normal").unwrap();

        let mut output = std::io::Cursor::new(output);
        let ready: WorkerMessage =
            serde_json::from_slice(&read_frame(&mut output, MAX_RESPONSE_BYTES).unwrap()).unwrap();
        assert!(matches!(ready, WorkerMessage::Ready));
        let result: WorkerMessage =
            serde_json::from_slice(&read_frame(&mut output, MAX_RESPONSE_BYTES).unwrap()).unwrap();
        assert!(matches!(
            result,
            WorkerMessage::Result {
                request_id: 7,
                transcript: Some(ref transcript),
                error: None,
            } if transcript == "Troy and Abed in the morning"
        ));
    }

    #[test]
    fn worker_environment_contains_only_reviewed_runtime_values() {
        let mut spec = WorkerSpec {
            executable: PathBuf::from("/bin/true"),
            model_path: PathBuf::from("/tmp/greendale-model"),
            fake_behaviors: VecDeque::new(),
            deadline_override: None,
        };
        let command = worker_command(&mut spec);
        let environment = command.as_std().get_envs().collect::<Vec<_>>();

        assert!(environment.iter().all(|(key, _)| *key == "LD_LIBRARY_PATH"));
        assert!(environment.iter().all(|(key, _)| {
            !matches!(
                key.to_str(),
                Some("OPENROUTER_API_KEY" | "OPENCODE_ZEN_API_KEY")
            )
        }));
    }

    #[test]
    fn protocol_errors_restart_the_worker_but_inference_errors_do_not() {
        let inference = Ok(WorkerMessage::Result {
            request_id: 4,
            transcript: None,
            error: Some("Parakeet inference failed".into()),
        });
        let (result, restart) = parse_worker_result(inference, 4);
        assert_eq!(result.unwrap_err(), "Parakeet inference failed");
        assert!(!restart);

        let (result, restart) =
            parse_worker_result(Err(anyhow::anyhow!("malformed worker frame")), 4);
        assert!(result.unwrap_err().contains("protocol failed"));
        assert!(restart);
    }

    #[test]
    fn inference_deadline_scales_with_audio_and_has_a_global_cap() {
        let short = CapturedAudio {
            samples: vec![0.0; 16_000],
            sample_rate: 16_000,
        };
        let long = CapturedAudio {
            samples: vec![0.0; 160_000],
            sample_rate: 16_000,
        };
        assert_eq!(inference_deadline(&short), Duration::from_secs(22));
        assert_eq!(inference_deadline(&long), Duration::from_secs(40));

        let metadata_only = CapturedAudio {
            samples: Vec::new(),
            sample_rate: 0,
        };
        assert_eq!(inference_deadline(&metadata_only), Duration::from_secs(20));
    }
}
