use std::{env, net::Ipv6Addr, time::Duration};

use hsin_core::{ManualProxyConfig, ProxyProtocol, UpstreamProxyConfig, UpstreamProxyMode};
use secrecy::SecretString;

use crate::error::{DaemonError, Result};

pub(crate) struct OutboundProxySnapshot {
    pub config: UpstreamProxyConfig,
    pub password: Option<SecretString>,
}

impl OutboundProxySnapshot {
    pub(crate) fn direct() -> Self {
        Self {
            config: UpstreamProxyConfig::default(),
            password: None,
        }
    }
}

pub(crate) struct ClientOptions {
    pub connect_timeout: Duration,
    pub timeout: Option<Duration>,
}

pub(crate) async fn build_client(
    snapshot: &OutboundProxySnapshot,
    options: ClientOptions,
) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(options.connect_timeout);
    if let Some(timeout) = options.timeout {
        builder = builder.timeout(timeout);
    }
    builder = match snapshot.config.mode {
        UpstreamProxyMode::Direct => builder.no_proxy(),
        UpstreamProxyMode::Manual => apply_manual_proxy(
            builder.no_proxy(),
            &snapshot.config.manual,
            snapshot.password.as_ref(),
        )?,
        UpstreamProxyMode::System => apply_system_proxy(builder).await?,
    };
    builder
        .build()
        .map_err(|error| DaemonError::Config(format!("cannot create HTTP client: {error}")))
}

fn apply_manual_proxy(
    builder: reqwest::ClientBuilder,
    manual: &ManualProxyConfig,
    password: Option<&SecretString>,
) -> Result<reqwest::ClientBuilder> {
    manual
        .validate()
        .map_err(|error| DaemonError::Invalid(error.to_string()))?;
    let endpoint = proxy_endpoint(manual.protocol, &manual.host, manual.port);
    let mut proxy = reqwest::Proxy::all(&endpoint)
        .map_err(|error| DaemonError::Invalid(format!("invalid manual proxy: {error}")))?;
    if !manual.username.is_empty() {
        proxy = proxy.basic_auth(
            &manual.username,
            password.map_or("", secrecy::ExposeSecret::expose_secret),
        );
    }
    Ok(builder.proxy(proxy))
}

async fn apply_system_proxy(builder: reqwest::ClientBuilder) -> Result<reqwest::ClientBuilder> {
    // Reqwest's built-in system proxy follows the conventional environment variables. Services
    // often do not inherit them, so use the native desktop proxy as a fallback.
    if proxy_environment_configured() {
        return Ok(builder);
    }
    let native = tokio::task::spawn_blocking(|| {
        let native = systemproxy::SystemProxy::get_system_proxy()?;
        let protocol = native_proxy_protocol(&native);
        Ok::<_, systemproxy::Error>((native, protocol))
    })
    .await
    .map_err(|error| DaemonError::Internal(format!("system proxy lookup failed: {error}")))?;
    let Ok((native, protocol)) = native else {
        return Ok(builder.no_proxy());
    };
    if !native.enable || native.host.trim().is_empty() || native.port == 0 {
        return Ok(builder.no_proxy());
    }
    let endpoint = proxy_endpoint(protocol, &native.host, native.port);
    let mut proxy = reqwest::Proxy::all(endpoint)
        .map_err(|error| DaemonError::Config(format!("invalid system proxy: {error}")))?;
    let bypass = native.bypass.replace(';', ",");
    if !bypass.trim().is_empty() {
        proxy = proxy.no_proxy(reqwest::NoProxy::from_string(&bypass));
    }
    Ok(builder.no_proxy().proxy(proxy))
}

#[cfg(target_os = "linux")]
fn native_proxy_protocol(native: &systemproxy::SystemProxy) -> ProxyProtocol {
    let socks = systemproxy::SystemProxy::get_socks().ok();
    if socks.as_ref().is_some_and(|socks| {
        !socks.host.trim().is_empty()
            && socks.host.eq_ignore_ascii_case(&native.host)
            && socks.port == native.port
    }) {
        ProxyProtocol::Socks5
    } else {
        ProxyProtocol::Http
    }
}

#[cfg(target_os = "macos")]
fn native_proxy_protocol(native: &systemproxy::SystemProxy) -> ProxyProtocol {
    let socks = std::process::Command::new("scutil")
        .arg("--proxy")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|output| macos_socks_proxy(&output));
    if socks
        .as_ref()
        .is_some_and(|(host, port)| host.eq_ignore_ascii_case(&native.host) && *port == native.port)
    {
        ProxyProtocol::Socks5
    } else {
        ProxyProtocol::Http
    }
}

#[cfg(target_os = "macos")]
fn macos_socks_proxy(output: &str) -> Option<(String, u16)> {
    let mut enabled = false;
    let mut host = None;
    let mut port = None;
    for line in output.lines().map(str::trim) {
        let Some((key, value)) = line.split_once(" : ") else {
            continue;
        };
        match key {
            "SOCKSEnable" => enabled = value == "1",
            "SOCKSProxy" => host = Some(value.trim().to_owned()),
            "SOCKSPort" => port = value.trim().parse().ok(),
            _ => {}
        }
    }
    enabled.then(|| Some((host?, port?))).flatten()
}

#[cfg(target_os = "windows")]
const fn native_proxy_protocol(_: &systemproxy::SystemProxy) -> ProxyProtocol {
    ProxyProtocol::Http
}

fn proxy_environment_configured() -> bool {
    [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .iter()
    .any(|name| env::var_os(name).is_some_and(|value| !value.is_empty()))
}

fn proxy_endpoint(protocol: ProxyProtocol, host: &str, port: u16) -> String {
    let host = host.trim();
    let authority = if host.parse::<Ipv6Addr>().is_ok() {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let scheme = match protocol {
        ProxyProtocol::Http => "http",
        // Resolve upstream hostnames at the proxy so a blocked local DNS path does not defeat SOCKS.
        ProxyProtocol::Socks5 => "socks5h",
    };
    format!("{scheme}://{authority}:{port}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    async fn read_http_head(stream: &mut TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while request.len() < 16 * 1024 {
            stream.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
            if request.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        request
    }

    #[test]
    fn proxy_endpoint_brackets_ipv6_and_uses_remote_dns_for_socks() {
        assert_eq!(
            proxy_endpoint(ProxyProtocol::Http, "::1", 7890),
            "http://[::1]:7890"
        );
        assert_eq!(
            proxy_endpoint(ProxyProtocol::Socks5, "proxy.example", 1080),
            "socks5h://proxy.example:1080"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_system_proxy_output_preserves_socks_protocol() {
        let output = r"<dictionary> {
  HTTPEnable : 1
  HTTPPort : 7890
  HTTPProxy : 127.0.0.1
  SOCKSEnable : 1
  SOCKSPort : 1080
  SOCKSProxy : 127.0.0.1
}";
        assert_eq!(macos_socks_proxy(output), Some(("127.0.0.1".into(), 1080)));
        assert_eq!(
            macos_socks_proxy(&output.replace("SOCKSEnable : 1", "SOCKSEnable : 0")),
            None
        );
    }

    #[tokio::test]
    async fn direct_and_authenticated_manual_clients_build() {
        build_client(
            &OutboundProxySnapshot::direct(),
            ClientOptions {
                connect_timeout: Duration::from_secs(1),
                timeout: Some(Duration::from_secs(1)),
            },
        )
        .await
        .unwrap();
        build_client(
            &OutboundProxySnapshot {
                config: UpstreamProxyConfig {
                    mode: UpstreamProxyMode::Manual,
                    manual: ManualProxyConfig {
                        protocol: ProxyProtocol::Socks5,
                        host: "127.0.0.1".into(),
                        port: 1080,
                        username: "user".into(),
                        password_configured: true,
                    },
                },
                password: Some(SecretString::from("password")),
            },
            ClientOptions {
                connect_timeout: Duration::from_secs(1),
                timeout: None,
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn authenticated_http_proxy_receives_absolute_uri_and_basic_auth() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_head(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
            request
        });
        let client = build_client(
            &OutboundProxySnapshot {
                config: UpstreamProxyConfig {
                    mode: UpstreamProxyMode::Manual,
                    manual: ManualProxyConfig {
                        protocol: ProxyProtocol::Http,
                        host: address.ip().to_string(),
                        port: address.port(),
                        username: "proxy-user".into(),
                        password_configured: true,
                    },
                },
                password: Some(SecretString::from("proxy-password")),
            },
            ClientOptions {
                connect_timeout: Duration::from_secs(1),
                timeout: Some(Duration::from_secs(2)),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            client
                .get("http://provider.invalid/v1/models")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "ok"
        );
        let request = String::from_utf8(server.await.unwrap()).unwrap();
        let lower = request.to_ascii_lowercase();
        assert!(lower.starts_with("get http://provider.invalid/v1/models http/1.1\r\n"));
        assert!(
            lower.contains(
                "proxy-authorization: basic cHJveHktdXNlcjpwcm94eS1wYXNzd29yZA=="
                    .to_ascii_lowercase()
                    .as_str()
            )
        );
    }

    #[tokio::test]
    async fn authenticated_socks5_proxy_uses_password_handshake_and_remote_dns() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            let mut greeting = [0_u8; 2];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting[0], 5);
            let mut methods = vec![0_u8; usize::from(greeting[1])];
            stream.read_exact(&mut methods).await.unwrap();
            assert!(methods.contains(&2));
            stream.write_all(&[5, 2]).await.unwrap();

            let mut auth = [0_u8; 2];
            stream.read_exact(&mut auth).await.unwrap();
            assert_eq!(auth[0], 1);
            let mut username = vec![0_u8; usize::from(auth[1])];
            stream.read_exact(&mut username).await.unwrap();
            let mut password_len = [0_u8; 1];
            stream.read_exact(&mut password_len).await.unwrap();
            let mut password = vec![0_u8; usize::from(password_len[0])];
            stream.read_exact(&mut password).await.unwrap();
            stream.write_all(&[1, 0]).await.unwrap();

            let mut request = [0_u8; 4];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(&request[..3], &[5, 1, 0]);
            assert_eq!(request[3], 3, "socks5h must send the domain to the proxy");
            let mut domain_len = [0_u8; 1];
            stream.read_exact(&mut domain_len).await.unwrap();
            let mut domain = vec![0_u8; usize::from(domain_len[0])];
            stream.read_exact(&mut domain).await.unwrap();
            let mut port = [0_u8; 2];
            stream.read_exact(&mut port).await.unwrap();
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
                .await
                .unwrap();

            let request = read_http_head(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
            (
                username == b"proxy-user",
                password == b"proxy-password",
                domain == b"provider.invalid",
                u16::from_be_bytes(port) == 80,
                request.starts_with(b"GET /v1/models HTTP/1.1\r\n"),
            )
        });
        let client = build_client(
            &OutboundProxySnapshot {
                config: UpstreamProxyConfig {
                    mode: UpstreamProxyMode::Manual,
                    manual: ManualProxyConfig {
                        protocol: ProxyProtocol::Socks5,
                        host: address.ip().to_string(),
                        port: address.port(),
                        username: "proxy-user".into(),
                        password_configured: true,
                    },
                },
                password: Some(SecretString::from("proxy-password")),
            },
            ClientOptions {
                connect_timeout: Duration::from_secs(1),
                timeout: Some(Duration::from_secs(2)),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            client
                .get("http://provider.invalid/v1/models")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "ok"
        );
        assert_eq!(server.await.unwrap(), (true, true, true, true, true));
    }
}
