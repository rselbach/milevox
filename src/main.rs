mod audio;
mod config;
mod credentials;
mod daemon;
mod ipc;
mod output;
mod paths;
mod post_processing;
mod private_file;
mod transcription;

use std::io::{BufRead, IsTerminal, Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};

use crate::config::{Config, PostProcessingProvider};
use crate::ipc::{Command, run_client};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Use this configuration file when starting the daemon only.
    #[arg(long)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Run the background recording service.
    Daemon,
    /// Control a recording or active processing operation.
    Record {
        #[command(subcommand)]
        command: RecordCommand,
    },
    /// Print the current daemon state as JSON.
    Status {
        /// Continue printing state changes until interrupted.
        #[arg(long)]
        follow: bool,
        /// Include compact audio-level events while following.
        #[arg(long, requires = "follow")]
        levels: bool,
    },
    /// Inspect or change post-processing settings.
    Settings {
        #[command(subcommand)]
        command: SettingsCommand,
    },
    /// Inspect or configure transcript diagnostics.
    Debug {
        #[command(subcommand)]
        command: DebugCommand,
    },
    #[command(name = "__transcription-worker", hide = true)]
    TranscriptionWorker {
        #[arg(long)]
        model_path: PathBuf,
        #[arg(long, hide = true)]
        fake: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Subcommand)]
enum RecordCommand {
    /// Begin microphone capture.
    Start,
    /// Stop capture and wait for transcription, refinement, and delivery.
    Stop,
    /// Start while idle, stop while recording, or cancel while processing.
    Toggle,
    /// Discard the current recording or in-progress processing.
    Cancel,
    /// Copy the latest final transcript to the clipboard.
    Copy,
}

#[derive(Debug, Subcommand)]
enum SettingsCommand {
    /// Print the active settings and token source.
    Show {
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Change one or more post-processing settings.
    Set(SettingsSetArgs),
    /// List curated models, optionally selecting a provider.
    Models {
        #[arg(long)]
        provider: Option<ProviderArg>,
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Store or remove a provider token.
    Token {
        #[command(subcommand)]
        command: Option<TokenCommand>,
        /// Use this provider instead of the active provider.
        #[arg(long, global = true)]
        provider: Option<ProviderArg>,
    },
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("changes")
        .required(true)
        .multiple(true)
        .args(["enabled", "provider", "model"])
))]
struct SettingsSetArgs {
    /// Enable or disable post-processing.
    #[arg(long)]
    enabled: Option<bool>,
    /// Select the post-processing provider.
    #[arg(long)]
    provider: Option<ProviderArg>,
    /// Select a curated model for the active provider.
    #[arg(long)]
    model: Option<String>,
}

#[derive(Debug, Clone, Copy, Subcommand)]
enum TokenCommand {
    /// Remove a token stored by Milevox without changing environment tokens.
    Remove,
}

#[derive(Debug, Clone, Copy, Subcommand)]
enum DebugCommand {
    /// Enable persistent transcript debug logging.
    Enable,
    /// Disable persistent transcript debug logging.
    Disable,
    /// Print diagnostics for the most recent transcription attempt.
    Last,
    /// Remove the persistent debug log and its rotated backup.
    Clear,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProviderArg {
    Openrouter,
    #[value(name = "opencode_zen")]
    OpencodeZen,
}

impl From<ProviderArg> for PostProcessingProvider {
    fn from(provider: ProviderArg) -> Self {
        match provider {
            ProviderArg::Openrouter => Self::Openrouter,
            ProviderArg::OpencodeZen => Self::OpencodeZen,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let is_daemon = matches!(cli.command, CliCommand::Daemon);
    if cli.config.is_some() && !is_daemon {
        bail!("--config controls daemon startup only and cannot be used with client commands");
    }

    match cli.command {
        CliCommand::Daemon => {
            let config_path = cli.config.unwrap_or_else(paths::config_path);
            let config = load_daemon_config(&config_path)?;
            daemon::run(config, config_path).await
        }
        CliCommand::Record { command } => {
            let command = match command {
                RecordCommand::Start => Command::Start {
                    output_target: output::capture_output_target().await,
                },
                RecordCommand::Stop => Command::Stop,
                RecordCommand::Toggle => Command::Toggle {
                    output_target: output::capture_output_target().await,
                },
                RecordCommand::Cancel => Command::Cancel,
                RecordCommand::Copy => Command::CopyLast,
            };
            run_client(command).await
        }
        CliCommand::Status { follow, levels } => {
            run_client(Command::Status { follow, levels }).await
        }
        CliCommand::Settings { command } => {
            let command = match command {
                SettingsCommand::Show { json } => Command::Settings {
                    enabled: None,
                    provider: None,
                    model: None,
                    json,
                },
                SettingsCommand::Set(SettingsSetArgs {
                    enabled,
                    provider,
                    model,
                }) => Command::Settings {
                    enabled,
                    provider: provider.map(Into::into),
                    model,
                    json: false,
                },
                SettingsCommand::Models { provider, json } => Command::SettingsModels {
                    provider: provider.map(Into::into),
                    json,
                },
                SettingsCommand::Token { command, provider } => match command {
                    Some(TokenCommand::Remove) => Command::RemoveToken {
                        provider: provider.map(Into::into),
                    },
                    None => Command::SetToken {
                        provider: provider.map(Into::into),
                        token: read_token()?,
                    },
                },
            };
            run_client(command).await
        }
        CliCommand::Debug { command } => {
            let command = match command {
                DebugCommand::Enable => Command::Debug { enabled: true },
                DebugCommand::Disable => Command::Debug { enabled: false },
                DebugCommand::Last => Command::DebugLast,
                DebugCommand::Clear => Command::DebugClear,
            };
            run_client(command).await
        }
        CliCommand::TranscriptionWorker { model_path, fake } => {
            transcription::run_worker_mode(&model_path, fake.as_deref())
        }
    }
}

fn load_daemon_config(path: &Path) -> Result<Config> {
    let config = Config::load(path)?;
    post_processing::validate(&config.post_processing)?;
    Ok(config)
}

fn read_token() -> Result<String> {
    if std::io::stdin().is_terminal() {
        #[cfg(unix)]
        {
            let stdin = std::io::stdin();
            let descriptor = stdin.as_raw_fd();
            let mut reader = stdin.lock();
            return with_terminal_echo_disabled(descriptor, || {
                eprint!("Provider token: ");
                std::io::stderr().flush()?;
                let result = read_token_from(&mut reader);
                eprintln!();
                result
            });
        }
        #[cfg(not(unix))]
        bail!("secure terminal token input is not supported on this platform");
    }

    read_token_from(&mut std::io::stdin().lock())
}

#[cfg(unix)]
struct TerminalEchoGuard {
    descriptor: RawFd,
    original: libc::termios,
    active: bool,
}

#[cfg(unix)]
impl TerminalEchoGuard {
    fn disable(descriptor: RawFd) -> Result<Self> {
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `original` points to writable termios storage and `descriptor` is borrowed.
        if unsafe { libc::tcgetattr(descriptor, original.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to inspect terminal settings");
        }
        // SAFETY: tcgetattr initialized `original` after returning success.
        let original = unsafe { original.assume_init() };
        let mut hidden = original;
        hidden.c_lflag &= !(libc::ECHO | libc::ECHONL);
        // SAFETY: `hidden` is initialized termios state for the borrowed descriptor.
        if unsafe { libc::tcsetattr(descriptor, libc::TCSANOW, &raw const hidden) } != 0 {
            return Err(std::io::Error::last_os_error()).context("failed to disable terminal echo");
        }
        Ok(Self {
            descriptor,
            original,
            active: true,
        })
    }

    fn restore(mut self) -> Result<()> {
        // SAFETY: `original` came from this descriptor and remains initialized.
        if unsafe { libc::tcsetattr(self.descriptor, libc::TCSANOW, &raw const self.original) } != 0
        {
            return Err(std::io::Error::last_os_error()).context("failed to restore terminal echo");
        }
        self.active = false;
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for TerminalEchoGuard {
    fn drop(&mut self) {
        if self.active {
            // SAFETY: best-effort restoration uses termios state captured from this descriptor.
            unsafe {
                libc::tcsetattr(self.descriptor, libc::TCSANOW, &raw const self.original);
            }
        }
    }
}

#[cfg(unix)]
fn with_terminal_echo_disabled<T>(
    descriptor: RawFd,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let guard = TerminalEchoGuard::disable(descriptor)?;
    let result = operation();
    let restore = guard.restore();
    match (result, restore) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(restore_error)) => Err(error).context(format!(
            "terminal echo restoration also failed: {restore_error:#}"
        )),
    }
}

fn read_token_from(reader: &mut impl BufRead) -> Result<String> {
    let mut bytes = Vec::new();
    reader
        .take((credentials::MAX_TOKEN_BYTES + 1) as u64)
        .read_until(b'\n', &mut bytes)?;
    let has_line_feed = bytes.last() == Some(&b'\n');
    if !has_line_feed && bytes.len() > credentials::MAX_TOKEN_BYTES {
        bail!("token is too long");
    }
    if has_line_feed {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    let token = String::from_utf8(bytes).context("provider token is not valid UTF-8")?;
    credentials::validate_token(&token)?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::fs::File;
    #[cfg(unix)]
    use std::io::BufReader;
    use std::io::Cursor;
    #[cfg(unix)]
    use std::os::fd::FromRawFd;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use clap::CommandFactory;

    static NEXT_CONFIG_ID: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn complete_help_tree_is_valid() {
        Cli::command().debug_assert();
        for arguments in [
            ["milevox", "record", "--help"].as_slice(),
            ["milevox", "settings", "--help"].as_slice(),
            ["milevox", "settings", "token", "--help"].as_slice(),
            ["milevox", "debug", "--help"].as_slice(),
        ] {
            let error = Cli::try_parse_from(arguments).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        }
    }

    #[test]
    fn settings_set_requires_a_change() {
        assert!(Cli::try_parse_from(["milevox", "settings", "set"]).is_err());
    }

    #[test]
    fn config_is_parsed_for_a_client_so_main_can_reject_it() {
        let cli = Cli::try_parse_from(["milevox", "--config", "other.toml", "status"]).unwrap();
        assert!(cli.config.is_some());
        assert!(matches!(cli.command, CliCommand::Status { .. }));
    }

    #[test]
    fn daemon_startup_accepts_a_disabled_retired_post_processing_model() {
        let (directory, path) = config_path("disabled-retired-model");
        let mut config = Config::default();
        config.post_processing.model = Some("retired-model".to_owned());
        config.save(&path).unwrap();

        let loaded = load_daemon_config(&path).unwrap();

        assert!(!loaded.post_processing.enabled);
        assert_eq!(
            loaded.post_processing.model.as_deref(),
            Some("retired-model")
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn daemon_startup_rejects_an_enabled_retired_post_processing_model() {
        let (directory, path) = config_path("enabled-retired-model");
        let mut config = Config::default();
        config.post_processing.enabled = true;
        config.post_processing.model = Some("retired-model".to_owned());
        config.save(&path).unwrap();

        let error = load_daemon_config(&path).unwrap_err();

        assert!(error.to_string().contains("valid models"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn config_path(name: &str) -> (PathBuf, PathBuf) {
        let id = NEXT_CONFIG_ID.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("milevox-main-{name}-{}-{id}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("config.toml");
        (directory, path)
    }

    #[test]
    fn provider_token_input_enforces_the_byte_limit_before_ipc() {
        let exact = "x".repeat(credentials::MAX_TOKEN_BYTES);
        for input in [exact.clone(), format!("{exact}\n")] {
            assert_eq!(read_token_from(&mut Cursor::new(input)).unwrap(), exact);
        }

        let oversized = "x".repeat(credentials::MAX_TOKEN_BYTES + 1);
        for input in [
            oversized.clone(),
            format!("{oversized}\n"),
            format!("{exact}\rX"),
        ] {
            let error = read_token_from(&mut Cursor::new(input)).unwrap_err();
            assert!(error.to_string().contains("too long"));
        }

        let mut windows_line_at_token_limit = Cursor::new(format!("{exact}\r\n"));
        let error = read_token_from(&mut windows_line_at_token_limit).unwrap_err();
        assert!(error.to_string().contains("too long"));
        assert_eq!(
            windows_line_at_token_limit.position(),
            (credentials::MAX_TOKEN_BYTES + 1) as u64
        );
    }

    #[test]
    fn provider_token_input_accepts_line_endings_and_preserves_validation() {
        for input in [
            "greendale-token",
            "greendale-token\n",
            "greendale-token\r\n",
        ] {
            assert_eq!(
                read_token_from(&mut Cursor::new(input)).unwrap(),
                "greendale-token"
            );
        }
        for input in [" greendale-token\n", "greendale-token \n", "green\tale\n"] {
            assert!(read_token_from(&mut Cursor::new(input)).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn terminal_token_input_disables_echo_and_restores_termios() {
        let mut master_descriptor = -1;
        let mut slave_descriptor = -1;
        // SAFETY: openpty receives valid descriptor pointers and no optional buffers.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &raw mut master_descriptor,
                    &raw mut slave_descriptor,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        // SAFETY: openpty returned owned descriptors on success.
        let mut master = unsafe { File::from_raw_fd(master_descriptor) };
        // SAFETY: openpty returned owned descriptors on success.
        let slave = unsafe { File::from_raw_fd(slave_descriptor) };
        let observer = slave.try_clone().unwrap();
        let original = terminal_settings(observer.as_raw_fd());

        let reader = std::thread::spawn(move || {
            let descriptor = slave.as_raw_fd();
            let mut reader = BufReader::new(slave);
            with_terminal_echo_disabled(descriptor, || read_token_from(&mut reader))
        });
        for _ in 0..10_000 {
            if terminal_settings(observer.as_raw_fd()).c_lflag & libc::ECHO == 0 {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(
            terminal_settings(observer.as_raw_fd()).c_lflag & libc::ECHO,
            0
        );

        master.write_all(b"greendale-token\n").unwrap();
        assert_eq!(reader.join().unwrap().unwrap(), "greendale-token");
        assert_eq!(
            terminal_settings(observer.as_raw_fd()).c_lflag,
            original.c_lflag
        );

        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_ne!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK,) },
            -1
        );
        let mut echoed = [0_u8; 64];
        let error = master.read(&mut echoed).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);

        let error = with_terminal_echo_disabled(observer.as_raw_fd(), || -> Result<()> {
            bail!("simulated terminal read failure")
        })
        .unwrap_err();
        assert!(error.to_string().contains("simulated"));
        assert_eq!(
            terminal_settings(observer.as_raw_fd()).c_lflag,
            original.c_lflag
        );
    }

    #[cfg(unix)]
    fn terminal_settings(descriptor: RawFd) -> libc::termios {
        let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `settings` points to writable termios storage for a pseudo-terminal.
        assert_eq!(
            unsafe { libc::tcgetattr(descriptor, settings.as_mut_ptr()) },
            0
        );
        // SAFETY: tcgetattr initialized `settings` after returning success.
        unsafe { settings.assume_init() }
    }
}
