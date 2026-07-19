use std::collections::BTreeMap;

use hsin_core::LANGUAGE_SYSTEM;

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
