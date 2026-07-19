use std::io::{self, Read, Write};

use anyhow::{Context, Result, bail};
use hsin_core::{
    ClientKind, ImportCurrentParams, ModeSetParams, ModelUpdate, Provider, ProviderAddParams,
    ProviderDraft, ProviderEditParams, ProviderPatch, ProviderRemoveParams, ProviderSwitchParams,
    SecretInput,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    bootstrap,
    cli::{Cli, Command, DaemonCommand, ModeCommand, ProviderCommand, SecurityCommand},
    i18n::I18n,
    rpc::DaemonClient,
    tui,
};

pub async fn run(cli: Cli, i18n: &mut I18n) -> Result<()> {
    let language_overridden = cli.language.is_some();
    let Some(command) = cli.command else {
        let client = DaemonClient::connect_or_bootstrap().await?;
        synchronize_language(&client, i18n, language_overridden).await;
        return tui::run(client, i18n, !language_overridden).await;
    };

    if let Command::Daemon { command } = command {
        return run_daemon_command(command, cli.json).await;
    }

    let client = DaemonClient::connect_or_bootstrap().await?;
    synchronize_language(&client, i18n, language_overridden).await;
    match command {
        Command::Provider { command } => run_provider(command, &client, cli.json).await,
        Command::Mode { command } => run_mode(command, &client, cli.json).await,
        Command::Status => {
            let value: Value = client.call("status", &json!({})).await?;
            print_value(&value, cli.json)
        }
        Command::Doctor => {
            let value: Value = client.call("doctor", &json!({})).await?;
            print_value(&value, cli.json)
        }
        Command::Security { command } => run_security(command, &client, cli.json).await,
        Command::Credential {
            client: kind,
            provider_id,
            revision,
        } => {
            let value: Value = client
                .call(
                    "credential.resolve",
                    &json!({
                        "client": ClientKind::from(kind),
                        "provider_id": provider_id,
                        "revision": revision,
                    }),
                )
                .await?;
            let secret = value
                .as_str()
                .or_else(|| value.get("secret").and_then(Value::as_str))
                .context("daemon returned an invalid credential response")?;
            let mut stdout = io::stdout().lock();
            stdout.write_all(secret.as_bytes())?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
            Ok(())
        }
        Command::Daemon { .. } => unreachable!("daemon command handled before connecting"),
    }
}

async fn synchronize_language(client: &DaemonClient, i18n: &mut I18n, overridden: bool) {
    if overridden {
        return;
    }
    if let Ok(settings) = client
        .call::<_, hsin_core::Settings>("settings.get", &json!({}))
        .await
    {
        i18n.set_language(&settings.language);
    }
}

async fn run_provider(command: ProviderCommand, client: &DaemonClient, json: bool) -> Result<()> {
    match command {
        ProviderCommand::List(options) => {
            let providers = client.provider_list(options.client.map(Into::into)).await?;
            if json {
                print_json(&providers)
            } else {
                print_provider_table(&providers);
                Ok(())
            }
        }
        ProviderCommand::Add(args) => {
            let secret = read_optional_secret(args.secret_stdin)?;
            let kind = ClientKind::from(args.client);
            let auth_scheme = args.auth_scheme.map_or_else(
                || match kind {
                    ClientKind::Codex => hsin_core::AuthScheme::Bearer,
                    ClientKind::Claude => hsin_core::AuthScheme::XApiKey,
                },
                Into::into,
            );
            let request = ProviderAddParams {
                provider: ProviderDraft {
                    client: kind,
                    name: args.name,
                    description: args.description,
                    base_url: args.base_url,
                    auth_scheme,
                    model: args.model,
                },
                secret: secret.map_or(SecretInput::Clear, SecretInput::Replace),
            };
            let value: Value = client.call("provider.add", &request).await?;
            print_value(&value, json)
        }
        ProviderCommand::Edit(args) => {
            let secret = read_optional_secret(args.secret_stdin)?;
            let provider = find_provider(client, &args.id).await?;
            let request = ProviderEditParams {
                id: args.id,
                expected_revision: provider.revision,
                patch: ProviderPatch {
                    name: args.name,
                    base_url: args.base_url,
                    auth_scheme: args.auth_scheme.map(Into::into),
                    description: args.description,
                    model: args.model.map_or(ModelUpdate::Preserve, ModelUpdate::Set),
                },
                secret: secret.map_or(SecretInput::Preserve, SecretInput::Replace),
            };
            let value: Value = client.call("provider.edit", &request).await?;
            print_value(&value, json)
        }
        ProviderCommand::Remove(args) => {
            let provider = find_provider(client, &args.id).await?;
            let value: Value = client
                .call(
                    "provider.remove",
                    &ProviderRemoveParams {
                        id: args.id,
                        expected_revision: provider.revision,
                    },
                )
                .await?;
            print_value(&value, json)
        }
        ProviderCommand::Switch(args) => {
            let value: Value = client
                .call(
                    "provider.switch",
                    &ProviderSwitchParams {
                        client: args.client.into(),
                        provider_id: args.id,
                    },
                )
                .await?;
            print_value(&value, json)
        }
        ProviderCommand::ImportCurrent(options) => {
            let value: Value = client
                .call(
                    "provider.import_current",
                    &ImportCurrentParams {
                        client: options.client.into(),
                        name: options.name,
                    },
                )
                .await?;
            print_value(&value, json)
        }
    }
}

async fn run_mode(command: ModeCommand, client: &DaemonClient, json: bool) -> Result<()> {
    let ModeCommand::Set {
        client: client_kind,
        mode,
    } = command;
    let value: Value = client
        .call(
            "mode.set",
            &ModeSetParams {
                client: client_kind.into(),
                mode: mode.into(),
            },
        )
        .await?;
    print_value(&value, json)
}

async fn find_provider(client: &DaemonClient, id: &str) -> Result<Provider> {
    client
        .provider_list(None)
        .await?
        .into_iter()
        .find(|provider| provider.id == id)
        .with_context(|| format!("provider {id:?} was not found"))
}

async fn run_security(command: SecurityCommand, client: &DaemonClient, json: bool) -> Result<()> {
    let (method, params, sensitive) = match command {
        SecurityCommand::Status => ("security.status", json!({}), false),
        SecurityCommand::ExportRecoveryKey => ("security.export_recovery_key", json!({}), true),
        SecurityCommand::ImportRecoveryKey { stdin: _ } => (
            "security.import_recovery_key",
            json!({ "recovery_key": read_secret()? }),
            false,
        ),
        SecurityCommand::RotateKey => ("security.rotate_key", json!({}), false),
    };
    let value: Value = client.call(method, &params).await?;
    if sensitive && !json {
        let key = value
            .as_str()
            .or_else(|| value.get("recovery_key").and_then(Value::as_str))
            .context("daemon returned an invalid recovery key")?;
        println!("{key}");
        return Ok(());
    }
    print_value(&value, json)
}

async fn run_daemon_command(command: DaemonCommand, json: bool) -> Result<()> {
    if matches!(command, DaemonCommand::Status) {
        let running = bootstrap::service_status().await?;
        if json {
            return print_json(&json!({ "running": running }));
        }
        println!("{}", if running { "running" } else { "stopped" });
        return Ok(());
    }

    let owned;
    let args: &[&str] = match command {
        DaemonCommand::Status => unreachable!("status handled above"),
        DaemonCommand::Start => &["start"],
        DaemonCommand::Stop => &["stop"],
        DaemonCommand::Restart => &["restart"],
        DaemonCommand::Install { start: true } | DaemonCommand::Update => &["install", "--start"],
        DaemonCommand::Install { start: false } => &["install"],
        DaemonCommand::Uninstall { purge } => {
            owned = if purge {
                vec!["uninstall", "--purge"]
            } else {
                vec!["uninstall"]
            };
            &owned
        }
    };
    bootstrap::run_service(args).await?;
    if json {
        print_json(&json!({ "ok": true }))?;
    }
    Ok(())
}

fn read_optional_secret(enabled: bool) -> Result<Option<String>> {
    enabled.then(read_secret).transpose()
}

fn read_secret() -> Result<String> {
    let mut secret = String::new();
    io::stdin()
        .lock()
        .read_to_string(&mut secret)
        .context("read secret from standard input")?;
    while secret.ends_with(['\n', '\r']) {
        secret.pop();
    }
    if secret.is_empty() {
        bail!("secret read from standard input is empty");
    }
    Ok(secret)
}

fn print_provider_table(providers: &[Provider]) {
    if providers.is_empty() {
        println!("No providers configured");
        return;
    }
    println!("{:<24}  {:<8}  {:<24}  URL", "ID", "CLIENT", "NAME");
    for provider in providers {
        println!(
            "{:<24}  {:<8}  {:<24}  {}",
            provider.id,
            format!("{:?}", provider.client).to_ascii_lowercase(),
            provider.name,
            provider.base_url
        );
    }
}

fn print_value(value: &Value, json: bool) -> Result<()> {
    if json {
        return print_json(value);
    }
    match value {
        Value::Null => Ok(()),
        Value::Bool(true) => {
            println!("ok");
            Ok(())
        }
        Value::String(text) => {
            println!("{text}");
            Ok(())
        }
        _ => print_json(value),
    }
}

fn print_json(value: &impl Serialize) -> Result<()> {
    serde_json::to_writer_pretty(io::stdout().lock(), value)?;
    println!();
    Ok(())
}
