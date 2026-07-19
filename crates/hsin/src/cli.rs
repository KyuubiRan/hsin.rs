use clap::{Args, Parser, Subcommand, ValueEnum};
use hsin_core::{AuthScheme, ClientKind, ConnectionMode};

#[derive(Debug, Parser)]
#[command(name = "hsin", version, about = "心 / HSIN provider switcher")]
pub struct Cli {
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,

    /// UI language (system, en-US, or zh-CN).
    #[arg(long, global = true, env = "HSIN_LANGUAGE")]
    pub language: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage upstream providers.
    Provider {
        #[command(subcommand)]
        command: ProviderCommand,
    },
    /// Change direct/proxy connection mode.
    Mode {
        #[command(subcommand)]
        command: ModeCommand,
    },
    /// Show daemon and client state.
    Status,
    /// Run diagnostics.
    Doctor,
    /// Manage the background daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Manage encryption and recovery keys.
    Security {
        #[command(subcommand)]
        command: SecurityCommand,
    },
    /// Resolve a credential for Codex or Claude Code (internal).
    #[command(hide = true)]
    Credential {
        #[arg(value_enum)]
        client: ClientArg,
        #[arg(long, hide = true)]
        provider_id: Option<String>,
        #[arg(long, hide = true)]
        revision: Option<u64>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    List(ClientOption),
    Add(ProviderAdd),
    Edit(ProviderEdit),
    Remove(ProviderId),
    Switch(ProviderSwitch),
    ImportCurrent(ImportCurrent),
}

#[derive(Debug, Args)]
pub struct ClientOption {
    #[arg(long, value_enum)]
    pub client: Option<ClientArg>,
}

#[derive(Debug, Args)]
pub struct ImportCurrent {
    #[arg(long, value_enum)]
    pub client: ClientArg,
    #[arg(long, default_value = "Imported")]
    pub name: String,
}

#[derive(Debug, Args)]
pub struct ProviderId {
    pub id: String,
}

#[derive(Debug, Args)]
pub struct ProviderSwitch {
    #[arg(value_enum)]
    pub client: ClientArg,
    pub id: String,
}

#[derive(Debug, Args)]
pub struct ProviderAdd {
    #[arg(value_enum)]
    pub client: ClientArg,
    #[arg(long, default_value = "")]
    pub name: String,
    #[arg(long, default_value = "")]
    pub description: String,
    #[arg(long)]
    pub base_url: String,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long, value_enum)]
    pub auth_scheme: Option<AuthArg>,
    /// Read the API key from standard input. This avoids exposing it in process arguments.
    #[arg(long)]
    pub secret_stdin: bool,
}

#[derive(Debug, Args)]
pub struct ProviderEdit {
    pub id: String,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub description: Option<String>,
    #[arg(long)]
    pub base_url: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long, value_enum)]
    pub auth_scheme: Option<AuthArg>,
    /// Replace the API key with a value read from standard input.
    #[arg(long)]
    pub secret_stdin: bool,
}

#[derive(Debug, Subcommand)]
pub enum ModeCommand {
    Set {
        #[arg(value_enum)]
        client: ClientArg,
        #[arg(value_enum)]
        mode: ModeArg,
    },
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    Status,
    Start,
    Stop,
    Restart,
    Install {
        #[arg(long)]
        start: bool,
    },
    Uninstall {
        #[arg(long)]
        purge: bool,
    },
    Update,
}

#[derive(Debug, Subcommand)]
pub enum SecurityCommand {
    Status,
    ExportRecoveryKey,
    ImportRecoveryKey {
        /// Read the recovery key from standard input.
        #[arg(long, default_value_t = true)]
        stdin: bool,
    },
    RotateKey,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ClientArg {
    Codex,
    Claude,
}

impl From<ClientArg> for ClientKind {
    fn from(value: ClientArg) -> Self {
        match value {
            ClientArg::Codex => Self::Codex,
            ClientArg::Claude => Self::Claude,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ModeArg {
    Direct,
    Proxy,
}

impl From<ModeArg> for ConnectionMode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::Direct => Self::Direct,
            ModeArg::Proxy => Self::Proxy,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum AuthArg {
    Bearer,
    XApiKey,
}

impl From<AuthArg> for AuthScheme {
    fn from(value: AuthArg) -> Self {
        match value {
            AuthArg::Bearer => Self::Bearer,
            AuthArg::XApiKey => Self::XApiKey,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn command_tree_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_provider_switch() {
        let cli = Cli::try_parse_from(["hsin", "provider", "switch", "codex", "p1"])
            .expect("valid command");
        assert!(matches!(
            cli.command,
            Some(Command::Provider {
                command: ProviderCommand::Switch(_)
            })
        ));
    }
}
