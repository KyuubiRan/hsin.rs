use std::collections::BTreeMap;

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
        let language = requested
            .map(str::to_owned)
            .or_else(|| std::env::var("LANG").ok())
            .unwrap_or_else(|| String::from("en-US"));
        let primary = if language.to_ascii_lowercase().starts_with("zh") {
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
        self.primary = if language.to_ascii_lowercase().starts_with("zh") {
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
