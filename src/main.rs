mod audio;
mod config;
mod credentials;
mod daemon;
mod ipc;
mod output;
mod paths;
mod post_processing;
mod transcription;

use std::io::BufRead;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

use crate::config::{Config, PostProcessingProvider};
use crate::ipc::{Command, run_client};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Use a configuration file other than the XDG default.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Run the background recording service.
    Daemon,
    /// Control a recording.
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
    /// Enable or disable transcript debug logging.
    Debug {
        #[command(subcommand)]
        command: DebugCommand,
    },
}

#[derive(Debug, Clone, Copy, Subcommand)]
enum RecordCommand {
    Start,
    Stop,
    Toggle,
    Cancel,
}

#[derive(Debug, Subcommand)]
enum SettingsCommand {
    /// Print the active settings as JSON.
    Show,
    /// Change one or more post-processing settings.
    Set {
        /// Enable or disable post-processing.
        #[arg(long)]
        enabled: Option<bool>,
        /// Select the post-processing provider.
        #[arg(long)]
        provider: Option<ProviderArg>,
        /// Select a curated model for the active provider.
        #[arg(long)]
        model: Option<String>,
    },
    /// Save a token read from standard input.
    Token {
        /// Save the token for this provider instead of the active provider.
        #[arg(long)]
        provider: Option<ProviderArg>,
    },
}

#[derive(Debug, Clone, Copy, Subcommand)]
enum DebugCommand {
    /// Enable transcript debug logging.
    Enable,
    /// Disable transcript debug logging.
    Disable,
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
    let config_path = cli.config.unwrap_or_else(paths::config_path);

    match cli.command {
        CliCommand::Daemon => {
            let config = Config::load(&config_path)?;
            daemon::run(config, config_path).await
        }
        CliCommand::Record { command } => {
            let command = match command {
                RecordCommand::Start => Command::Start,
                RecordCommand::Stop => Command::Stop,
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
                SettingsCommand::Set {
                    enabled,
                    provider,
                    model,
                } => Command::Settings {
                    enabled,
                    provider: provider.map(Into::into),
                    model,
                },
                SettingsCommand::Token { provider } => {
                    let mut token = String::new();
                    std::io::stdin().lock().read_line(&mut token)?;
                    let token = token.trim_end_matches(['\r', '\n']).to_owned();
                    Command::SetToken {
                        provider: provider.map(Into::into),
                        token,
                    }
                }
            };
            run_client(command).await
        }
        CliCommand::Debug { command } => {
            let enabled = matches!(command, DebugCommand::Enable);
            run_client(Command::Debug { enabled }).await
        }
    }
}
