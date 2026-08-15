use anyhow::{Context, Result};
use hsin_core::{
    ClaudeModelMappingUpdate, ClientAuthUpdate, ClientKind, ClientSettings, ConnectionMode,
    ImportCurrentParams, ImportCurrentResult, ModeSetParams, ModelDiscoverParams, ModelUpdate,
    Provider, ProviderAddParams, ProviderDraft, ProviderEditParams, ProviderPatch,
    ProviderRemoveParams, ProviderSwitchParams, SecretInput, Settings, SettingsPatch,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::rpc::{DaemonClient, StatusSnapshot};

use super::state::{Action, FormSubmission, ProviderClipboard};

pub(super) enum Effect {
    Refresh,
    Switch {
        client: ClientKind,
        id: String,
    },
    SetMode {
        client: ClientKind,
        mode: ConnectionMode,
    },
    SetProxyEnabled(bool),
    SetProxyHost(String),
    SetProxyPort(u16),
    SetClients(ClientSettings),
    SetClientAuth {
        client: ClientKind,
        disable_custom_auth: bool,
    },
    SetClaudeModelNames(bool),
    ImportCurrent(ClientKind),
    Add(FormSubmission),
    Edit(FormSubmission),
    DiscoverModels(FormSubmission),
    CopyProvider(Provider),
    Remove {
        id: String,
        expected_revision: u64,
    },
    SetLanguage(String),
}

pub(super) async fn worker(
    client: DaemonClient,
    mut effects: mpsc::Receiver<Effect>,
    actions: mpsc::Sender<Action>,
) {
    while let Some(effect) = effects.recv().await {
        if let Effect::DiscoverModels(form) = effect {
            let request = ModelDiscoverParams {
                client: form.client,
                provider_id: form.id.clone(),
                base_url: form.base_url.clone(),
                auth_scheme: form.auth_scheme,
                secret: if form.secret.is_empty() && form.id.is_some() {
                    SecretInput::Preserve
                } else {
                    SecretInput::Replace(form.secret.to_string())
                },
            };
            match client.call("provider.discover_models", &request).await {
                Ok(discovery) => {
                    let _ = actions
                        .send(Action::ModelsDiscovered { form, discovery })
                        .await;
                }
                Err(error) => {
                    let _ = actions
                        .send(Action::ModelDiscoveryFailed {
                            form,
                            message: format!("{error:#}"),
                        })
                        .await;
                }
            }
            continue;
        }
        if let Effect::CopyProvider(provider) = effect {
            match resolve_provider_copy(&client, provider).await {
                Ok(clipboard) => {
                    let _ = actions.send(Action::ProviderCopied(clipboard)).await;
                }
                Err(error) => {
                    let _ = actions.send(Action::Failed(error_notice(&error))).await;
                }
            }
            continue;
        }
        let result = execute_effect(&client, effect).await;
        match result {
            Ok(notice) => {
                if let Some(notice) = notice {
                    let _ = actions.send(Action::Notice(notice)).await;
                }
                match load(&client).await {
                    Ok((providers, status, settings)) => {
                        let _ = actions
                            .send(Action::Loaded {
                                providers,
                                status,
                                settings,
                            })
                            .await;
                    }
                    Err(error) => {
                        let _ = actions.send(Action::Failed(error_notice(&error))).await;
                    }
                }
            }
            Err(error) => {
                let _ = actions.send(Action::Failed(error_notice(&error))).await;
            }
        }
    }
}

fn error_notice(error: &anyhow::Error) -> String {
    if let Some(hsin_ipc::TransportError::Rpc(rpc)) =
        error.downcast_ref::<hsin_ipc::TransportError>()
        && let Some(application) = &rpc.data
    {
        return format!("@error.{}", application.code.as_str());
    }
    format!("{error:#}")
}

#[allow(clippy::too_many_lines)]
async fn execute_effect(client: &DaemonClient, effect: Effect) -> Result<Option<&'static str>> {
    match effect {
        Effect::Refresh => Ok(None),
        Effect::Switch { client: kind, id } => {
            let _: Value = client
                .call(
                    "provider.switch",
                    &ProviderSwitchParams {
                        client: kind,
                        provider_id: id,
                    },
                )
                .await?;
            Ok(Some("switched"))
        }
        Effect::SetMode { client: kind, mode } => {
            let _: Value = client
                .call("mode.set", &ModeSetParams { client: kind, mode })
                .await?;
            Ok(Some(if mode == ConnectionMode::Proxy {
                "mode_proxy_enabled"
            } else {
                "mode_proxy_disabled"
            }))
        }
        Effect::SetProxyEnabled(enabled) => update_proxy_enabled(client, enabled).await,
        Effect::SetProxyHost(host) => update_proxy_host(client, host).await,
        Effect::SetProxyPort(port) => update_proxy_port(client, port).await,
        Effect::SetClients(settings) => update_clients(client, settings).await,
        Effect::SetClientAuth {
            client: kind,
            disable_custom_auth,
        } => update_client_auth(client, kind, disable_custom_auth).await,
        Effect::SetClaudeModelNames(enabled) => update_claude_model_names(client, enabled).await,
        Effect::ImportCurrent(kind) => {
            let imported = import_current(client, kind).await?;
            Ok(Some(if imported {
                "provider_imported"
            } else {
                "provider_unchanged"
            }))
        }
        Effect::Add(form) => {
            let _: Value = client
                .call("provider.add", &provider_add_params(form))
                .await?;
            Ok(Some("provider_added"))
        }
        Effect::Edit(form) => {
            let _: Value = client
                .call("provider.edit", &provider_edit_params(form)?)
                .await?;
            Ok(Some("provider_updated"))
        }
        Effect::Remove {
            id,
            expected_revision,
        } => {
            let _: Value = client
                .call(
                    "provider.remove",
                    &ProviderRemoveParams {
                        id,
                        expected_revision,
                    },
                )
                .await?;
            Ok(Some("provider_removed"))
        }
        Effect::SetLanguage(language) => update_language(client, language).await,
        Effect::DiscoverModels(_) => unreachable!("model discovery is handled by the worker"),
        Effect::CopyProvider(_) => unreachable!("provider copying is handled by the worker"),
    }
}

/// Translate a saved provider form into the daemon request.
///
/// Split out from the call itself so the fields the form carries — the Claude model mapping in
/// particular — can be checked without a running daemon.
pub(super) fn provider_add_params(form: FormSubmission) -> ProviderAddParams {
    let model = match form.model {
        ModelUpdate::Set(model) => Some(model),
        ModelUpdate::Preserve | ModelUpdate::Clear => None,
    };
    let claude_model_mapping = match form.claude_model_mapping {
        ClaudeModelMappingUpdate::Set(mapping) => Some(mapping),
        ClaudeModelMappingUpdate::Preserve | ClaudeModelMappingUpdate::Clear => None,
    };
    ProviderAddParams {
        provider: ProviderDraft {
            client: form.client,
            name: form.name,
            description: form.description,
            base_url: form.base_url,
            auth_scheme: form.auth_scheme,
            model,
            claude_model_mapping,
        },
        secret: if form.secret.is_empty() {
            SecretInput::Clear
        } else {
            SecretInput::Replace(form.secret.to_string())
        },
    }
}

pub(super) fn provider_edit_params(form: FormSubmission) -> Result<ProviderEditParams> {
    Ok(ProviderEditParams {
        id: form.id.context("edit form is missing provider ID")?,
        expected_revision: form
            .revision
            .context("edit form is missing provider revision")?,
        patch: ProviderPatch {
            name: Some(form.name),
            base_url: Some(form.base_url),
            auth_scheme: Some(form.auth_scheme),
            description: Some(form.description),
            model: form.model,
            claude_model_mapping: form.claude_model_mapping,
        },
        secret: if form.secret.is_empty() {
            SecretInput::Preserve
        } else {
            SecretInput::Replace(form.secret.to_string())
        },
    })
}

async fn resolve_provider_copy(
    client: &DaemonClient,
    provider: Provider,
) -> Result<ProviderClipboard> {
    let value: Value = client
        .call(
            "credential.resolve",
            &json!({
                "client": provider.client,
                "provider_id": provider.id,
                "revision": provider.revision,
            }),
        )
        .await?;
    let secret = value
        .as_str()
        .or_else(|| value.get("secret").and_then(Value::as_str))
        .context("daemon returned an invalid credential response")?;
    Ok(ProviderClipboard {
        provider,
        secret: zeroize::Zeroizing::new(secret.to_owned()),
    })
}

async fn import_current(client: &DaemonClient, kind: ClientKind) -> Result<bool> {
    let result: ImportCurrentResult = client
        .call(
            "provider.import_current",
            &ImportCurrentParams {
                client: kind,
                name: String::new(),
            },
        )
        .await?;
    Ok(result.imported)
}

async fn update_proxy_enabled(
    client: &DaemonClient,
    enabled: bool,
) -> Result<Option<&'static str>> {
    let _: Value = client
        .call(
            "settings.set",
            &SettingsPatch {
                language: None,
                proxy_host: None,
                proxy_port: None,
                proxy_enabled: Some(enabled),
                clients: None,
                client_auth: None,
                claude_model_names_enabled: None,
            },
        )
        .await?;
    Ok(Some(if enabled {
        "proxy_enabled"
    } else {
        "proxy_disabled"
    }))
}

async fn update_proxy_host(client: &DaemonClient, host: String) -> Result<Option<&'static str>> {
    let _: Value = client
        .call(
            "settings.set",
            &SettingsPatch {
                language: None,
                proxy_host: Some(host),
                proxy_port: None,
                proxy_enabled: None,
                clients: None,
                client_auth: None,
                claude_model_names_enabled: None,
            },
        )
        .await?;
    Ok(Some("proxy_address_changed"))
}

async fn update_proxy_port(client: &DaemonClient, port: u16) -> Result<Option<&'static str>> {
    let _: Value = client
        .call(
            "settings.set",
            &SettingsPatch {
                language: None,
                proxy_host: None,
                proxy_port: Some(port),
                proxy_enabled: None,
                clients: None,
                client_auth: None,
                claude_model_names_enabled: None,
            },
        )
        .await?;
    Ok(Some("proxy_port_changed"))
}

async fn update_language(client: &DaemonClient, language: String) -> Result<Option<&'static str>> {
    let _: Value = client
        .call(
            "settings.set",
            &SettingsPatch {
                language: Some(language),
                proxy_host: None,
                proxy_port: None,
                proxy_enabled: None,
                clients: None,
                client_auth: None,
                claude_model_names_enabled: None,
            },
        )
        .await?;
    Ok(Some("language_changed"))
}

async fn update_clients(
    client: &DaemonClient,
    clients: ClientSettings,
) -> Result<Option<&'static str>> {
    let _: Value = client
        .call(
            "settings.set",
            &SettingsPatch {
                language: None,
                proxy_host: None,
                proxy_port: None,
                proxy_enabled: None,
                clients: Some(clients),
                client_auth: None,
                claude_model_names_enabled: None,
            },
        )
        .await?;
    Ok(Some("client_settings_changed"))
}

async fn update_client_auth(
    client: &DaemonClient,
    kind: ClientKind,
    disable_custom_auth: bool,
) -> Result<Option<&'static str>> {
    let _: Value = client
        .call(
            "settings.set",
            &SettingsPatch {
                language: None,
                proxy_host: None,
                proxy_port: None,
                proxy_enabled: None,
                clients: None,
                client_auth: Some(ClientAuthUpdate {
                    client: kind,
                    disable_custom_auth,
                }),
                claude_model_names_enabled: None,
            },
        )
        .await?;
    Ok(Some("client_auth_changed"))
}

async fn update_claude_model_names(
    client: &DaemonClient,
    enabled: bool,
) -> Result<Option<&'static str>> {
    let _: Value = client
        .call(
            "settings.set",
            &SettingsPatch {
                language: None,
                proxy_host: None,
                proxy_port: None,
                proxy_enabled: None,
                clients: None,
                client_auth: None,
                claude_model_names_enabled: Some(enabled),
            },
        )
        .await?;
    Ok(Some("claude_model_names_changed"))
}

async fn load(client: &DaemonClient) -> Result<(Vec<Provider>, StatusSnapshot, Settings)> {
    let providers = client.provider_list(None).await?;
    let mut status = client.status().await?;
    status.recovery_key_exported = client.security_status().await?.recovery_key_configured;
    let settings = client.call("settings.get", &serde_json::json!({})).await?;
    Ok((providers, status, settings))
}
