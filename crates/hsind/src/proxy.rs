use std::{future::IntoFuture, net::SocketAddr, sync::Arc};

use axum::{
    Router,
    body::Body,
    extract::{ConnectInfo, State},
    http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode, header},
    routing::any,
};
use futures_util::StreamExt;
use secrecy::ExposeSecret;
use subtle::ConstantTimeEq;
use tokio::{net::TcpListener, sync::Semaphore};

use crate::{
    app::App,
    config::HSIN_MANAGED_KEY,
    error::{DaemonError, Result},
    model::{AuthScheme, ClientKind},
};

#[derive(Clone)]
struct ProxyState {
    app: Arc<App>,
    client: reqwest::Client,
    concurrency: Arc<Semaphore>,
}

pub async fn serve(app: Arc<App>) -> Result<()> {
    let mut runtime = app.subscribe_proxy_runtime();
    loop {
        while !runtime.borrow().enabled {
            tokio::select! {
                () = app.wait_shutdown() => return Ok(()),
                changed = runtime.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                }
            }
        }
        let desired = *runtime.borrow();
        if let Err(error) = serve_enabled(app.clone(), &mut runtime, desired).await {
            app.mark_proxy_active(None);
            tracing::warn!(%error, "proxy failed; retrying while enabled");
            tokio::select! {
                () = app.wait_shutdown() => return Ok(()),
                () = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
                changed = runtime.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                }
            }
        }
        if app.is_shutdown_requested() {
            return Ok(());
        }
    }
}

async fn serve_enabled(
    app: Arc<App>,
    runtime: &mut tokio::sync::watch::Receiver<crate::app::ProxyRuntimeConfig>,
    desired: crate::app::ProxyRuntimeConfig,
) -> Result<()> {
    let listener = TcpListener::bind(desired.address).await?;
    let state = ProxyState {
        app: app.clone(),
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|error| DaemonError::Internal(error.to_string()))?,
        concurrency: Arc::new(Semaphore::new(128)),
    };
    let router = Router::new()
        .route("/codex/v1/{*path}", any(codex))
        .route("/claude/{*path}", any(claude))
        .with_state(state);
    app.mark_proxy_active(Some(desired.address));
    let server = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .into_future();
    tokio::pin!(server);
    let result = loop {
        tokio::select! {
            result = &mut server => break result.map_err(DaemonError::Io),
            () = app.wait_shutdown() => break Ok(()),
            changed = runtime.changed() => {
                if changed.is_err() || *runtime.borrow() != desired {
                    break Ok(());
                }
            }
        }
    };
    app.mark_proxy_active(None);
    result
}

async fn codex(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<ProxyState>,
    request: Request<Body>,
) -> Response<Body> {
    forward(state, ClientKind::Codex, "/codex/v1", peer, request).await
}
async fn claude(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<ProxyState>,
    request: Request<Body>,
) -> Response<Body> {
    forward(state, ClientKind::Claude, "/claude", peer, request).await
}

async fn forward(
    state: ProxyState,
    kind: ClientKind,
    prefix: &str,
    peer: SocketAddr,
    request: Request<Body>,
) -> Response<Body> {
    let Ok(permit) = state.concurrency.clone().try_acquire_owned() else {
        return text_response(
            StatusCode::TOO_MANY_REQUESTS,
            "proxy concurrency limit reached",
        );
    };
    let capability = match state.app.proxy_capability(kind) {
        Ok(value) => value,
        Err(error) => return error_response(&error),
    };
    let managed_key_enabled = state.app.disable_custom_auth(kind).unwrap_or(false);
    if !authorized(
        request.headers(),
        capability.expose_secret(),
        managed_key_enabled,
        peer.ip().is_loopback(),
    ) {
        return text_response(StatusCode::UNAUTHORIZED, "invalid local proxy capability");
    }
    let (provider, secret) = match state.app.upstream_snapshot(kind) {
        Ok(value) => value,
        Err(error) => return error_response(&error),
    };
    let (parts, body) = request.into_parts();
    let upstream = upstream_url(&provider.base_url, prefix, &parts.uri);
    let mut headers = parts.headers;
    remove_hop_by_hop(&mut headers);
    headers.remove(header::AUTHORIZATION);
    headers.remove("x-api-key");
    headers.remove(header::HOST);
    headers.remove(header::CONTENT_LENGTH);
    match provider.auth_scheme {
        AuthScheme::Bearer => {
            if let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", secret.expose_secret()))
            {
                headers.insert(header::AUTHORIZATION, value);
            } else {
                return text_response(StatusCode::BAD_GATEWAY, "invalid upstream credential");
            }
        }
        AuthScheme::XApiKey => {
            if let Ok(value) = HeaderValue::from_str(secret.expose_secret()) {
                headers.insert(HeaderName::from_static("x-api-key"), value);
            } else {
                return text_response(StatusCode::BAD_GATEWAY, "invalid upstream credential");
            }
        }
        AuthScheme::OAuth => {
            return text_response(
                StatusCode::BAD_GATEWAY,
                "Official OAuth providers cannot use the local proxy",
            );
        }
    }
    let outgoing = state
        .client
        .request(parts.method, upstream)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()));
    let response = match outgoing.send().await {
        Ok(response) => response,
        Err(error) => {
            return text_response(
                StatusCode::BAD_GATEWAY,
                &format!("upstream request failed: {error}"),
            );
        }
    };
    let status = response.status();
    let mut builder = Response::builder().status(status);
    let mut response_headers = response.headers().clone();
    remove_hop_by_hop(&mut response_headers);
    for (name, value) in &response_headers {
        builder = builder.header(name, value);
    }
    let stream = response.bytes_stream().map(move |item| {
        let _permit = &permit;
        item
    });
    builder.body(Body::from_stream(stream)).unwrap_or_else(|_| {
        text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to build proxy response",
        )
    })
}

fn authorized(
    headers: &HeaderMap,
    expected: &str,
    managed_key_enabled: bool,
    peer_is_loopback: bool,
) -> bool {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok());
    let bearer_matches =
        bearer.is_some_and(|actual| actual.as_bytes().ct_eq(expected.as_bytes()).into());
    let api_key_matches =
        api_key.is_some_and(|actual| actual.as_bytes().ct_eq(expected.as_bytes()).into());
    let managed_key_matches = managed_key_enabled
        && peer_is_loopback
        && bearer
            .or(api_key)
            .is_some_and(|actual| actual == HSIN_MANAGED_KEY);
    bearer_matches | api_key_matches | managed_key_matches
}

fn upstream_url(base_url: &str, prefix: &str, uri: &axum::http::Uri) -> String {
    let suffix = uri.path().strip_prefix(prefix).unwrap_or(uri.path());
    let mut upstream = format!("{}{}", base_url.trim_end_matches('/'), suffix);
    if let Some(query) = uri.query() {
        upstream.push('?');
        upstream.push_str(query);
    }
    upstream
}

fn remove_hop_by_hop(headers: &mut HeaderMap) {
    let connection_headers = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect::<Vec<_>>();
    for name in connection_headers {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}
fn text_response(status: StatusCode, text: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(text.to_owned()))
        .expect("static response is valid")
}
fn error_response(error: &DaemonError) -> Response<Body> {
    let status = match error {
        DaemonError::Locked => StatusCode::SERVICE_UNAVAILABLE,
        DaemonError::NotFound(_) => StatusCode::BAD_GATEWAY,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    text_response(status, &error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, convert::Infallible, fs, net::Ipv4Addr, path::PathBuf};

    use axum::{body::Bytes, extract::State as AxumState, response::Response as AxumResponse};
    use hsin_core::{Provider, ProviderAddParams, ProviderDraft, SecretInput};
    use http_body_util::BodyExt;
    use parking_lot::Mutex;
    use tokio::{
        sync::{Notify, mpsc},
        task::JoinHandle,
        time::{Duration, timeout},
    };

    use crate::{crypto::KeyStore, paths::Paths};

    #[derive(Default)]
    struct MemoryStore(Mutex<HashMap<u32, String>>);

    impl KeyStore for MemoryStore {
        fn load(&self, version: u32) -> Result<Option<String>> {
            Ok(self.0.lock().get(&version).cloned())
        }

        fn store(&self, version: u32, value: &str) -> Result<()> {
            self.0.lock().insert(version, value.to_owned());
            Ok(())
        }

        fn delete(&self, version: u32) -> Result<()> {
            self.0.lock().remove(&version);
            Ok(())
        }
    }

    struct TestApp {
        app: Arc<App>,
        root: PathBuf,
    }

    impl TestApp {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("hsind-proxy-{}", uuid::Uuid::new_v4()));
            let paths = Paths {
                database: root.join("hsin.sqlite3"),
                lock: root.join("hsind.lock"),
                logs: root.join("logs"),
                backups: root.join("backups"),
                home: root.clone(),
            };
            let app = App::open_with_store(&paths, Arc::new(MemoryStore::default())).unwrap();
            Self { app, root }
        }

        async fn add_provider(
            &self,
            client: ClientKind,
            name: &str,
            base_url: String,
            auth_scheme: AuthScheme,
            secret: &str,
        ) -> Provider {
            self.app
                .add_provider(ProviderAddParams {
                    provider: ProviderDraft {
                        client,
                        name: name.to_owned(),
                        description: String::new(),
                        base_url,
                        auth_scheme,
                        model: None,
                    },
                    secret: SecretInput::Replace(secret.to_owned()),
                })
                .await
                .unwrap()
        }
    }

    impl Drop for TestApp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn proxy_state(app: Arc<App>) -> ProxyState {
        ProxyState {
            app,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            concurrency: Arc::new(Semaphore::new(8)),
        }
    }

    fn proxy_request(uri: &str, capability: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {capability}"))
            .body(Body::empty())
            .unwrap()
    }

    fn loopback_peer() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 12345))
    }

    async fn forward_loopback(
        state: ProxyState,
        kind: ClientKind,
        prefix: &str,
        request: Request<Body>,
    ) -> Response<Body> {
        forward(state, kind, prefix, loopback_peer(), request).await
    }

    async fn spawn_server(router: Router) -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (address, task)
    }

    #[derive(Debug)]
    struct SeenRequest {
        uri: String,
        headers: HeaderMap,
        body: Bytes,
    }

    async fn capture_request(
        AxumState(sender): AxumState<mpsc::UnboundedSender<SeenRequest>>,
        request: Request<Body>,
    ) -> AxumResponse<Body> {
        let (parts, body) = request.into_parts();
        let body = body.collect().await.unwrap().to_bytes();
        sender
            .send(SeenRequest {
                uri: parts.uri.to_string(),
                headers: parts.headers,
                body,
            })
            .unwrap();
        Response::builder()
            .status(StatusCode::ACCEPTED)
            .header(header::CONNECTION, "x-response-hop")
            .header("x-response-hop", "remove")
            .header("x-end-to-end", "preserve")
            .body(Body::from("upstream-response"))
            .unwrap()
    }

    #[test]
    fn capability_auth_accepts_bearer_or_api_key() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer token"),
        );
        assert!(authorized(&headers, "token", false, true));
        assert!(!authorized(&headers, "other", false, true));
        headers.clear();
        headers.insert("x-api-key", HeaderValue::from_static("token"));
        assert!(authorized(&headers, "token", false, true));
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer attacker"),
        );
        assert!(authorized(&headers, "token", false, true));
    }

    #[test]
    fn managed_key_requires_the_client_setting() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer HSIN_MANAGED_KEY"),
        );
        assert!(!authorized(&headers, "capability", false, true));
        assert!(authorized(&headers, "capability", true, true));
        assert!(!authorized(&headers, "capability", true, false));
        headers.clear();
        headers.insert("x-api-key", HeaderValue::from_static("HSIN_MANAGED_KEY"));
        assert!(authorized(&headers, "capability", true, true));
        assert!(!authorized(&headers, "capability", true, false));
    }

    #[test]
    fn codex_path_join_does_not_duplicate_v1_and_preserves_query() {
        let uri = "/codex/v1/responses?stream=true".parse().unwrap();
        assert_eq!(
            upstream_url("https://example.test/v1", "/codex/v1", &uri),
            "https://example.test/v1/responses?stream=true"
        );
    }

    #[test]
    fn connection_header_extensions_are_removed() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, x-hop"),
        );
        headers.insert("x-hop", HeaderValue::from_static("remove"));
        headers.insert("x-keep", HeaderValue::from_static("preserve"));
        remove_hop_by_hop(&mut headers);
        assert!(!headers.contains_key(header::CONNECTION));
        assert!(!headers.contains_key("x-hop"));
        assert!(headers.contains_key("x-keep"));
    }

    #[tokio::test]
    async fn forwards_codex_and_claude_paths_queries_and_rewrites_authentication() {
        let test = TestApp::new();
        let (sender, mut requests) = mpsc::unbounded_channel();
        let router = Router::new()
            .fallback(any(capture_request))
            .with_state(sender);
        let (address, server) = spawn_server(router).await;

        let codex = test
            .add_provider(
                ClientKind::Codex,
                "codex",
                format!("http://{address}/v1"),
                AuthScheme::Bearer,
                "codex-upstream-secret",
            )
            .await;
        let claude = test
            .add_provider(
                ClientKind::Claude,
                "claude",
                format!("http://{address}/anthropic"),
                AuthScheme::XApiKey,
                "claude-upstream-secret",
            )
            .await;
        test.app
            .db
            .set_active(ClientKind::Codex, &codex.id, "synchronized")
            .unwrap();
        test.app
            .db
            .set_active(ClientKind::Claude, &claude.id, "synchronized")
            .unwrap();
        let state = proxy_state(test.app.clone());

        let codex_capability = test.app.proxy_capability(ClientKind::Codex).unwrap();
        let codex_request = Request::builder()
            .method("POST")
            .uri("/codex/v1/responses?stream=true&trace=codex")
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", codex_capability.expose_secret()),
            )
            .header("x-api-key", "remove-client-key")
            .header(header::CONNECTION, "keep-alive, x-request-hop")
            .header("x-request-hop", "remove")
            .header("x-end-to-end-request", "preserve")
            .body(Body::from("codex-body"))
            .unwrap();
        let codex_response =
            forward_loopback(state.clone(), ClientKind::Codex, "/codex/v1", codex_request).await;
        assert_eq!(codex_response.status(), StatusCode::ACCEPTED);
        assert!(!codex_response.headers().contains_key(header::CONNECTION));
        assert!(!codex_response.headers().contains_key("x-response-hop"));
        assert_eq!(codex_response.headers()["x-end-to-end"], "preserve");
        assert_eq!(
            codex_response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes(),
            "upstream-response"
        );
        let seen = requests.recv().await.unwrap();
        assert_eq!(seen.uri, "/v1/responses?stream=true&trace=codex");
        assert_eq!(
            seen.headers[header::AUTHORIZATION],
            "Bearer codex-upstream-secret"
        );
        assert!(!seen.headers.contains_key("x-api-key"));
        assert!(!seen.headers.contains_key("x-request-hop"));
        assert_eq!(seen.headers["x-end-to-end-request"], "preserve");
        assert_eq!(seen.body, "codex-body");

        let claude_capability = test.app.proxy_capability(ClientKind::Claude).unwrap();
        let claude_request = Request::builder()
            .method("POST")
            .uri("/claude/v1/messages?beta=true")
            .header(header::AUTHORIZATION, "Bearer remove-client-token")
            .header("x-api-key", claude_capability.expose_secret())
            .body(Body::from("claude-body"))
            .unwrap();
        let claude_response =
            forward_loopback(state.clone(), ClientKind::Claude, "/claude", claude_request).await;
        assert_eq!(claude_response.status(), StatusCode::ACCEPTED);
        let seen = requests.recv().await.unwrap();
        assert_eq!(seen.uri, "/anthropic/v1/messages?beta=true");
        assert_eq!(seen.headers["x-api-key"], "claude-upstream-secret");
        assert!(!seen.headers.contains_key(header::AUTHORIZATION));
        assert_eq!(seen.body, "claude-body");

        server.abort();
    }

    async fn sse_response(AxumState(release): AxumState<Arc<Notify>>) -> AxumResponse<Body> {
        let stream = futures_util::stream::unfold((0_u8, release), |(step, release)| async move {
            match step {
                0 => Some((
                    Ok::<_, Infallible>(Bytes::from_static(b"data: first\n\n")),
                    (1, release),
                )),
                1 => {
                    release.notified().await;
                    Some((
                        Ok::<_, Infallible>(Bytes::from_static(b"data: second\n\n")),
                        (2, release),
                    ))
                }
                _ => None,
            }
        });
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap()
    }

    #[tokio::test]
    async fn streams_sse_without_buffering_the_upstream_response() {
        let test = TestApp::new();
        let release = Arc::new(Notify::new());
        let router = Router::new()
            .fallback(any(sse_response))
            .with_state(release.clone());
        let (address, server) = spawn_server(router).await;
        let provider = test
            .add_provider(
                ClientKind::Codex,
                "streaming",
                format!("http://{address}/v1"),
                AuthScheme::Bearer,
                "upstream-secret",
            )
            .await;
        test.app
            .db
            .set_active(ClientKind::Codex, &provider.id, "synchronized")
            .unwrap();
        let capability = test.app.proxy_capability(ClientKind::Codex).unwrap();
        let response = timeout(
            Duration::from_secs(2),
            forward(
                proxy_state(test.app.clone()),
                ClientKind::Codex,
                "/codex/v1",
                loopback_peer(),
                proxy_request(
                    "/codex/v1/responses?stream=true",
                    capability.expose_secret(),
                ),
            ),
        )
        .await
        .expect("proxy should return response headers before the SSE stream completes");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        let mut body = response.into_body();
        let first = timeout(Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert_eq!(first, "data: first\n\n");
        assert!(
            timeout(Duration::from_millis(100), body.frame())
                .await
                .is_err()
        );
        release.notify_one();
        let second = timeout(Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert_eq!(second, "data: second\n\n");

        server.abort();
    }

    #[derive(Clone)]
    struct BlockingUpstream {
        arrived: Arc<Notify>,
        release: Arc<Notify>,
        name: &'static str,
    }

    async fn blocking_response(
        AxumState(state): AxumState<BlockingUpstream>,
    ) -> AxumResponse<Body> {
        state.arrived.notify_one();
        state.release.notified().await;
        Response::new(Body::from(state.name))
    }

    async fn named_response(AxumState(name): AxumState<&'static str>) -> AxumResponse<Body> {
        Response::new(Body::from(name))
    }

    #[tokio::test]
    async fn provider_switch_only_affects_requests_started_after_the_switch() {
        let test = TestApp::new();
        let arrived = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (first_address, first_server) =
            spawn_server(Router::new().fallback(any(blocking_response)).with_state(
                BlockingUpstream {
                    arrived: arrived.clone(),
                    release: release.clone(),
                    name: "provider-a",
                },
            ))
            .await;
        let (second_address, second_server) = spawn_server(
            Router::new()
                .fallback(any(named_response))
                .with_state("provider-b"),
        )
        .await;
        let provider_a = test
            .add_provider(
                ClientKind::Codex,
                "provider-a",
                format!("http://{first_address}/v1"),
                AuthScheme::Bearer,
                "secret-a",
            )
            .await;
        let provider_b = test
            .add_provider(
                ClientKind::Codex,
                "provider-b",
                format!("http://{second_address}/v1"),
                AuthScheme::Bearer,
                "secret-b",
            )
            .await;
        test.app
            .db
            .set_active(ClientKind::Codex, &provider_a.id, "synchronized")
            .unwrap();
        let state = proxy_state(test.app.clone());
        let capability = test.app.proxy_capability(ClientKind::Codex).unwrap();
        let first_request = proxy_request("/codex/v1/responses", capability.expose_secret());
        let first_state = state.clone();
        let first = tokio::spawn(async move {
            forward(
                first_state,
                ClientKind::Codex,
                "/codex/v1",
                loopback_peer(),
                first_request,
            )
            .await
        });
        timeout(Duration::from_secs(2), arrived.notified())
            .await
            .expect("the first upstream should receive the in-flight request");
        test.app
            .db
            .set_active(ClientKind::Codex, &provider_b.id, "synchronized")
            .unwrap();
        release.notify_one();
        let first_body = first
            .await
            .unwrap()
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(first_body, "provider-a");

        let second = forward(
            state,
            ClientKind::Codex,
            "/codex/v1",
            loopback_peer(),
            proxy_request("/codex/v1/responses", capability.expose_secret()),
        )
        .await;
        assert_eq!(
            second.into_body().collect().await.unwrap().to_bytes(),
            "provider-b"
        );

        first_server.abort();
        second_server.abort();
    }
}
