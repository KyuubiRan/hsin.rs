pub use hsin_core::{
    AuthScheme, ClaudeModelMapping, ClientKind, ClientState, ConfigStatus, ConnectionMode, Provider,
};

use hsin_core::{ProviderDraft, provider_name_from_url};

use crate::error::{DaemonError, Result};

#[derive(Debug, Clone)]
pub struct ProviderInput {
    pub client: ClientKind,
    pub name: String,
    pub description: String,
    pub base_url: String,
    pub auth_scheme: AuthScheme,
    pub model: Option<String>,
    pub claude_model_mapping: Option<ClaudeModelMapping>,
}

impl ProviderInput {
    pub fn normalized_name(&self) -> Result<String> {
        let name = self.name.trim();
        if !name.is_empty() {
            return Ok(name.to_owned());
        }
        provider_name_from_url(&self.base_url)
            .ok_or_else(|| DaemonError::Invalid("provider URL must include a host".into()))
    }

    pub fn validate(&self) -> Result<()> {
        let draft = ProviderDraft {
            client: self.client,
            name: self.normalized_name()?,
            description: self.description.clone(),
            base_url: self.base_url.clone(),
            auth_scheme: self.auth_scheme,
            model: self.model.clone(),
            claude_model_mapping: self.claude_model_mapping.clone(),
        };
        draft
            .validate()
            .map_err(|error| DaemonError::Invalid(error.to_string()))?;
        let url = url::Url::parse(self.base_url.trim())
            .map_err(|error| DaemonError::Invalid(error.to_string()))?;
        let loopback = match url.host() {
            Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            None => false,
        };
        if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
            return Err(DaemonError::Invalid(
                "provider URL must use HTTPS (loopback HTTP is allowed)".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(url: &str) -> ProviderInput {
        ProviderInput {
            client: ClientKind::Codex,
            name: "test".into(),
            description: String::new(),
            base_url: url.into(),
            auth_scheme: AuthScheme::Bearer,
            model: None,
            claude_model_mapping: None,
        }
    }

    #[test]
    fn plaintext_http_is_limited_to_exact_loopback_hosts() {
        assert!(input("http://127.0.0.1:8080/v1").validate().is_ok());
        assert!(input("http://[::1]:8080/v1").validate().is_ok());
        assert!(input("http://localhost:8080/v1").validate().is_ok());
        assert!(input("http://127.0.0.1.evil.test/v1").validate().is_err());
        assert!(input("http://localhost.evil.test/v1").validate().is_err());
        assert!(input("http://example.test/v1").validate().is_err());
    }

    #[test]
    fn empty_name_uses_registrable_domain_label() {
        let mut provider = input("https://api.example.test/v1");
        provider.name.clear();
        assert_eq!(provider.normalized_name().unwrap(), "example");
        provider.base_url = "https://ai.router.team/v1".into();
        assert_eq!(provider.normalized_name().unwrap(), "router");
        assert!(provider.validate().is_ok());
    }
}
