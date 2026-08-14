use std::ffi::OsString;

use clap::{
    Arg, Args, Command as ClapCommand, CommandFactory, FromArgMatches, Parser, Subcommand,
    ValueEnum,
};
use hsin_core::{AuthScheme, ClientKind, ConnectionMode};

use crate::i18n::I18n;

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
    /// Update hsin to the latest release.
    Update,
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
    /// Inspect or change daemon settings.
    Settings {
        #[command(subcommand)]
        command: SettingsCommand,
    },
    /// Resolve a credential for Codex or Claude Code (internal).
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
    /// List configured providers.
    List(ClientOption),
    /// Add a provider and read its API key from standard input.
    Add(ProviderAdd),
    /// Edit an existing provider.
    Edit(ProviderEdit),
    /// Remove an existing provider.
    Remove(ProviderId),
    /// Switch the active provider for a client.
    Switch(ProviderSwitch),
    /// Import the provider currently configured by a client.
    ImportCurrent(ImportCurrent),
}

#[derive(Debug, Args)]
pub struct ClientOption {
    /// Filter providers by client.
    #[arg(long, value_enum)]
    pub client: Option<ClientArg>,
}

#[derive(Debug, Args)]
pub struct ImportCurrent {
    /// Client configuration to import.
    #[arg(long, value_enum)]
    pub client: ClientArg,
    /// Name used when a new provider is created.
    #[arg(long, default_value = "Imported")]
    pub name: String,
}

#[derive(Debug, Args)]
pub struct ProviderId {
    /// Provider ID.
    pub id: String,
}

#[derive(Debug, Args)]
pub struct ProviderSwitch {
    /// Client whose active provider will change.
    #[arg(value_enum)]
    pub client: ClientArg,
    /// Provider ID to activate.
    pub id: String,
}

#[derive(Debug, Args)]
pub struct ProviderAdd {
    /// Client that will use the provider.
    #[arg(value_enum)]
    pub client: ClientArg,
    /// Display name; the endpoint domain is used when omitted.
    #[arg(long, default_value = "")]
    pub name: String,
    /// Optional provider description.
    #[arg(long, default_value = "")]
    pub description: String,
    /// Provider API base URL.
    #[arg(long)]
    pub base_url: String,
    /// Codex model to write when this provider is selected.
    #[arg(long)]
    pub model: Option<String>,
    /// Upstream authentication scheme.
    #[arg(long, value_enum)]
    pub auth_scheme: Option<AuthArg>,
    /// Read the API key from standard input. This avoids exposing it in process arguments.
    #[arg(long)]
    pub secret_stdin: bool,
}

#[derive(Debug, Args)]
pub struct ProviderEdit {
    /// Provider ID.
    pub id: String,
    /// New display name.
    #[arg(long)]
    pub name: Option<String>,
    /// New provider description.
    #[arg(long)]
    pub description: Option<String>,
    /// New API base URL.
    #[arg(long)]
    pub base_url: Option<String>,
    /// New Codex model; omit to preserve the current value.
    #[arg(long)]
    pub model: Option<String>,
    /// New upstream authentication scheme.
    #[arg(long, value_enum)]
    pub auth_scheme: Option<AuthArg>,
    /// Replace the API key with a value read from standard input.
    #[arg(long)]
    pub secret_stdin: bool,
}

#[derive(Debug, Subcommand)]
pub enum ModeCommand {
    /// Set direct or local-proxy mode for a client.
    Set {
        /// Client whose connection mode will change.
        #[arg(value_enum)]
        client: ClientArg,
        /// Connection mode to activate.
        #[arg(value_enum)]
        mode: ModeArg,
    },
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Show background service status.
    Status,
    /// Start the background service.
    Start,
    /// Stop the background service.
    Stop,
    /// Restart the background service.
    Restart,
    /// Install the background service.
    Install {
        /// Start the service immediately after installation.
        #[arg(long)]
        start: bool,
    },
    /// Uninstall the background service.
    Uninstall {
        /// Also remove hsin state, backups, and keyring entries.
        #[arg(long)]
        purge: bool,
    },
    /// Reinstall the service using the hsind binary next to this CLI.
    Update,
}

#[derive(Debug, Subcommand)]
pub enum SettingsCommand {
    /// Show daemon settings.
    Get,
    /// Change daemon settings.
    Set {
        /// Address the local proxy listens on.
        #[arg(long, value_name = "HOST")]
        proxy_host: Option<String>,
        /// Port the local proxy listens on.
        #[arg(long, value_name = "PORT")]
        proxy_port: Option<u16>,
        /// Whether the local proxy listener runs.
        #[arg(long, value_name = "BOOL", action = clap::ArgAction::Set)]
        proxy_enabled: Option<bool>,
    },
}

#[derive(Debug, Subcommand)]
pub enum SecurityCommand {
    /// Show encryption and key-store status.
    Status,
    /// Print the recovery key.
    ExportRecoveryKey,
    /// Import a recovery key.
    ImportRecoveryKey {
        /// Read the recovery key from standard input.
        #[arg(long, default_value_t = true)]
        stdin: bool,
    },
    /// Rotate the encryption key and re-encrypt provider credentials.
    RotateKey,
}

impl Cli {
    pub fn parse_localized_from<I, T>(args: I, i18n: &I18n) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let matches = localized_command(i18n).get_matches_from(args);
        Self::from_arg_matches(&matches).unwrap_or_else(|error| error.exit())
    }
}

pub fn language_override(args: &[OsString]) -> Option<String> {
    let mut arguments = args.iter().skip(1);
    while let Some(argument) = arguments.next() {
        let argument = argument.to_string_lossy();
        if argument == "--" {
            break;
        }
        if let Some(language) = argument.strip_prefix("--language=") {
            return (!language.is_empty()).then(|| language.to_owned());
        }
        if argument == "--language"
            && let Some(language) = arguments.next()
        {
            let language = language.to_string_lossy();
            if !language.starts_with('-') && !language.is_empty() {
                return Some(language.into_owned());
            }
        }
    }
    std::env::var("HSIN_LANGUAGE")
        .ok()
        .filter(|language| !language.is_empty())
}

fn localized_command(i18n: &I18n) -> ClapCommand {
    let mut command = <Cli as CommandFactory>::command();
    command.build();
    localize_command(command, "", i18n)
}

fn localize_command(mut command: ClapCommand, path: &str, i18n: &I18n) -> ClapCommand {
    let name = command.get_name().to_owned();
    let command_path = if name == "help" {
        path.to_owned()
    } else if path.is_empty() {
        name.clone()
    } else {
        format!("{path}.{name}")
    };
    let about_key = if name == "hsin" {
        "cli.about".to_owned()
    } else if name == "help" {
        "cli.help.about".to_owned()
    } else {
        format!("cli.{command_path}.about")
    };
    if let Some(about) = i18n.get(&about_key) {
        command = command.about(about.to_owned());
    }

    let template = format!(
        "{{before-help}}{{about-with-newline}}\n{}: {{usage}}\n\n{{all-args}}{{after-help}}",
        i18n.text("cli.heading.usage")
    );
    command = command
        .help_template(template)
        .subcommand_help_heading(i18n.text("cli.heading.commands").to_owned())
        .subcommand_value_name(i18n.text("cli.value.command").to_owned());

    let argument_path = if name == "hsin" { "" } else { &command_path };
    command = command.mut_args(|argument| localize_argument(argument, argument_path, i18n));
    command.mut_subcommands(|subcommand| {
        let subcommand_is_help = subcommand.get_name() == "help";
        let child_path = if subcommand_is_help {
            path
        } else {
            argument_path
        };
        localize_command(subcommand, child_path, i18n)
    })
}

fn localize_argument(mut argument: Arg, path: &str, i18n: &I18n) -> Arg {
    let id = argument.get_id().as_str();
    let positional = argument.is_positional();
    let specific_key = if path.is_empty() {
        format!("cli.arg.{id}")
    } else {
        format!("cli.{path}.arg.{id}")
    };
    let generic_key = format!("cli.arg.{id}");
    if let Some(help) = i18n.get(&specific_key).or_else(|| i18n.get(&generic_key)) {
        argument = argument.help(help.to_owned()).long_help(None);
    }
    argument.help_heading(
        i18n.text(if positional {
            "cli.heading.arguments"
        } else {
            "cli.heading.options"
        })
        .to_owned(),
    )
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
    use clap::{CommandFactory, error::ErrorKind};

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

    #[test]
    fn parses_settings_set_for_a_remote_listener() {
        // Reaching the proxy from another host needs a wildcard bind and a free
        // port. Neither was reachable from the CLI before, only from the TUI.
        let cli = Cli::try_parse_from([
            "hsin",
            "settings",
            "set",
            "--proxy-host",
            "0.0.0.0",
            "--proxy-port",
            "9998",
            "--proxy-enabled",
            "true",
        ])
        .expect("valid command");
        let Some(Command::Settings {
            command:
                SettingsCommand::Set {
                    proxy_host,
                    proxy_port,
                    proxy_enabled,
                },
        }) = cli.command
        else {
            panic!("expected a settings set command");
        };
        assert_eq!(proxy_host.as_deref(), Some("0.0.0.0"));
        assert_eq!(proxy_port, Some(9998));
        assert_eq!(proxy_enabled, Some(true));
    }

    #[test]
    fn parses_top_level_update() {
        let cli = Cli::try_parse_from(["hsin", "update"]).expect("valid command");
        assert!(matches!(cli.command, Some(Command::Update)));
    }

    #[test]
    fn settings_help_is_localized() {
        // A missing locale key silently falls back to the English doc comment,
        // which is only visible by reading the translated help.
        let error = localized_command(&I18n::new(Some("zh-CN")))
            .try_get_matches_from(["hsin", "settings", "set", "--help"])
            .expect_err("help should stop argument parsing");
        let help = error.to_string();
        for expected in ["修改 daemon 设置", "本地代理监听地址", "本地代理监听端口"]
        {
            assert!(help.contains(expected), "missing {expected:?} in:\n{help}");
        }
        assert!(!help.contains("Port the local proxy"));
    }

    #[test]
    fn provider_help_localizes_headings_and_every_subcommand() {
        let error = localized_command(&I18n::new(Some("zh-CN")))
            .try_get_matches_from(["hsin", "provider", "--help"])
            .expect_err("help should stop argument parsing");
        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        let help = error.to_string();
        for expected in [
            "管理上游 Provider",
            "用法:",
            "命令:",
            "list            列出已配置的 Provider",
            "add             添加 Provider",
            "edit            编辑已有 Provider",
            "remove          删除已有 Provider",
            "switch          切换客户端当前使用的 Provider",
            "import-current  导入客户端当前配置的 Provider",
            "help            显示当前帮助",
            "选项:",
        ] {
            assert!(help.contains(expected), "missing {expected:?} in:\n{help}");
        }
        assert!(!help.contains("Print help"));
    }

    #[test]
    fn nested_help_localizes_argument_descriptions() {
        let error = localized_command(&I18n::new(Some("en-US")))
            .try_get_matches_from(["hsin", "provider", "add", "--help"])
            .expect_err("help should stop argument parsing");
        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        let help = error.to_string();
        for expected in [
            "Client that will use the provider",
            "Provider API base URL",
            "Upstream authentication scheme",
            "Read the API key from standard input instead of process arguments",
        ] {
            assert!(help.contains(expected), "missing {expected:?} in:\n{help}");
        }
    }

    #[test]
    fn language_override_accepts_global_option_forms() {
        assert_eq!(
            language_override(&[
                "hsin".into(),
                "provider".into(),
                "--language".into(),
                "zh-CN".into(),
                "--help".into(),
            ]),
            Some("zh-CN".into())
        );
        assert_eq!(
            language_override(&["hsin".into(), "--language=en-US".into(), "status".into(),]),
            Some("en-US".into())
        );
    }
}
