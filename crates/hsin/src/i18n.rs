use std::collections::BTreeMap;

use hsin_core::{ClientKind, ConfigOwnershipStatus, LANGUAGE_SYSTEM};

/// Ownership reason prefix for hsin configuration that predates ownership
/// records and could not be adopted by this instance.
const LEGACY_UNCLAIMED: &str = "legacy_unclaimed";

const EN_US: &str = include_str!("../../../locales/en-US.json");
const ZH_CN: &str = include_str!("../../../locales/zh-CN.json");

#[derive(Debug)]
pub struct I18n {
    primary: BTreeMap<String, String>,
    fallback: BTreeMap<String, String>,
}

impl I18n {
    pub fn new(requested: Option<&str>) -> Self {
        let fallback = parse(EN_US);
        let system = system_locale();
        let language = resolve_language(requested, system.as_deref());
        let primary = if is_chinese(&language) {
            parse(ZH_CN)
        } else {
            fallback.clone()
        };
        Self { primary, fallback }
    }

    pub fn text<'a>(&'a self, key: &'a str) -> &'a str {
        self.primary
            .get(key)
            .or_else(|| self.fallback.get(key))
            .map_or(key, String::as_str)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.primary
            .get(key)
            .or_else(|| self.fallback.get(key))
            .map(String::as_str)
    }

    /// Configuration hsin wrote before ownership records existed has no owner;
    /// it must not be presented as another instance's.
    pub fn ownership_is_legacy(target: &ConfigOwnershipStatus) -> bool {
        target.owner.is_none()
            && target
                .takeover_unavailable_reason
                .as_deref()
                .and_then(|reason| reason.split(':').next())
                == Some(LEGACY_UNCLAIMED)
    }

    pub fn owner_label(&self, target: &ConfigOwnershipStatus) -> String {
        if target.owner_is_self {
            self.text("config_owner_self").to_owned()
        } else if let Some(owner) = &target.owner {
            format!("{} · {}", owner.instance_label, owner.instance_home)
        } else if Self::ownership_is_legacy(target) {
            self.text("config_owner_legacy").to_owned()
        } else {
            self.text("config_owner_unknown").to_owned()
        }
    }

    /// Explains why takeover is unavailable. Unclaimed legacy state also names
    /// the failed migration check and the client's manual recovery.
    pub fn ownership_reason_lines(&self, target: &ConfigOwnershipStatus) -> Vec<String> {
        let Some(reason) = &target.takeover_unavailable_reason else {
            return Vec::new();
        };
        let mut codes = reason.split(':');
        let code = codes.next().unwrap_or(reason);
        if code != LEGACY_UNCLAIMED {
            return vec![
                self.get(&format!("config_takeover.reason.{code}"))
                    .unwrap_or_else(|| self.text("config_takeover_unavailable"))
                    .to_owned(),
            ];
        }
        let detail = codes.next().unwrap_or_default().trim();
        vec![
            self.text("config_takeover.reason.legacy_unclaimed")
                .to_owned(),
            self.get(&format!("config_takeover.legacy.{detail}"))
                .unwrap_or_else(|| self.text("config_takeover.legacy.unverified"))
                .to_owned(),
            self.text(match target.client {
                ClientKind::Codex => "config_takeover.legacy_recovery.codex",
                ClientKind::Claude => "config_takeover.legacy_recovery.claude",
            })
            .to_owned(),
        ]
    }

    pub fn set_language(&mut self, language: &str) {
        let system = system_locale();
        let language = resolve_language(Some(language), system.as_deref());
        self.primary = if is_chinese(&language) {
            parse(ZH_CN)
        } else {
            self.fallback.clone()
        };
    }

    pub fn error_message(&self, error: &anyhow::Error) -> String {
        if let Some(hsin_ipc::TransportError::Rpc(rpc)) =
            error.downcast_ref::<hsin_ipc::TransportError>()
            && let Some(application) = &rpc.data
        {
            let key = format!("error.{}", application.code.as_str());
            let message = self
                .primary
                .get(&key)
                .or_else(|| self.fallback.get(&key))
                .cloned()
                .unwrap_or_else(|| application.code.to_string());
            if let Some(conflict) = &application.config_conflict {
                let details = conflict
                    .targets
                    .iter()
                    .map(|target| {
                        let mut lines = vec![format!(
                            "{}: {} — {}",
                            target.client,
                            self.owner_label(target),
                            target.config_path
                        )];
                        lines.extend(
                            self.ownership_reason_lines(target)
                                .into_iter()
                                .map(|line| format!("  {line}")),
                        );
                        lines.join("\n")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let hint = if !conflict.targets.is_empty()
                    && conflict.targets.iter().all(Self::ownership_is_legacy)
                {
                    self.text("config_takeover_legacy_unavailable").to_owned()
                } else if !conflict.targets.is_empty()
                    && conflict
                        .targets
                        .iter()
                        .all(|target| target.takeover_available)
                {
                    let clients = conflict
                        .targets
                        .iter()
                        .map(|target| target.client.to_string())
                        .collect::<Vec<_>>()
                        .join(" ");
                    format!(
                        "{}: hsin config takeover {clients}",
                        self.text("config_takeover_cli_hint")
                    )
                } else {
                    self.text("config_takeover_unavailable").to_owned()
                };
                return format!("{message}\n{details}\n{hint}");
            }
            if application.args.is_empty() {
                return message;
            }
            let args = application
                .args
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(", ");
            return format!("{message} ({args})");
        }
        format!("{error:#}")
    }
}

fn system_locale() -> Option<String> {
    sys_locale::get_locale()
        .or_else(|| {
            std::env::var("LC_ALL")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .or_else(|| {
            std::env::var("LC_MESSAGES")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .or_else(|| std::env::var("LANG").ok().filter(|value| !value.is_empty()))
}

fn resolve_language(requested: Option<&str>, system: Option<&str>) -> String {
    match requested {
        None | Some(LANGUAGE_SYSTEM) => system.unwrap_or("en-US").to_owned(),
        Some(language) => language.to_owned(),
    }
}

fn is_chinese(language: &str) -> bool {
    language.to_ascii_lowercase().starts_with("zh")
}

fn parse(input: &str) -> BTreeMap<String, String> {
    serde_json::from_str(input).expect("embedded locale must be valid JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_keys_are_identical() {
        let en = parse(EN_US);
        let zh = parse(ZH_CN);
        assert_eq!(en.keys().collect::<Vec<_>>(), zh.keys().collect::<Vec<_>>());
    }

    #[test]
    fn unknown_language_falls_back_to_english() {
        assert_eq!(I18n::new(Some("fr-FR")).text("title"), "Heart / HSIN");
    }

    #[test]
    fn system_language_uses_the_detected_locale() {
        assert_eq!(
            resolve_language(Some(LANGUAGE_SYSTEM), Some("zh-CN")),
            "zh-CN"
        );
        assert_eq!(resolve_language(None, Some("en-US")), "en-US");
        assert_eq!(resolve_language(Some("zh-CN"), Some("en-US")), "zh-CN");
    }

    fn legacy_target(client: ClientKind, reason: &str) -> ConfigOwnershipStatus {
        ConfigOwnershipStatus {
            client,
            target_id: format!("legacy-{client}"),
            config_path: format!("/tmp/{client}/config"),
            generation: 0,
            owner: None,
            owner_is_self: false,
            takeover_available: false,
            takeover_unavailable_reason: Some(reason.into()),
        }
    }

    #[test]
    fn legacy_configuration_conflicts_explain_the_failed_check_and_recovery() {
        let mut application = hsin_core::AppError::new(hsin_core::ErrorCode::ConfigConflict);
        application.config_conflict = Some(hsin_core::ConfigConflictDetails {
            targets: vec![
                legacy_target(
                    ClientKind::Codex,
                    "legacy_unclaimed:no_journal: this instance cannot prove that it wrote the legacy hsin configuration",
                ),
                legacy_target(ClientKind::Claude, "legacy_unclaimed:unknown_future_check"),
            ],
        });
        let error = anyhow::Error::new(hsin_ipc::TransportError::Rpc(
            hsin_ipc::RpcError::application(application),
        ));
        let message = I18n::new(Some("en-US")).error_message(&error);
        assert!(message.contains("codex: Legacy hsin configuration (unclaimed)"));
        assert!(message.contains("no completed record of writing this configuration"));
        assert!(message.contains("[model_providers.hsin]"));
        assert!(message.contains("cannot be adopted automatically"));
        assert!(message.contains("apiKeyHelper"));
        assert!(message.contains("do not need to start another instance"));
        assert!(!message.contains("hsin config takeover"));
        let chinese = I18n::new(Some("zh-CN")).error_message(&error);
        assert!(chinese.contains("旧版 hsin 配置（归属未确认）"));
        assert!(chinese.contains("无需启动其他实例"));
    }

    #[test]
    fn owned_conflicts_keep_their_owner_and_generic_reason() {
        let i18n = I18n::new(Some("en-US"));
        let mut target = legacy_target(ClientKind::Codex, "recovery_required: pending");
        assert!(!I18n::ownership_is_legacy(&target));
        assert_eq!(i18n.owner_label(&target), i18n.text("config_owner_unknown"));
        assert_eq!(
            i18n.ownership_reason_lines(&target),
            vec![
                i18n.text("config_takeover.reason.recovery_required")
                    .to_owned()
            ]
        );
        target.takeover_unavailable_reason = Some("legacy_unclaimed:files_changed: x".into());
        target.owner = Some(hsin_core::ConfigOwnerInfo {
            instance_id: "other".into(),
            instance_home: "/other".into(),
            instance_label: "Release".into(),
            daemon_version: "0.3.0".into(),
        });
        assert!(!I18n::ownership_is_legacy(&target));
        assert_eq!(i18n.owner_label(&target), "Release · /other");
    }

    #[test]
    fn application_errors_are_localized() {
        let transport = hsin_ipc::TransportError::Rpc(hsin_ipc::RpcError::application(
            hsin_core::AppError::new(hsin_core::ErrorCode::RevisionConflict),
        ));
        let error = anyhow::Error::new(transport);
        assert_eq!(
            I18n::new(Some("zh-CN")).error_message(&error),
            "Provider 已发生变化，请刷新后重试"
        );
    }
}
