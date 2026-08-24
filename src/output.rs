use std::ffi::{OsStr, OsString};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

use crate::config::{OutputConfig, OutputMode};
use crate::ipc::{DeliveryMethod, Notice};

const OUTPUT_TIMEOUT: Duration = Duration::from_secs(5);
const TARGET_RESPONSE_MAX_BYTES: usize = 16 * 1024;
const CHILD_STDERR_MAX_BYTES: usize = 16 * 1024;

#[derive(Debug)]
pub struct DeliveryResult {
    pub method: DeliveryMethod,
    pub notices: Vec<Notice>,
}

#[derive(Clone, Copy)]
struct OutputPrograms<'a> {
    wtype: &'a OsStr,
    wl_copy: &'a OsStr,
    environment: &'a [(OsString, OsString)],
    timeout: Duration,
}

pub async fn capture_output_target() -> Option<String> {
    let environment = std::env::vars_os().collect::<Vec<_>>();
    HyprlandTargetResolver {
        program: OsStr::new("hyprctl"),
        environment: &environment,
        timeout: OUTPUT_TIMEOUT,
    }
    .active_target()
    .await
    .ok()
    .flatten()
}

pub async fn deliver_to_target(
    config: &OutputConfig,
    text: &str,
    expected_target: Option<&str>,
) -> Result<DeliveryResult> {
    let environment = std::env::vars_os().collect::<Vec<_>>();
    let resolver = HyprlandTargetResolver {
        program: OsStr::new("hyprctl"),
        environment: &environment,
        timeout: OUTPUT_TIMEOUT,
    };
    deliver_with_target(
        config,
        text,
        expected_target,
        &resolver,
        OutputPrograms {
            wtype: OsStr::new("wtype"),
            wl_copy: OsStr::new("wl-copy"),
            environment: &environment,
            timeout: OUTPUT_TIMEOUT,
        },
    )
    .await
}

pub async fn copy_transcript(text: &str) -> Result<()> {
    let environment = std::env::vars_os().collect::<Vec<_>>();
    copy_to_clipboard(OsStr::new("wl-copy"), text, &environment, OUTPUT_TIMEOUT).await
}

#[cfg(test)]
async fn deliver_with(
    config: &OutputConfig,
    text: &str,
    wtype: &OsStr,
    wl_copy: &OsStr,
    environment: &[(OsString, OsString)],
    timeout: Duration,
) -> Result<DeliveryResult> {
    deliver_after_target_check(config, text, Ok(()), wtype, wl_copy, environment, timeout).await
}

async fn deliver_with_target<R: TargetResolver>(
    config: &OutputConfig,
    text: &str,
    expected_target: Option<&str>,
    resolver: &R,
    programs: OutputPrograms<'_>,
) -> Result<DeliveryResult> {
    let target_check = if matches!(config.mode, OutputMode::Type) && !text.trim().is_empty() {
        verify_target(expected_target, resolver).await
    } else {
        Ok(())
    };
    deliver_after_target_check(
        config,
        text,
        target_check,
        programs.wtype,
        programs.wl_copy,
        programs.environment,
        programs.timeout,
    )
    .await
}

async fn deliver_after_target_check(
    config: &OutputConfig,
    text: &str,
    target_check: Result<()>,
    wtype: &OsStr,
    wl_copy: &OsStr,
    environment: &[(OsString, OsString)],
    timeout: Duration,
) -> Result<DeliveryResult> {
    if text.trim().is_empty() {
        return Ok(DeliveryResult {
            method: DeliveryMethod::None,
            notices: Vec::new(),
        });
    }

    match config.mode {
        OutputMode::Clipboard => {
            copy_to_clipboard(wl_copy, text, environment, timeout).await?;
            Ok(DeliveryResult {
                method: DeliveryMethod::Clipboard,
                notices: Vec::new(),
            })
        }
        OutputMode::Type => match target_check {
            Ok(()) => match type_text(wtype, text, environment, timeout).await {
                Ok(()) => Ok(DeliveryResult {
                    method: DeliveryMethod::Typed,
                    notices: Vec::new(),
                }),
                Err(error) => {
                    handle_typing_failure(config, text, error, wl_copy, environment, timeout).await
                }
            },
            Err(error) => {
                handle_typing_failure(config, text, error, wl_copy, environment, timeout).await
            }
        },
    }
}

async fn handle_typing_failure(
    config: &OutputConfig,
    text: &str,
    typing_error: anyhow::Error,
    wl_copy: &OsStr,
    environment: &[(OsString, OsString)],
    timeout: Duration,
) -> Result<DeliveryResult> {
    match config.clipboard_fallback {
        false => Err(typing_error),
        true => {
            copy_to_clipboard(wl_copy, text, environment, timeout)
                .await
                .context(format!(
                    "typing failed ({typing_error:#}); clipboard fallback also failed"
                ))?;
            Ok(DeliveryResult {
                method: DeliveryMethod::ClipboardFallback,
                notices: vec![
                    Notice::warning(
                        "clipboard_fallback",
                        "Typing failed; transcript copied to the clipboard",
                    )
                    .with_detail(format!("{typing_error:#}")),
                ],
            })
        }
    }
}

trait TargetResolver {
    async fn active_target(&self) -> Result<Option<String>>;
}

struct HyprlandTargetResolver<'a> {
    program: &'a OsStr,
    environment: &'a [(OsString, OsString)],
    timeout: Duration,
}

impl TargetResolver for HyprlandTargetResolver<'_> {
    async fn active_target(&self) -> Result<Option<String>> {
        let mut environment = self.environment.to_vec();
        if !has_nonempty_environment_value(&environment, "HYPRLAND_INSTANCE_SIGNATURE") {
            let (signature, wayland_display) = self.single_instance().await?;
            environment.push(("HYPRLAND_INSTANCE_SIGNATURE".into(), signature.into()));
            if !has_nonempty_environment_value(&environment, "WAYLAND_DISPLAY")
                && let Some(wayland_display) = wayland_display
            {
                environment.push(("WAYLAND_DISPLAY".into(), wayland_display.into()));
            }
        }
        let output = run_capturing_output(
            self.program,
            &[OsStr::new("-j"), OsStr::new("activewindow")],
            &environment,
            self.timeout,
            TARGET_RESPONSE_MAX_BYTES,
        )
        .await
        .context("failed to inspect the active Hyprland window")?;
        if !output.status.success() {
            bail!("hyprctl exited with {}: {}", output.status, output.stderr);
        }
        let response: Value = serde_json::from_slice(&output.stdout)
            .context("hyprctl returned malformed active-window data")?;
        Ok(response
            .get("address")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|address| !address.is_empty() && *address != "0x0")
            .map(str::to_owned))
    }
}

impl HyprlandTargetResolver<'_> {
    async fn single_instance(&self) -> Result<(String, Option<String>)> {
        let output = run_capturing_output(
            self.program,
            &[OsStr::new("instances"), OsStr::new("-j")],
            self.environment,
            self.timeout,
            TARGET_RESPONSE_MAX_BYTES,
        )
        .await
        .context("failed to discover the active Hyprland instance")?;
        if !output.status.success() {
            bail!(
                "hyprctl instances exited with {}: {}",
                output.status,
                output.stderr
            );
        }
        let response: Value = serde_json::from_slice(&output.stdout)
            .context("hyprctl returned malformed instance data")?;
        let instances = response
            .as_array()
            .context("hyprctl returned invalid instance data")?;
        if instances.len() != 1 {
            bail!(
                "hyprctl reported {} instances; cannot select one safely",
                instances.len()
            );
        }
        let instance = &instances[0];
        let signature = instance
            .get("instance")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|signature| !signature.is_empty())
            .context("hyprctl returned an instance without a signature")?;
        let wayland_display = instance
            .get("wl_socket")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|display| !display.is_empty())
            .map(str::to_owned);
        Ok((signature.to_owned(), wayland_display))
    }
}

fn has_nonempty_environment_value(environment: &[(OsString, OsString)], name: &str) -> bool {
    environment
        .iter()
        .any(|(key, value)| key == OsStr::new(name) && !value.is_empty())
}

async fn verify_target<R: TargetResolver>(
    expected_target: Option<&str>,
    resolver: &R,
) -> Result<()> {
    let expected_target = expected_target
        .filter(|target| !target.is_empty())
        .context("no output target was captured when recording began")?;
    let active_target = resolver
        .active_target()
        .await
        .context("could not verify the active output target")?
        .context("the recorded output target is no longer active")?;
    if active_target != expected_target {
        bail!("the active output target changed during dictation");
    }
    Ok(())
}

async fn type_text(
    program: &OsStr,
    text: &str,
    environment: &[(OsString, OsString)],
    timeout: Duration,
) -> Result<()> {
    if text.chars().any(char::is_control) {
        bail!("transcript contains a control character that cannot be typed safely");
    }
    let output = run_with_stdin(
        program,
        &[OsStr::new("-")],
        text,
        environment,
        timeout,
        SuccessfulStderr::WaitForEof,
    )
    .await
    .context("wtype failed")?;
    if output.status.success() {
        return Ok(());
    }
    bail!("wtype exited with {}: {}", output.status, output.stderr)
}

async fn copy_to_clipboard(
    program: &OsStr,
    text: &str,
    environment: &[(OsString, OsString)],
    timeout: Duration,
) -> Result<()> {
    let output = run_with_stdin(
        program,
        &[],
        text,
        environment,
        timeout,
        SuccessfulStderr::Detach,
    )
    .await
    .context("wl-copy failed")?;
    if output.status.success() {
        return Ok(());
    }
    bail!("wl-copy exited with {}: {}", output.status, output.stderr)
}

struct ChildOutput {
    status: ExitStatus,
    stderr: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SuccessfulStderr {
    WaitForEof,
    // wl-copy forks a clipboard server that intentionally retains stderr.
    Detach,
}

struct CapturedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: String,
}

async fn run_capturing_output(
    program: &OsStr,
    arguments: &[&OsStr],
    environment: &[(OsString, OsString)],
    timeout: Duration,
    max_bytes: usize,
) -> Result<CapturedOutput> {
    let mut child = reviewed_command(program, environment)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to start {}", program.to_string_lossy()))?;
    let process_group = child.id();
    let mut stdout = child.stdout.take().context("child stdout is unavailable")?;
    let mut stderr = child.stderr.take().context("child stderr is unavailable")?;
    let result = tokio::time::timeout(timeout, async {
        tokio::try_join!(
            child.wait(),
            read_capped(&mut stdout, max_bytes),
            read_capped(&mut stderr, max_bytes),
        )
    })
    .await;
    match result {
        Ok(Ok((status, stdout, stderr))) => Ok(CapturedOutput {
            status,
            stdout,
            stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
        }),
        Ok(Err(error)) => {
            stop_child(&mut child, program, process_group).await?;
            Err(error).with_context(|| {
                format!("failed to read output from {}", program.to_string_lossy())
            })
        }
        Err(_) => {
            stop_child(&mut child, program, process_group).await?;
            bail!("{} timed out", program.to_string_lossy())
        }
    }
}

async fn read_capped(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    max_bytes: usize,
) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(bytes);
        }
        if bytes.len().saturating_add(count) > max_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("child output exceeds {max_bytes} bytes"),
            ));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
}

async fn run_with_stdin(
    program: &OsStr,
    arguments: &[&OsStr],
    input: &str,
    environment: &[(OsString, OsString)],
    timeout: Duration,
    successful_stderr: SuccessfulStderr,
) -> Result<ChildOutput> {
    let mut command = reviewed_command(program, environment);
    let mut child = command
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to start {}", program.to_string_lossy()))?;
    let process_group = child.id();
    let mut stdin = child.stdin.take().context("child stdin is unavailable")?;
    let mut stderr = child.stderr.take().context("child stderr is unavailable")?;
    let mut stderr_reader =
        tokio::spawn(async move { read_capped(&mut stderr, CHILD_STDERR_MAX_BYTES).await });
    let operation = async {
        stdin
            .write_all(input.as_bytes())
            .await
            .with_context(|| format!("failed to write input to {}", program.to_string_lossy()))?;
        drop(stdin);
        let status = child
            .wait()
            .await
            .with_context(|| format!("failed to wait for {}", program.to_string_lossy()))?;
        let stderr = if status.success() && successful_stderr == SuccessfulStderr::Detach {
            stderr_reader.abort();
            match (&mut stderr_reader).await {
                Ok(stderr) => stderr.context("failed to read child stderr")?,
                Err(error) if error.is_cancelled() => Vec::new(),
                Err(error) => {
                    return Err(error).context("child stderr reader stopped unexpectedly");
                }
            }
        } else {
            (&mut stderr_reader)
                .await
                .context("child stderr reader stopped unexpectedly")?
                .context("failed to read child stderr")?
        };
        Ok::<_, anyhow::Error>(ChildOutput {
            status,
            stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
        })
    };
    match tokio::time::timeout(timeout, operation).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => {
            stderr_reader.abort();
            stop_child(&mut child, program, process_group).await?;
            Err(error)
        }
        Err(_) => {
            stop_child(&mut child, program, process_group).await?;
            stderr_reader.abort();
            let _ = stderr_reader.await;
            bail!("{} timed out", program.to_string_lossy())
        }
    }
}

fn reviewed_command(program: &OsStr, environment: &[(OsString, OsString)]) -> Command {
    let mut command = Command::new(program);
    command.env_clear();
    #[cfg(unix)]
    command.process_group(0);
    for (key, value) in environment {
        if is_reviewed_environment_key(key) {
            command.env(key, value);
        }
    }
    command
}

fn is_reviewed_environment_key(key: &OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    matches!(
        key,
        "PATH"
            | "HOME"
            | "WAYLAND_DISPLAY"
            | "HYPRLAND_INSTANCE_SIGNATURE"
            | "XDG_RUNTIME_DIR"
            | "XKB_CONFIG_ROOT"
            | "LANG"
            | "LANGUAGE"
            | "LC_ALL"
            | "LC_ADDRESS"
            | "LC_COLLATE"
            | "LC_CTYPE"
            | "LC_IDENTIFICATION"
            | "LC_MEASUREMENT"
            | "LC_MESSAGES"
            | "LC_MONETARY"
            | "LC_NAME"
            | "LC_NUMERIC"
            | "LC_PAPER"
            | "LC_TELEPHONE"
            | "LC_TIME"
    )
}

async fn stop_child(child: &mut Child, program: &OsStr, process_group: Option<u32>) -> Result<()> {
    #[cfg(unix)]
    if let Some(process_group) = process_group {
        let process_group =
            i32::try_from(process_group).context("child process ID is too large")?;
        let result = unsafe { libc::kill(-process_group, libc::SIGKILL) };
        if result == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error).with_context(|| {
                    format!("failed to stop {} process group", program.to_string_lossy())
                });
            }
        }
    }
    if child
        .try_wait()
        .with_context(|| format!("failed to inspect {}", program.to_string_lossy()))?
        .is_some()
    {
        return Ok(());
    }
    #[cfg(not(unix))]
    child
        .kill()
        .await
        .with_context(|| format!("failed to stop timed-out {}", program.to_string_lossy()))?;
    child
        .wait()
        .await
        .with_context(|| format!("failed to reap timed-out {}", program.to_string_lossy()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

    fn test_directory(name: &str) -> PathBuf {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("milevox-output-{name}-{}-{id}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        directory
    }

    fn fake_program(directory: &Path, name: &str, body: &str) -> PathBuf {
        let path = directory.join(name);
        let script = format!(
            "#!/usr/bin/env bash\n# Fake Milevox output helper.\nset -euo pipefail\n{body}\n"
        );
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn test_environment(directory: &Path) -> Vec<(OsString, OsString)> {
        [
            ("PATH", "/usr/bin:/bin"),
            ("HOME", directory.to_str().unwrap()),
            ("WAYLAND_DISPLAY", "wayland-greendale"),
            ("HYPRLAND_INSTANCE_SIGNATURE", "greendale-hyprland"),
            ("XDG_RUNTIME_DIR", directory.to_str().unwrap()),
            ("XKB_CONFIG_ROOT", "/usr/share/X11/xkb"),
            ("LANG", "en_CA.UTF-8"),
            ("LC_CTYPE", "en_CA.UTF-8"),
            ("LC_SESSION_SECRET", "locale-shaped-secret"),
            ("OPENROUTER_API_KEY", "provider-secret"),
            ("MILEVOX_ARBITRARY_SECRET", "session-secret"),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect()
    }

    #[derive(Clone, Copy)]
    enum FixedResolution {
        Target(Option<&'static str>),
        Unavailable,
    }

    struct FixedTargetResolver(FixedResolution);

    impl TargetResolver for FixedTargetResolver {
        async fn active_target(&self) -> Result<Option<String>> {
            match self.0 {
                FixedResolution::Target(target) => Ok(target.map(str::to_owned)),
                FixedResolution::Unavailable => bail!("Hyprland is unavailable"),
            }
        }
    }

    #[tokio::test]
    async fn wtype_receives_text_only_on_stdin_with_a_reviewed_environment() {
        let directory = test_directory("wtype-stdin");
        let program = fake_program(
            &directory,
            "wtype",
            r#"{
  printf 'argc=%s\n' "$#"
  for argument in "$@"; do printf 'arg=%s\n' "$argument"; done
  printf 'stdin='
  cat
  printf '\n--environment--\n'
  env | sort
} > "${0}.capture""#,
        );
        let transcript = "Troy, Abed, and café punctuation!";

        type_text(
            program.as_os_str(),
            transcript,
            &test_environment(&directory),
            Duration::from_secs(1),
        )
        .await
        .unwrap();

        let capture = fs::read_to_string(program.with_extension("capture")).unwrap();
        assert!(capture.contains("argc=1\narg=-\n"));
        assert!(capture.contains(&format!("stdin={transcript}\n")));
        assert!(!capture.contains("arg=Troy"));
        assert!(!capture.contains("provider-secret"));
        assert!(!capture.contains("session-secret"));
        assert!(!capture.contains("locale-shaped-secret"));
        assert!(capture.contains("WAYLAND_DISPLAY=wayland-greendale"));
        assert!(capture.contains("HYPRLAND_INSTANCE_SIGNATURE=greendale-hyprland"));
        assert!(capture.contains("LC_CTYPE=en_CA.UTF-8"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn wtype_rejects_control_characters_before_spawn() {
        let missing = Path::new("/definitely/missing/wtype");
        for character in ['\n', '\r', '\t', '\u{1b}', '\0', '\u{7f}'] {
            let error = type_text(
                missing.as_os_str(),
                &format!("Troy{character}Abed"),
                &[],
                Duration::from_millis(10),
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("control character"));
        }
    }

    #[tokio::test]
    async fn explicit_type_mode_requires_the_original_active_target() {
        let directory = test_directory("target-match");
        let wtype = fake_program(
            &directory,
            "wtype",
            "touch \"${0}.started\"\ncat > \"${0}.stdin\"",
        );
        let config = OutputConfig {
            mode: OutputMode::Type,
            clipboard_fallback: false,
        };

        let result = deliver_with_target(
            &config,
            "Troy and Abed",
            Some("0xdecaf"),
            &FixedTargetResolver(FixedResolution::Target(Some("0xdecaf"))),
            OutputPrograms {
                wtype: wtype.as_os_str(),
                wl_copy: OsStr::new("/definitely/missing/wl-copy"),
                environment: &test_environment(&directory),
                timeout: Duration::from_secs(1),
            },
        )
        .await
        .unwrap();

        assert_eq!(result.method, DeliveryMethod::Typed);
        assert!(wtype.with_extension("started").exists());
        assert_eq!(
            fs::read_to_string(wtype.with_extension("stdin")).unwrap(),
            "Troy and Abed"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn target_failures_never_start_wtype() {
        let directory = test_directory("target-failures");
        let wtype = fake_program(&directory, "wtype", "touch \"${0}.started\"");
        let config = OutputConfig {
            mode: OutputMode::Type,
            clipboard_fallback: false,
        };
        let cases = [
            (
                None,
                FixedResolution::Target(Some("0xdecaf")),
                "no output target",
            ),
            (
                Some("0xdecaf"),
                FixedResolution::Target(Some("0xcafe")),
                "target changed",
            ),
            (
                Some("0xdecaf"),
                FixedResolution::Target(None),
                "no longer active",
            ),
            (
                Some("0xdecaf"),
                FixedResolution::Unavailable,
                "could not verify",
            ),
        ];

        for (expected_target, resolution, expected_error) in cases {
            let error = deliver_with_target(
                &config,
                "Annie Edison",
                expected_target,
                &FixedTargetResolver(resolution),
                OutputPrograms {
                    wtype: wtype.as_os_str(),
                    wl_copy: OsStr::new("/definitely/missing/wl-copy"),
                    environment: &test_environment(&directory),
                    timeout: Duration::from_secs(1),
                },
            )
            .await
            .unwrap_err();
            assert!(format!("{error:#}").contains(expected_error));
            assert!(!wtype.with_extension("started").exists());
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn changed_target_uses_only_an_explicit_clipboard_fallback() {
        let directory = test_directory("target-fallback");
        let wtype = fake_program(&directory, "wtype", "touch \"${0}.started\"");
        let wl_copy = fake_program(&directory, "wl-copy", "cat > \"${0}.stdin\"");
        let config = OutputConfig {
            mode: OutputMode::Type,
            clipboard_fallback: true,
        };

        let result = deliver_with_target(
            &config,
            "Britta Perry",
            Some("0xdecaf"),
            &FixedTargetResolver(FixedResolution::Target(Some("0xcafe"))),
            OutputPrograms {
                wtype: wtype.as_os_str(),
                wl_copy: wl_copy.as_os_str(),
                environment: &test_environment(&directory),
                timeout: Duration::from_secs(1),
            },
        )
        .await
        .unwrap();

        assert_eq!(result.method, DeliveryMethod::ClipboardFallback);
        assert!(!wtype.with_extension("started").exists());
        assert_eq!(
            fs::read_to_string(wl_copy.with_extension("stdin")).unwrap(),
            "Britta Perry"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn hyprland_resolver_returns_only_the_opaque_window_address() {
        let directory = test_directory("hyprland-target");
        let hyprctl = fake_program(
            &directory,
            "hyprctl",
            "printf '%s\\n' \"$*\" > \"${0}.args\"\nprintf '%s' '{\"address\":\"0xdecaf\",\"class\":\"secret-class\",\"title\":\"secret title\"}'",
        );
        let environment = test_environment(&directory);
        let resolver = HyprlandTargetResolver {
            program: hyprctl.as_os_str(),
            environment: &environment,
            timeout: Duration::from_secs(1),
        };

        assert_eq!(
            resolver.active_target().await.unwrap().as_deref(),
            Some("0xdecaf")
        );
        assert_eq!(
            fs::read_to_string(hyprctl.with_extension("args")).unwrap(),
            "-j activewindow\n"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn hyprland_resolver_discovers_the_only_local_instance() {
        let directory = test_directory("hyprland-discovery");
        let hyprctl = fake_program(
            &directory,
            "hyprctl",
            r#"printf '%s\n' "$*" >> "${0}.args"
if [[ "$*" == "instances -j" ]]; then
  printf '%s' '[{"instance":"greendale-instance","wl_socket":"wayland-greendale"}]'
elif [[ "${HYPRLAND_INSTANCE_SIGNATURE:-}" == "greendale-instance" && "${WAYLAND_DISPLAY:-}" == "wayland-greendale" ]]; then
  printf '%s' '{"address":"0xdecaf"}'
else
  printf 'missing discovered environment\n' >&2
  exit 23
fi"#,
        );
        let environment = test_environment(&directory)
            .into_iter()
            .filter(|(key, _)| {
                key != OsStr::new("HYPRLAND_INSTANCE_SIGNATURE")
                    && key != OsStr::new("WAYLAND_DISPLAY")
            })
            .collect::<Vec<_>>();
        let resolver = HyprlandTargetResolver {
            program: hyprctl.as_os_str(),
            environment: &environment,
            timeout: Duration::from_secs(1),
        };

        assert_eq!(
            resolver.active_target().await.unwrap().as_deref(),
            Some("0xdecaf")
        );
        assert_eq!(
            fs::read_to_string(hyprctl.with_extension("args")).unwrap(),
            "instances -j\n-j activewindow\n"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn hyprland_resolver_refuses_to_guess_between_instances() {
        let directory = test_directory("hyprland-ambiguous");
        let hyprctl = fake_program(
            &directory,
            "hyprctl",
            r#"printf '%s\n' "$*" >> "$0.args"
printf '%s' '[{"instance":"greendale-one"},{"instance":"greendale-two"}]'"#,
        );
        let resolver = HyprlandTargetResolver {
            program: hyprctl.as_os_str(),
            environment: &[],
            timeout: Duration::from_secs(1),
        };

        let error = resolver.active_target().await.unwrap_err();

        assert!(format!("{error:#}").contains("2 instances"));
        assert_eq!(
            fs::read_to_string(hyprctl.with_extension("args")).unwrap(),
            "instances -j\n"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn wl_copy_reports_stderr_and_receives_stdin() {
        let directory = test_directory("clipboard-stderr");
        let program = fake_program(
            &directory,
            "wl-copy",
            "cat > \"${0}.stdin\"\nprintf 'Greendale clipboard failed\\n' >&2\nexit 23",
        );

        let error = copy_to_clipboard(
            program.as_os_str(),
            "Troy and Abed",
            &test_environment(&directory),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();

        assert!(format!("{error:#}").contains("Greendale clipboard failed"));
        assert_eq!(
            fs::read_to_string(program.with_extension("stdin")).unwrap(),
            "Troy and Abed"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn wl_copy_accepts_a_successful_daemonized_clipboard_server() {
        let directory = test_directory("clipboard-daemon");
        let program = fake_program(
            &directory,
            "wl-copy",
            "cat > \"${0}.stdin\"\nsleep 30 &\nprintf '%s' \"$!\" > \"${0}.descendant\"\nexit 0",
        );
        let started = std::time::Instant::now();

        copy_to_clipboard(
            program.as_os_str(),
            "Troy and Abed",
            &test_environment(&directory),
            Duration::from_secs(1),
        )
        .await
        .unwrap();

        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            fs::read_to_string(program.with_extension("stdin")).unwrap(),
            "Troy and Abed"
        );
        let descendant_pid = fs::read_to_string(program.with_extension("descendant"))
            .unwrap()
            .parse::<i32>()
            .unwrap();
        // SAFETY: the test owns the recorded descendant process.
        assert_eq!(unsafe { libc::kill(descendant_pid, libc::SIGKILL) }, 0);
        for _ in 0..100 {
            // SAFETY: signal 0 only checks whether the recorded process still exists.
            if unsafe { libc::kill(descendant_pid, 0) } == -1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // SAFETY: signal 0 only checks whether the recorded process still exists.
        assert_eq!(unsafe { libc::kill(descendant_pid, 0) }, -1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn clipboard_fallback_requires_explicit_permission() {
        let directory = test_directory("fallback");
        let wtype = fake_program(&directory, "wtype", "cat >/dev/null\nexit 12");
        let wl_copy = fake_program(&directory, "wl-copy", "cat > \"${0}.stdin\"");
        let environment = test_environment(&directory);
        let mut config = OutputConfig {
            mode: OutputMode::Type,
            clipboard_fallback: false,
        };

        assert!(
            deliver_with(
                &config,
                "Dean Pelton",
                wtype.as_os_str(),
                wl_copy.as_os_str(),
                &environment,
                Duration::from_secs(1),
            )
            .await
            .is_err()
        );
        assert!(!wl_copy.with_extension("stdin").exists());

        config.clipboard_fallback = true;
        let result = deliver_with(
            &config,
            "Dean Pelton",
            wtype.as_os_str(),
            wl_copy.as_os_str(),
            &environment,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(result.method, DeliveryMethod::ClipboardFallback);
        assert_eq!(
            fs::read_to_string(wl_copy.with_extension("stdin")).unwrap(),
            "Dean Pelton"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn safe_default_never_starts_wtype() {
        let directory = test_directory("safe-default");
        let wtype = fake_program(&directory, "wtype", "touch \"${0}.started\"");
        let wl_copy = fake_program(&directory, "wl-copy", "cat > \"${0}.stdin\"");

        let result = deliver_with(
            &OutputConfig::default(),
            "Shirley Bennett",
            wtype.as_os_str(),
            wl_copy.as_os_str(),
            &test_environment(&directory),
            Duration::from_secs(1),
        )
        .await
        .unwrap();

        assert_eq!(result.method, DeliveryMethod::Clipboard);
        assert!(!wtype.with_extension("started").exists());
        assert_eq!(
            fs::read_to_string(wl_copy.with_extension("stdin")).unwrap(),
            "Shirley Bennett"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn timed_out_child_is_killed_and_reaped() {
        let directory = test_directory("timeout");
        let program = fake_program(
            &directory,
            "wtype",
            "printf '%s' \"$$\" > \"${0}.pid\"\ncat >/dev/null\nwhile :; do :; done",
        );

        let error = type_text(
            program.as_os_str(),
            "Annie Edison",
            &test_environment(&directory),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();

        assert!(format!("{error:#}").contains("timed out"));
        let pid = fs::read_to_string(program.with_extension("pid"))
            .unwrap()
            .parse::<i32>()
            .unwrap();
        let result = unsafe { libc::kill(pid, 0) };
        assert_eq!(result, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn stderr_drain_is_bounded_when_a_descendant_keeps_the_pipe_open() {
        let directory = test_directory("stderr-descendant");
        let program = fake_program(
            &directory,
            "wtype",
            "printf '%s' \"$$\" > \"${0}.pid\"\nsleep 30 &\nprintf '%s' \"$!\" > \"${0}.descendant\"\nexit 0",
        );
        let started = std::time::Instant::now();

        let error = type_text(
            program.as_os_str(),
            "Annie Edison",
            &test_environment(&directory),
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();

        assert!(format!("{error:#}").contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
        let child_pid = fs::read_to_string(program.with_extension("pid"))
            .unwrap()
            .parse::<i32>()
            .unwrap();
        // SAFETY: signal 0 only checks whether the recorded process still exists.
        assert_eq!(unsafe { libc::kill(child_pid, 0) }, -1);
        let descendant_pid = fs::read_to_string(program.with_extension("descendant"))
            .unwrap()
            .parse::<i32>()
            .unwrap();
        for _ in 0..100 {
            // SAFETY: signal 0 only checks whether the recorded process still exists.
            if unsafe { libc::kill(descendant_pid, 0) } == -1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // SAFETY: signal 0 only checks whether the recorded process still exists.
        assert_eq!(unsafe { libc::kill(descendant_pid, 0) }, -1);
        fs::remove_dir_all(directory).unwrap();
    }
}
