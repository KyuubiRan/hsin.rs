use anyhow::{Context, Result, ensure};
use hsin_core::{
    ClaudeModelMappingUpdate, ClientAuthUpdate, ClientKind, ClientSettings, CodexConfigNameUpdate,
    CodexImageConfigUpdate, CodexImageListParams, CodexImageSwitchParams, ConfigConflictDetails,
    ConfigTakeoverParams, ConfigTakeoverResult, ConnectionMode, ErrorCode, ImportCurrentParams,
    ImportCurrentResult, ModeSetParams, ModelDiscoverParams, ModelPriceInput, ModelPriceList,
    ModelUpdate, Provider, ProviderAddParams, ProviderDraft, ProviderEditParams, ProviderPatch,
    ProviderRemoveParams, ProviderSwitchParams, SecretInput, Settings, SettingsPatch,
    UsageStatsQuery, UsageStatsReport,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use zeroize::Zeroize;

use crate::rpc::{DaemonClient, StatusSnapshot};

use super::state::{Action, FormSubmission, ProviderClipboard};

pub(super) enum Effect {
    Refresh,
    Switch {
        client: ClientKind,
        id: String,
    },
    SwitchImage(String),
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
    SetCodexOfficialAuthPreservation(bool),
    SetClaudeModelNames(bool),
    ImportCurrent(ClientKind),
    Add(FormSubmission),
    Edit(FormSubmission),
    DiscoverModels(FormSubmission),
    /// The mapping dialog's model list. The dialog is already up and waiting for the result, so
    /// only the request travels out.
    DiscoverMappingModels(ModelDiscoverParams),
    CopyProvider(Provider),
    Remove {
        id: String,
        expected_revision: u64,
    },
    SetLanguage(String),
    SetUpstreamProxy {
        config: hsin_core::UpstreamProxyConfig,
        password: SensitiveSecretInput,
    },
    SetStatsChartStyle(hsin_core::StatsChartStyle),
    QueryUsage(UsageStatsQuery),
    QueryUsageDay(UsageStatsQuery),
    LoadPrices,
    SetPrice(ModelPriceInput),
    RemovePrice(String),
    /// Fetches the public price list; only ever sent on the user's request.
    RefreshPrices,
    Takeover {
        request: ConfigTakeoverParams,
        operation: Box<Effect>,
    },
}

/// A retained UI operation must clear its password on cancellation as well as on completion.
#[derive(Debug)]
pub(super) struct SensitiveSecretInput(pub(super) SecretInput);

impl From<SecretInput> for SensitiveSecretInput {
    fn from(value: SecretInput) -> Self {
        Self(value)
    }
}

impl PartialEq<SecretInput> for SensitiveSecretInput {
    fn eq(&self, other: &SecretInput) -> bool {
        self.0 == *other
    }
}

impl Drop for SensitiveSecretInput {
    fn drop(&mut self) {
        clear_secret(&mut self.0);
    }
}

fn clear_secret(secret: &mut SecretInput) {
    if let SecretInput::Replace(value) = secret {
        value.zeroize();
    }
}

fn ownership_conflict(error: &anyhow::Error) -> Option<ConfigConflictDetails> {
    error.chain().find_map(|source| {
        let hsin_ipc::TransportError::Rpc(rpc) = source.downcast_ref()? else {
            return None;
        };
        rpc.data.as_ref().and_then(|application| {
            (application.code == ErrorCode::ConfigConflict)
                .then(|| application.config_conflict.clone())
                .flatten()
        })
    })
}

#[allow(clippy::too_many_lines)]
pub(super) async fn worker(
    client: DaemonClient,
    mut effects: mpsc::Receiver<Effect>,
    actions: mpsc::Sender<Action>,
) {
    while let Some(effect) = effects.recv().await {
        if let Effect::DiscoverModels(form) = effect {
            let request = discovery_request_for_form(&form);
            let action = match discover(&client, &request).await {
                Ok(discovery) => Action::ModelsDiscovered { form, discovery },
                Err(message) => Action::ModelDiscoveryFailed { form, message },
            };
            let _ = actions.send(action).await;
            continue;
        }
        if let Effect::DiscoverMappingModels(request) = effect {
            let action = match discover(&client, &request).await {
                Ok(discovery) => Action::MappingModelsDiscovered(discovery),
                Err(message) => Action::MappingModelDiscoveryFailed(message),
            };
            let _ = actions.send(action).await;
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
        if matches!(
            effect,
            Effect::LoadPrices
                | Effect::SetPrice(_)
                | Effect::RemovePrice(_)
                | Effect::RefreshPrices
        ) {
            let action = match pricing(&client, effect).await {
                Ok((prices, notice)) => {
                    if let Some(notice) = notice {
                        let _ = actions.send(Action::Notice(notice)).await;
                    }
                    Action::PricesLoaded(prices)
                }
                Err(error) => Action::Failed(error_notice(&error)),
            };
            let _ = actions.send(action).await;
            continue;
        }
        if let Effect::QueryUsageDay(query) = effect {
            let action = match client
                .call::<_, UsageStatsReport>(hsin_ipc::method::STATS_QUERY, &query)
                .await
            {
                Ok(report) => Action::DayUsageLoaded(report),
                Err(error) => Action::Failed(error_notice(&error)),
            };
            let _ = actions.send(action).await;
            continue;
        }
        if let Effect::QueryUsage(query) = effect {
            match client
                .call::<_, UsageStatsReport>(hsin_ipc::method::STATS_QUERY, &query)
                .await
            {
                Ok(report) => {
                    let _ = actions.send(Action::UsageLoaded(report)).await;
                }
                Err(error) => {
                    let _ = actions.send(Action::Failed(error_notice(&error))).await;
                }
            }
            continue;
        }
        let (operation, result, takeover_failed) =
            if let Effect::Takeover { request, operation } = effect {
                let takeover = client
                    .call::<_, ConfigTakeoverResult>(hsin_ipc::method::CONFIG_TAKEOVER, &request)
                    .await;
                let (result, takeover_failed) = match takeover {
                    Ok(_) => (execute_effect(&client, &operation).await, false),
                    Err(error) => (Err(error), true),
                };
                (*operation, result, takeover_failed)
            } else {
                let result = execute_effect(&client, &effect).await;
                (effect, result, false)
            };
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
                let action = if let Some(details) = ownership_conflict(&error) {
                    Action::ConfigConflict { operation, details }
                } else {
                    Action::ConfigurationFailed {
                        operation,
                        message: if takeover_failed {
                            takeover_error_notice(&error)
                        } else {
                            error_notice(&error)
                        },
                    }
                };
                let _ = actions.send(action).await;
            }
        }
    }
}

/// Runs a price-rule change and returns the list as it stands afterwards.
async fn pricing(
    client: &DaemonClient,
    effect: Effect,
) -> Result<(ModelPriceList, Option<&'static str>)> {
    use hsin_ipc::method;

    let notice = match effect {
        Effect::SetPrice(input) => {
            let _: Value = client.call(method::PRICING_SET, &input).await?;
            Some("pricing_saved")
        }
        Effect::RemovePrice(id) => {
            let _: Value = client
                .call(method::PRICING_REMOVE, &json!({ "id": id }))
                .await?;
            Some("pricing_removed")
        }
        Effect::RefreshPrices => {
            let _: Value = client.call(method::PRICING_REFRESH, &json!({})).await?;
            Some("pricing_refreshed")
        }
        _ => None,
    };
    let prices = client.call(method::PRICING_LIST, &json!({})).await?;
    Ok((prices, notice))
}

/// Run a model lookup, folding any failure into the message the caller reports.
async fn discover(
    client: &DaemonClient,
    request: &ModelDiscoverParams,
) -> std::result::Result<hsin_core::ModelDiscovery, String> {
    client
        .call("provider.discover_models", request)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// The lookup a provider form asks for. An edit reuses the stored credential rather than sending
/// the empty string the form carries, so a provider whose key is already saved still lists.
fn discovery_request_for_form(form: &FormSubmission) -> ModelDiscoverParams {
    ModelDiscoverParams {
        client: form.client,
        provider_id: form.id.clone(),
        base_url: form.base_url.clone(),
        auth_scheme: form.auth_scheme,
        secret: if form.secret.is_empty() && form.id.is_some() {
            SecretInput::Preserve
        } else {
            SecretInput::Replace(form.secret.to_string())
        },
        network_proxy: form.network_proxy.clone(),
        proxy_password: proxy_password_input(
            &form.proxy_password,
            form.proxy_password_clear,
            form.id.is_some(),
        ),
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

/// Only translate the daemon's controlled takeover failure reasons; never display request data.
fn takeover_error_notice(error: &anyhow::Error) -> String {
    let application = error.chain().find_map(|source| {
        let hsin_ipc::TransportError::Rpc(rpc) = source.downcast_ref()? else {
            return None;
        };
        rpc.data.as_ref()
    });
    let key = application.and_then(|application| {
        if application.code == ErrorCode::KeyStoreLocked {
            return Some("config_takeover_owner_locked");
        }
        let message = application.args.get("message")?;
        let message = message
            .strip_prefix("configuration error: ")
            .unwrap_or(message);
        let message = message
            .strip_prefix("codex: ")
            .or_else(|| message.strip_prefix("claude: "))
            .unwrap_or(message);
        if message.starts_with("managing instance is unavailable") {
            Some("config_takeover_owner_unavailable")
        } else if message.starts_with("managing instance has an incompatible version")
            || message.starts_with("managing instance has no IPC endpoint")
        {
            Some("config_takeover_owner_upgrade")
        } else if message.starts_with("managing instance identity or capability changed") {
            Some("config_takeover_owner_changed")
        } else if message.starts_with("managing instance could not restore its configuration") {
            Some("config_takeover_owner_restore_failed")
        } else if message.starts_with("Codex authentication backup could not be decrypted") {
            Some("config_takeover_auth_backup_unreadable")
        } else {
            None
        }
    });
    key.map_or_else(|| error_notice(error), |key| format!("@{key}"))
}

#[allow(clippy::too_many_lines)]
async fn execute_effect(client: &DaemonClient, effect: &Effect) -> Result<Option<&'static str>> {
    match effect {
        Effect::Refresh => Ok(None),
        Effect::Switch { client: kind, id } => {
            let _: Value = client
                .call(
                    "provider.switch",
                    &ProviderSwitchParams {
                        client: *kind,
                        provider_id: id.clone(),
                    },
                )
                .await?;
            Ok(Some("switched"))
        }
        Effect::SwitchImage(provider_id) => {
            let _: Value = client
                .call(
                    "codex_image.switch",
                    &CodexImageSwitchParams {
                        provider_id: provider_id.clone(),
                    },
                )
                .await?;
            Ok(Some("image_provider_switched"))
        }
        Effect::SetMode { client: kind, mode } => {
            let _: Value = client
                .call(
                    "mode.set",
                    &ModeSetParams {
                        client: *kind,
                        mode: *mode,
                    },
                )
                .await?;
            Ok(Some(if *mode == ConnectionMode::Proxy {
                "mode_proxy_enabled"
            } else {
                "mode_proxy_disabled"
            }))
        }
        Effect::SetProxyEnabled(enabled) => update_proxy_enabled(client, *enabled).await,
        Effect::SetProxyHost(host) => update_proxy_host(client, host.clone()).await,
        Effect::SetProxyPort(port) => update_proxy_port(client, *port).await,
        Effect::SetClients(settings) => update_clients(client, settings.clone()).await,
        Effect::SetClientAuth {
            client: kind,
            disable_custom_auth,
        } => update_client_auth(client, *kind, *disable_custom_auth).await,
        Effect::SetCodexOfficialAuthPreservation(enabled) => {
            update_codex_official_auth_preservation(client, *enabled).await
        }
        Effect::SetClaudeModelNames(enabled) => update_claude_model_names(client, *enabled).await,
        Effect::ImportCurrent(kind) => {
            let imported = import_current(client, *kind).await?;
            Ok(Some(if imported {
                "provider_imported"
            } else {
                "provider_unchanged"
            }))
        }
        Effect::Add(form) => {
            let mut request = provider_add_params(form.clone());
            let result = client.call::<_, Value>("provider.add", &request).await;
            clear_secret(&mut request.secret);
            clear_secret(&mut request.proxy_password);
            result?;
            Ok(Some("provider_added"))
        }
        Effect::Edit(form) => {
            let mut request = provider_edit_params(form.clone())?;
            let result = client.call::<_, Value>("provider.edit", &request).await;
            clear_secret(&mut request.secret);
            clear_secret(&mut request.proxy_password);
            result?;
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
                        id: id.clone(),
                        expected_revision: *expected_revision,
                    },
                )
                .await?;
            Ok(Some("provider_removed"))
        }
        Effect::SetLanguage(language) => update_language(client, language.clone()).await,
        Effect::SetUpstreamProxy { config, password } => {
            update_upstream_proxy(client, config.clone(), &password.0).await
        }
        Effect::SetStatsChartStyle(style) => {
            let _: Value = client
                .call(
                    "settings.set",
                    &SettingsPatch {
                        stats_chart_style: Some(*style),
                        ..SettingsPatch::default()
                    },
                )
                .await?;
            Ok(None)
        }
        Effect::DiscoverModels(_) => unreachable!("model discovery is handled by the worker"),
        Effect::DiscoverMappingModels(_) => {
            unreachable!("mapping model discovery is handled by the worker")
        }
        Effect::CopyProvider(_) => unreachable!("provider copying is handled by the worker"),
        Effect::QueryUsage(_) | Effect::QueryUsageDay(_) => {
            unreachable!("usage queries are handled by the worker")
        }
        Effect::LoadPrices
        | Effect::SetPrice(_)
        | Effect::RemovePrice(_)
        | Effect::RefreshPrices => {
            unreachable!("price rules are handled by the worker")
        }
        Effect::Takeover { .. } => unreachable!("takeover is handled by the worker"),
    }
}

/// Translate a saved provider form into the daemon request.
///
/// Split out from the call itself so the fields the form carries — the Claude model mapping in
/// particular — can be checked without a running daemon.
pub(super) fn provider_add_params(form: FormSubmission) -> ProviderAddParams {
    let proxy_password = proxy_password_input(
        &form.proxy_password,
        form.proxy_password_clear,
        form.id.is_some(),
    );
    let model = match form.model {
        ModelUpdate::Set(model) => Some(model),
        ModelUpdate::Preserve | ModelUpdate::Clear => None,
    };
    let claude_model_mapping = match form.claude_model_mapping {
        ClaudeModelMappingUpdate::Set(mapping) => Some(mapping),
        ClaudeModelMappingUpdate::Preserve | ClaudeModelMappingUpdate::Clear => None,
    };
    let codex_config_name = match form.codex_config_name {
        CodexConfigNameUpdate::Set(name) => Some(name),
        CodexConfigNameUpdate::Preserve => None,
    };
    ProviderAddParams {
        provider: ProviderDraft {
            client: form.client,
            name: form.name,
            description: form.description,
            base_url: form.base_url,
            auth_scheme: form.auth_scheme,
            model,
            codex_config_name,
            claude_model_mapping,
            scope: form.scope,
            codex_image: form.codex_image,
            codex_tuning: form.codex_tuning,
            network_proxy: form.network_proxy,
        },
        proxy_password,
        secret: if form.secret.is_empty() {
            SecretInput::Clear
        } else {
            SecretInput::Replace(form.secret.to_string())
        },
    }
}

pub(super) fn provider_edit_params(form: FormSubmission) -> Result<ProviderEditParams> {
    let proxy_password = proxy_password_input(
        &form.proxy_password,
        form.proxy_password_clear,
        form.id.is_some(),
    );
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
            codex_config_name: form.codex_config_name,
            claude_model_mapping: form.claude_model_mapping,
            codex_image: CodexImageConfigUpdate::Set(form.codex_image),
            codex_tuning: Some(form.codex_tuning),
            network_proxy: Some(form.network_proxy),
        },
        secret: if form.secret.is_empty() {
            SecretInput::Preserve
        } else {
            SecretInput::Replace(form.secret.to_string())
        },
        proxy_password,
    })
}

fn proxy_password_input(password: &str, clear: bool, existing_provider: bool) -> SecretInput {
    if clear {
        SecretInput::Clear
    } else if password.is_empty() {
        if existing_provider {
            SecretInput::Preserve
        } else {
            SecretInput::Clear
        }
    } else {
        SecretInput::Replace(password.to_owned())
    }
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
                codex_preserve_official_auth: None,
                stats_chart_style: None,
                claude_model_names_enabled: None,
                upstream_proxy: None,
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
                codex_preserve_official_auth: None,
                stats_chart_style: None,
                claude_model_names_enabled: None,
                upstream_proxy: None,
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
                codex_preserve_official_auth: None,
                stats_chart_style: None,
                claude_model_names_enabled: None,
                upstream_proxy: None,
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
                codex_preserve_official_auth: None,
                stats_chart_style: None,
                claude_model_names_enabled: None,
                upstream_proxy: None,
            },
        )
        .await?;
    Ok(Some("language_changed"))
}

async fn update_upstream_proxy(
    client: &DaemonClient,
    config: hsin_core::UpstreamProxyConfig,
    password: &SecretInput,
) -> Result<Option<&'static str>> {
    let mut request = SettingsPatch {
        upstream_proxy: Some(hsin_core::UpstreamProxyUpdate {
            config,
            password: password.clone(),
        }),
        ..SettingsPatch::default()
    };
    let result = client.call::<_, Settings>("settings.set", &request).await;
    if let Some(proxy) = &mut request.upstream_proxy {
        clear_secret(&mut proxy.password);
    }
    result?;
    Ok(Some("upstream_proxy_changed"))
}

async fn update_clients(
    client: &DaemonClient,
    clients: ClientSettings,
) -> Result<Option<&'static str>> {
    let updated: Settings = client
        .call(
            "settings.set",
            &SettingsPatch {
                language: None,
                proxy_host: None,
                proxy_port: None,
                proxy_enabled: None,
                clients: Some(clients.clone()),
                client_auth: None,
                codex_preserve_official_auth: None,
                stats_chart_style: None,
                claude_model_names_enabled: None,
                upstream_proxy: None,
            },
        )
        .await?;
    ensure!(
        updated.clients == clients,
        "hsind did not retain client settings; reopen hsin to update the daemon"
    );
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
                codex_preserve_official_auth: None,
                stats_chart_style: None,
                claude_model_names_enabled: None,
                upstream_proxy: None,
            },
        )
        .await?;
    Ok(Some("client_auth_changed"))
}

async fn update_codex_official_auth_preservation(
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
                codex_preserve_official_auth: Some(enabled),
                stats_chart_style: None,
                claude_model_names_enabled: None,
                upstream_proxy: None,
            },
        )
        .await?;
    Ok(Some("codex_official_auth_preservation_changed"))
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
                codex_preserve_official_auth: None,
                stats_chart_style: None,
                claude_model_names_enabled: Some(enabled),
                upstream_proxy: None,
            },
        )
        .await?;
    Ok(Some("claude_model_names_changed"))
}

async fn load(client: &DaemonClient) -> Result<(Vec<Provider>, StatusSnapshot, Settings)> {
    let mut providers = client.provider_list(None).await?;
    let image_providers: Vec<Provider> = client
        .call("codex_image.list", &CodexImageListParams::default())
        .await?;
    providers.extend(
        image_providers
            .into_iter()
            .filter(|provider| provider.scope == hsin_core::ProviderScope::ImageOnly),
    );
    let mut status = client.status().await?;
    status.recovery_key_exported = client.security_status().await?.recovery_key_configured;
    let settings = client.call("settings.get", &serde_json::json!({})).await?;
    Ok((providers, status, settings))
}

#[cfg(test)]
mod ownership_tests {
    use super::*;

    #[test]
    fn ordinary_cas_and_transport_errors_are_never_takeover_candidates() {
        for error in [
            anyhow::Error::new(hsin_ipc::TransportError::Rpc(
                hsin_ipc::RpcError::application(hsin_core::AppError::new(
                    ErrorCode::ConfigConflict,
                )),
            )),
            anyhow::Error::new(hsin_ipc::TransportError::Rpc(
                hsin_ipc::RpcError::application(hsin_core::AppError::new(
                    ErrorCode::RevisionConflict,
                )),
            )),
            anyhow::Error::new(hsin_ipc::TransportError::Timeout(
                std::time::Duration::from_secs(10),
            )),
        ] {
            assert!(ownership_conflict(&error).is_none());
        }
    }

    #[test]
    fn takeover_failures_identify_the_remote_owner_without_echoing_rpc_data() {
        for (code, message, expected) in [
            (
                ErrorCode::ConfigUnavailable,
                "configuration error: codex: managing instance is unavailable; start it and retry takeover",
                "@config_takeover_owner_unavailable",
            ),
            (
                ErrorCode::ConfigUnavailable,
                "configuration error: codex: Codex authentication backup could not be decrypted; configuration files were left unchanged",
                "@config_takeover_auth_backup_unreadable",
            ),
            (
                ErrorCode::ConfigUnavailable,
                "configuration error: managing instance is unavailable; start it and retry takeover",
                "@config_takeover_owner_unavailable",
            ),
            (
                ErrorCode::ConfigUnavailable,
                "configuration error: managing instance has an incompatible version; upgrade it and retry",
                "@config_takeover_owner_upgrade",
            ),
            (
                ErrorCode::ConfigUnavailable,
                "configuration error: managing instance could not restore its configuration; inspect its status, unlock it if needed, and retry",
                "@config_takeover_owner_restore_failed",
            ),
            (
                ErrorCode::KeyStoreLocked,
                "daemon is locked because the system keyring is unavailable",
                "@config_takeover_owner_locked",
            ),
        ] {
            let application = hsin_core::AppError::new(code)
                .with_arg("message", message)
                .with_arg("unrelated", "never-echo-this-value");
            let error = anyhow::Error::new(hsin_ipc::TransportError::Rpc(
                hsin_ipc::RpcError::application(application),
            ));
            assert_eq!(takeover_error_notice(&error), expected);
            assert!(!takeover_error_notice(&error).contains("never-echo"));
        }
    }
}
