use std::sync::Arc;

use hsin_core::{
    ClientKind, ImportCurrentParams, ModeSetParams, PROTOCOL_VERSION, ProviderListParams,
    SettingsPatch,
};
use hsin_ipc::{
    HelloParams, HelloResult, IpcListener, JsonRpcRequest, JsonRpcResponse, RpcError, capability,
    method, read_frame, write_frame,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use crate::{
    app::App,
    error::{DaemonError, Result},
};

const MAX_CONNECTIONS: usize = 32;

pub async fn serve(app: Arc<App>) -> Result<()> {
    let endpoint = hsin_ipc::default_endpoint();
    #[cfg(unix)]
    if let hsin_ipc::IpcEndpoint::Filesystem(path) = &endpoint
        && path.exists()
    {
        std::fs::remove_file(path)?;
    }
    let listener =
        IpcListener::bind(endpoint).map_err(|error| DaemonError::Protocol(error.to_string()))?;
    let concurrency = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        tokio::select! {
            () = app.wait_shutdown() => return Ok(()),
            accepted = listener.accept() => {
                let stream=accepted.map_err(|error|DaemonError::Protocol(error.to_string()))?;
                let Ok(permit)=concurrency.clone().try_acquire_owned() else { continue; };
                let app=app.clone();
                tokio::spawn(async move { let _permit=permit; if let Err(error)=serve_connection(app,stream).await { tracing::warn!(%error,"IPC connection closed"); } });
            }
        }
    }
}

async fn serve_connection(
    app: Arc<App>,
    mut stream: interprocess::local_socket::tokio::Stream,
) -> Result<()> {
    let mut hello_complete = false;
    loop {
        let read_result = if hello_complete {
            read_frame(&mut stream).await
        } else {
            tokio::time::timeout(std::time::Duration::from_secs(10), read_frame(&mut stream))
                .await
                .map_err(|_| DaemonError::Protocol("IPC hello timed out".into()))?
        };
        let request: JsonRpcRequest<Value> = match read_result {
            Ok(request) => request,
            Err(hsin_ipc::TransportError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(DaemonError::Protocol(error.to_string())),
        };
        let id = request.id;
        let response = if request.jsonrpc != hsin_ipc::JSON_RPC_VERSION {
            JsonRpcResponse::failure(
                id,
                RpcError::protocol(RpcError::INVALID_REQUEST, "invalid JSON-RPC version"),
            )
        } else if !hello_complete && request.method != method::SYSTEM_HELLO {
            JsonRpcResponse::failure(
                id,
                RpcError::protocol(
                    RpcError::INVALID_REQUEST,
                    "system.hello must be called first",
                ),
            )
        } else if request.method == method::SYSTEM_HELLO {
            match parse::<HelloParams>(request.params).and_then(|params| {
                if params.protocol_version != PROTOCOL_VERSION {
                    return Err(DaemonError::Protocol(format!(
                        "protocol {} is incompatible with {}",
                        params.protocol_version, PROTOCOL_VERSION
                    )));
                }
                Ok(HelloResult {
                    protocol_version: PROTOCOL_VERSION,
                    daemon_version: env!("CARGO_PKG_VERSION").into(),
                    capabilities: vec![
                        capability::PROVIDERS.into(),
                        capability::LOCAL_PROXY.into(),
                        capability::SECURITY.into(),
                        capability::CONFIG_SAGA.into(),
                    ],
                })
            }) {
                Ok(result) => {
                    hello_complete = true;
                    success(id, result)
                }
                Err(error) => failure(id, &error),
            }
        } else {
            dispatch(app.clone(), id, &request.method, request.params).await
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            write_frame(&mut stream, &response),
        )
        .await
        .map_err(|_| DaemonError::Protocol("IPC response timed out".into()))?
        .map_err(|error| DaemonError::Protocol(error.to_string()))?;
    }
}

async fn dispatch(
    app: Arc<App>,
    id: u64,
    method_name: &str,
    params: Value,
) -> JsonRpcResponse<Value> {
    macro_rules! call {
        ($body:expr) => {
            match $body {
                Ok(value) => match serde_json::to_value(value) {
                    Ok(value) => JsonRpcResponse::success(id, value),
                    Err(error) => failure(id, &DaemonError::Json(error)),
                },
                Err(error) => failure(id, &error),
            }
        };
    }
    match method_name {
        method::PROVIDER_LIST => call!(
            parse::<ProviderListParams>(params).and_then(|params| app.list_providers(&params))
        ),
        method::PROVIDER_ADD => call!(async { app.add_provider(parse(params)?).await }.await),
        method::PROVIDER_EDIT => call!(async { app.edit_provider(parse(params)?).await }.await),
        method::PROVIDER_REMOVE => call!(
            async {
                app.remove_provider(parse(params)?)
                    .await
                    .map(|()| json!({"removed":true}))
            }
            .await
        ),
        method::PROVIDER_SWITCH => call!(async { app.switch_provider(parse(params)?).await }.await),
        method::PROVIDER_IMPORT_CURRENT => call!(
            async {
                app.import_current(parse::<ImportCurrentParams>(params)?)
                    .await
            }
            .await
        ),
        method::MODE_SET => call!(
            async {
                let params: ModeSetParams = parse(params)?;
                app.set_mode(params.client, params.mode).await?;
                app.status()
            }
            .await
        ),
        method::STATUS => call!(app.status()),
        method::DOCTOR => call!(app.doctor()),
        method::SETTINGS_GET => call!(app.settings()),
        method::SETTINGS_SET => {
            call!(async { app.update_settings(parse::<SettingsPatch>(params)?).await }.await)
        }
        method::SECURITY_STATUS => call!(app.security_status()),
        method::SECURITY_EXPORT_RECOVERY_KEY => call!(
            app.export_recovery_key()
                .map(|recovery_key| json!({"recovery_key":recovery_key}))
        ),
        method::SECURITY_IMPORT_RECOVERY_KEY => call!(
            parse::<RecoveryKeyParams>(params)
                .and_then(|params| app.import_recovery_key(&params.recovery_key))
                .map(|()| json!({"imported":true}))
        ),
        method::SECURITY_ROTATE_KEY => call!(
            async {
                app.rotate_key()
                    .await
                    .map(|key_version| json!({"key_version":key_version}))
            }
            .await
        ),
        method::CREDENTIAL_RESOLVE => call!(
            parse::<CredentialParams>(params)
                .and_then(|params| {
                    app.credential(
                        params.client,
                        params.provider_id.as_deref(),
                        params.revision,
                    )
                })
                .map(|secret| secret.expose_secret().to_owned())
        ),
        method::DAEMON_SHUTDOWN => {
            app.notify_shutdown();
            JsonRpcResponse::success(id, json!({"shutting_down":true}))
        }
        _ => JsonRpcResponse::failure(
            id,
            RpcError::protocol(
                RpcError::METHOD_NOT_FOUND,
                format!("unknown method {method_name}"),
            ),
        ),
    }
}

#[derive(serde::Deserialize)]
struct RecoveryKeyParams {
    recovery_key: String,
}
#[derive(serde::Deserialize)]
struct CredentialParams {
    client: ClientKind,
    #[serde(default)]
    provider_id: Option<String>,
    #[serde(default)]
    revision: Option<u64>,
}

fn parse<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|error| DaemonError::Invalid(error.to_string()))
}
fn success<T: serde::Serialize>(id: u64, value: T) -> JsonRpcResponse<Value> {
    match serde_json::to_value(value) {
        Ok(value) => JsonRpcResponse::success(id, value),
        Err(error) => failure(id, &DaemonError::Json(error)),
    }
}
fn failure(id: u64, error: &DaemonError) -> JsonRpcResponse<Value> {
    JsonRpcResponse::failure(id, RpcError::application(error.into()))
}
use secrecy::ExposeSecret;
