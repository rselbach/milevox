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

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Result, bail};
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
}

#[derive(Debug, Subcommand)]
enum SettingsCommand {
    /// Print the active settings and token source as JSON.
    Show,
    /// Change one or more post-processing settings.
    Set(SettingsSetArgs),
    /// List curated models, optionally selecting a provider.
    Models {
        #[arg(long)]
        provider: Option<ProviderArg>,
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
            let config = Config::load(&config_path)?;
            post_processing::validate(&config.post_processing)?;
            daemon::run(config, config_path).await
        }
        CliCommand::Record { command } => {
            let command = match command {
                RecordCommand::Start => Command::Start,
                RecordCommand::Stop => Command::StopAndWait,
                RecordCommand::Toggle => Command::Toggle,
                RecordCommand::Cancel => Command::Cancel,
            };
            run_client(command).await
        }
        CliCommand::Status { follow } => run_client(Command::Status { follow }).await,
        CliCommand::Settings { command } => {
            let command = match command {
                SettingsCommand::Show => Command::Settings {
                    enabled: None,
                    provider: None,
                    model: None,
                },
                SettingsCommand::Set(SettingsSetArgs {
                    enabled,
                    provider,
                    model,
                }) => Command::Settings {
                    enabled,
                    provider: provider.map(Into::into),
                    model,
                },
                SettingsCommand::Models { provider } => Command::SettingsModels {
                    provider: provider.map(Into::into),
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
    }
}

fn read_token() -> Result<String> {
    if std::io::stdin().is_terminal() {
        eprint!("Provider token: ");
        std::io::stderr().flush()?;
        return Ok(rpassword::read_password()?);
    }

    let mut token = String::new();
    std::io::stdin().lock().read_line(&mut token)?;
    Ok(token.trim_end_matches(['\r', '\n']).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

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
}
