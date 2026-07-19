use anyhow::{Context, Result};
use hsin_core::{
    ClientKind, ClientSettings, ConnectionMode, ImportCurrentParams, ImportCurrentResult,
    ModeSetParams, ModelDiscoverParams, ModelUpdate, Provider, ProviderAddParams, ProviderDraft,
    ProviderEditParams, ProviderPatch, ProviderRemoveParams, ProviderSwitchParams, SecretInput,
    Settings, SettingsPatch,
};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::rpc::{DaemonClient, StatusSnapshot};

use super::state::{Action, FormSubmission};

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
    SetProxyPort(u16),
    SetClients(ClientSettings),
    ImportCurrent(ClientKind),
    ImportAll,
    Add(FormSubmission),
    Edit(FormSubmission),
    DiscoverModels(FormSubmission),
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
            Ok(Some("mode_changed"))
        }
        Effect::SetProxyEnabled(enabled) => update_proxy_enabled(client, enabled).await,
        Effect::SetProxyPort(port) => update_proxy_port(client, port).await,
        Effect::SetClients(settings) => update_clients(client, settings).await,
        Effect::ImportCurrent(kind) => {
            let imported = import_current(client, kind).await?;
            Ok(Some(if imported {
                "provider_imported"
            } else {
                "provider_unchanged"
            }))
        }
        Effect::ImportAll => import_all(client).await,
        Effect::Add(form) => {
            let model = match form.model {
                ModelUpdate::Set(model) => Some(model),
                ModelUpdate::Preserve | ModelUpdate::Clear => None,
            };
            let request = ProviderAddParams {
                provider: ProviderDraft {
                    client: form.client,
                    name: form.name,
                    description: form.description,
                    base_url: form.base_url,
                    auth_scheme: form.auth_scheme,
                    model,
                },
                secret: if form.secret.is_empty() {
                    SecretInput::Clear
                } else {
                    SecretInput::Replace(form.secret.to_string())
                },
            };
            let _: Value = client.call("provider.add", &request).await?;
            Ok(Some("provider_added"))
        }
        Effect::Edit(form) => {
            let id = form.id.context("edit form is missing provider ID")?;
            let expected_revision = form
                .revision
                .context("edit form is missing provider revision")?;
            let request = ProviderEditParams {
                id,
                expected_revision,
                patch: ProviderPatch {
                    name: Some(form.name),
                    base_url: Some(form.base_url),
                    auth_scheme: Some(form.auth_scheme),
                    description: Some(form.description),
                    model: form.model,
                },
                secret: if form.secret.is_empty() {
                    SecretInput::Preserve
                } else {
                    SecretInput::Replace(form.secret.to_string())
                },
            };
            let _: Value = client.call("provider.edit", &request).await?;
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
    }
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

async fn import_all(client: &DaemonClient) -> Result<Option<&'static str>> {
    let codex = import_current(client, ClientKind::Codex).await;
    let claude = import_current(client, ClientKind::Claude).await;
    match (codex, claude) {
        (Ok(codex), Ok(claude)) => Ok(Some(if codex || claude {
            "providers_imported_all"
        } else {
            "providers_unchanged_all"
        })),
        (Ok(_), Err(_)) | (Err(_), Ok(_)) => Ok(Some("providers_imported_partial")),
        (Err(error), Err(_)) => Err(error),
    }
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
                proxy_port: None,
                proxy_enabled: Some(enabled),
                clients: None,
            },
        )
        .await?;
    Ok(Some(if enabled {
        "proxy_enabled"
    } else {
        "proxy_disabled"
    }))
}

async fn update_proxy_port(client: &DaemonClient, port: u16) -> Result<Option<&'static str>> {
    let _: Value = client
        .call(
            "settings.set",
            &SettingsPatch {
                language: None,
                proxy_port: Some(port),
                proxy_enabled: None,
                clients: None,
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
                proxy_port: None,
                proxy_enabled: None,
                clients: None,
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
                proxy_port: None,
                proxy_enabled: None,
                clients: Some(clients),
            },
        )
        .await?;
    Ok(Some("client_settings_changed"))
}

async fn load(client: &DaemonClient) -> Result<(Vec<Provider>, StatusSnapshot, Settings)> {
    let providers = client.provider_list(None).await?;
    let status = client.status().await?;
    let settings = client.call("settings.get", &serde_json::json!({})).await?;
    Ok((providers, status, settings))
}
