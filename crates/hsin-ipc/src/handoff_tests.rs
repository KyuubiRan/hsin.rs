use super::*;

struct Fixture {
    root: PathBuf,
    endpoint: IpcEndpoint,
    listener: IpcListener,
}

impl Fixture {
    fn new() -> Self {
        static NEXT_FIXTURE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT_FIXTURE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        #[cfg(unix)]
        let parent = PathBuf::from("/tmp");
        #[cfg(not(unix))]
        let parent = env::temp_dir();
        let root = parent.join(format!(
            "hsin-handoff-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        #[cfg(not(windows))]
        let endpoint = IpcEndpoint::filesystem(root.join(DEFAULT_SOCKET_FILE));
        #[cfg(windows)]
        let endpoint = IpcEndpoint::namespaced(format!("hsin-handoff-{nonce}-{sequence}"));
        let listener = IpcListener::bind(endpoint.clone()).unwrap();
        Self {
            root,
            endpoint,
            listener,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn owner_hello(code: u32, handoff: bool) -> HelloResult {
    let mut capabilities = vec![capability::CONFIG_OWNERSHIP.into()];
    if handoff {
        capabilities.push(capability::CONFIG_HANDOFF.into());
    }
    HelloResult {
        protocol_version: PROTOCOL_VERSION,
        version_code: code,
        daemon_version: "0.2.9".into(),
        capabilities,
        instance_id: Some("known-owner".into()),
    }
}

fn legacy_rejection() -> RpcError {
    RpcError::application(
        AppError::new(hsin_core::ErrorCode::ProtocolMismatch).with_arg(
            "message",
            format!("protocol error: version code {VERSION_CODE} is incompatible with 33"),
        ),
    )
}

#[tokio::test]
async fn compatible_config_handoff_is_restricted_to_release() {
    let fixture = Fixture::new();
    let endpoint = fixture.endpoint.clone();
    let server = async {
        let mut stream = fixture.listener.accept().await.unwrap();
        let request: JsonRpcRequest<HelloParams> = read_frame(&mut stream).await.unwrap();
        assert!(
            request
                .params
                .capabilities
                .iter()
                .any(|item| item == capability::CONFIG_HANDOFF)
        );
        write_frame(
            &mut stream,
            &JsonRpcResponse::success(request.id, owner_hello(VERSION_CODE + 1, true)),
        )
        .await
        .unwrap();
        let release: JsonRpcRequest = read_frame(&mut stream).await.unwrap();
        assert_eq!(release.method, method::CONFIG_RELEASE);
        write_frame(
            &mut stream,
            &JsonRpcResponse::success(release.id, serde_json::json!({"released":true})),
        )
        .await
        .unwrap();
    };
    let client = async {
        let mut client = IpcClient::connect(endpoint).await.unwrap();
        client
            .hello_for_config_release(&HelloParams::new("handoff-test", "0.2.9"))
            .await
            .unwrap();
        assert!(client.handshake_complete());
        assert!(matches!(
            client.call::<_, Value>(method::STATUS, &Value::Null).await,
            Err(TransportError::InvalidRequest(_))
        ));
        let result: Value = client
            .call(method::CONFIG_RELEASE, &Value::Null)
            .await
            .unwrap();
        assert_eq!(result["released"], true);
    };
    tokio::join!(server, client);
}

#[tokio::test]
async fn ordinary_hello_still_rejects_a_different_version_with_handoff_support() {
    let fixture = Fixture::new();
    let endpoint = fixture.endpoint.clone();
    let server = async {
        let mut stream = fixture.listener.accept().await.unwrap();
        let request: JsonRpcRequest = read_frame(&mut stream).await.unwrap();
        write_frame(
            &mut stream,
            &JsonRpcResponse::success(request.id, owner_hello(VERSION_CODE + 1, true)),
        )
        .await
        .unwrap();
    };
    let client = async {
        let mut client = IpcClient::connect(endpoint).await.unwrap();
        assert!(matches!(
            client
                .hello(&HelloParams::new("ordinary-test", "0.2.9"))
                .await,
            Err(TransportError::VersionCodeMismatch { .. })
        ));
        assert!(!client.handshake_complete());
    };
    tokio::join!(server, client);
}

#[tokio::test]
async fn config_handoff_retries_only_the_known_legacy_owner_version() {
    for returned_code in [33, VERSION_CODE + 1] {
        let fixture = Fixture::new();
        let endpoint = fixture.endpoint.clone();
        let server = async {
            let mut stream = fixture.listener.accept().await.unwrap();
            let first: JsonRpcRequest<HelloParams> = read_frame(&mut stream).await.unwrap();
            assert_eq!(first.params.version_code, VERSION_CODE);
            write_frame(
                &mut stream,
                &JsonRpcResponse::<Value>::failure(first.id, legacy_rejection()),
            )
            .await
            .unwrap();
            let retry: JsonRpcRequest<HelloParams> = read_frame(&mut stream).await.unwrap();
            assert_eq!(retry.params.version_code, 33);
            assert_ne!(retry.id, first.id);
            write_frame(
                &mut stream,
                &JsonRpcResponse::success(retry.id, owner_hello(returned_code, false)),
            )
            .await
            .unwrap();
        };
        let client = async {
            let mut client = IpcClient::connect(endpoint).await.unwrap();
            let result = client
                .hello_for_config_release(&HelloParams::new("handoff-test", "0.2.9"))
                .await;
            assert_eq!(result.is_ok(), returned_code == 33);
            assert_eq!(client.handshake_complete(), returned_code == 33);
        };
        tokio::join!(server, client);
    }
}

#[tokio::test]
async fn config_handoff_rejects_missing_identity_capability_and_protocol() {
    let mut missing_identity = owner_hello(VERSION_CODE, true);
    missing_identity.instance_id = None;
    let mut missing_capability = owner_hello(VERSION_CODE, true);
    missing_capability
        .capabilities
        .retain(|item| item != capability::CONFIG_OWNERSHIP);
    let mut wrong_protocol = owner_hello(VERSION_CODE, true);
    wrong_protocol.protocol_version = PROTOCOL_VERSION + 1;
    for hello in [
        missing_identity,
        missing_capability,
        wrong_protocol,
        owner_hello(VERSION_CODE + 1, false),
        owner_hello(0, true),
    ] {
        let fixture = Fixture::new();
        let endpoint = fixture.endpoint.clone();
        let server = async {
            let mut stream = fixture.listener.accept().await.unwrap();
            let request: JsonRpcRequest = read_frame(&mut stream).await.unwrap();
            write_frame(&mut stream, &JsonRpcResponse::success(request.id, hello))
                .await
                .unwrap();
        };
        let client = async {
            let mut client = IpcClient::connect(endpoint).await.unwrap();
            assert!(
                client
                    .hello_for_config_release(&HelloParams::new("handoff-test", "0.2.9"))
                    .await
                    .is_err()
            );
            assert!(!client.handshake_complete());
        };
        tokio::join!(server, client);
    }
}

#[tokio::test]
async fn config_handoff_does_not_retry_unknown_owner_errors() {
    let fixture = Fixture::new();
    let endpoint = fixture.endpoint.clone();
    let server = async {
        let mut stream = fixture.listener.accept().await.unwrap();
        let request: JsonRpcRequest = read_frame(&mut stream).await.unwrap();
        let rejection = RpcError::application(
            AppError::new(hsin_core::ErrorCode::ProtocolMismatch).with_arg(
                "message",
                format!("protocol error: version code {VERSION_CODE} is incompatible with 32"),
            ),
        );
        write_frame(
            &mut stream,
            &JsonRpcResponse::<Value>::failure(request.id, rejection),
        )
        .await
        .unwrap();
    };
    let client = async {
        let mut client = IpcClient::connect(endpoint).await.unwrap();
        assert!(matches!(
            client
                .hello_for_config_release(&HelloParams::new("handoff-test", "0.2.9"))
                .await,
            Err(TransportError::Rpc(_))
        ));
        assert!(!client.handshake_complete());
    };
    tokio::join!(server, client);
}
