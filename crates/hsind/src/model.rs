pub use hsin_core::{AuthScheme, ClientKind, ClientState, ConfigStatus, ConnectionMode, Provider};

use hsin_core::ProviderDraft;

use crate::error::{DaemonError, Result};

#[derive(Debug, Clone)]
pub struct ProviderInput {
    pub client: ClientKind,
    pub name: String,
    pub base_url: String,
    pub auth_scheme: AuthScheme,
}

impl ProviderInput {
    pub fn validate(&self) -> Result<()> {
        let draft = ProviderDraft {
            client: self.client,
            name: self.name.clone(),
            base_url: self.base_url.clone(),
            auth_scheme: self.auth_scheme,
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
            base_url: url.into(),
            auth_scheme: AuthScheme::Bearer,
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
}
