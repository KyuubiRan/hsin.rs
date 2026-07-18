//! Versioned, authenticated local IPC protocol shared by `hsin` and `hsind`.
//!
//! Each message is a four-byte, big-endian length followed by a UTF-8 JSON-RPC
//! 2.0 document. The length is checked before allocating a payload buffer.

use std::{
    env,
    ffi::OsStr,
    fmt, io,
    path::{Path, PathBuf},
    time::Duration,
};

use hsin_core::AppError;
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, ListenerOptions,
    tokio::{Listener as TokioLocalListener, Stream as TokioLocalStream, prelude::*},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::timeout,
};

#[cfg(windows)]
use interprocess::os::windows::{
    local_socket::ListenerOptionsExt, security_descriptor::SecurityDescriptor,
};
#[cfg(windows)]
use widestring::U16CString;

pub use hsin_core::PROTOCOL_VERSION;

pub const JSON_RPC_VERSION: &str = "2.0";
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;
pub const MAX_IN_FLIGHT_REQUESTS: usize = 64;
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_SOCKET_FILE: &str = "hsind.sock";
pub const INSTALL_HOME_MARKER: &str = ".hsin-home";
pub const INSTALL_HOME_MARKER_CONTENT: &str = "hsin-home-v1\n";

/// Stable RPC method names. Renaming one is a protocol-breaking change.
pub mod method {
    pub const SYSTEM_HELLO: &str = "system.hello";
    pub const PROVIDER_LIST: &str = "provider.list";
    pub const PROVIDER_ADD: &str = "provider.add";
    pub const PROVIDER_EDIT: &str = "provider.edit";
    pub const PROVIDER_REMOVE: &str = "provider.remove";
    pub const PROVIDER_SWITCH: &str = "provider.switch";
    pub const PROVIDER_IMPORT_CURRENT: &str = "provider.import_current";
    pub const MODE_SET: &str = "mode.set";
    pub const STATUS: &str = "status";
    pub const DOCTOR: &str = "doctor";
    pub const SETTINGS_GET: &str = "settings.get";
    pub const SETTINGS_SET: &str = "settings.set";
    pub const SECURITY_STATUS: &str = "security.status";
    pub const SECURITY_EXPORT_RECOVERY_KEY: &str = "security.export_recovery_key";
    pub const SECURITY_IMPORT_RECOVERY_KEY: &str = "security.import_recovery_key";
    pub const SECURITY_ROTATE_KEY: &str = "security.rotate_key";
    pub const CREDENTIAL_RESOLVE: &str = "credential.resolve";
    pub const DAEMON_SHUTDOWN: &str = "daemon.shutdown";
}

/// Optional capabilities negotiated during the mandatory hello exchange.
pub mod capability {
    pub const PROVIDERS: &str = "providers.v1";
    pub const LOCAL_PROXY: &str = "local_proxy.v1";
    pub const SECURITY: &str = "security.v1";
    pub const CONFIG_SAGA: &str = "config_saga.v1";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloParams {
    pub protocol_version: u32,
    pub client_name: String,
    pub client_version: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl HelloParams {
    #[must_use]
    pub fn new(client_name: impl Into<String>, client_version: impl Into<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            client_name: client_name.into(),
            client_version: client_version.into(),
            capabilities: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloResult {
    pub protocol_version: u32,
    pub daemon_version: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest<P = Value> {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    pub params: P,
}

impl<P> JsonRpcRequest<P> {
    #[must_use]
    pub fn new(id: u64, method: impl Into<String>, params: P) -> Self {
        Self {
            jsonrpc: JSON_RPC_VERSION.into(),
            id,
            method: method.into(),
            params,
        }
    }

    /// Check protocol-level request invariants.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::InvalidJsonRpcVersion`] for a non-2.0
    /// request, or [`TransportError::InvalidRequest`] for an empty method.
    pub fn validate(&self) -> Result<(), TransportError> {
        validate_json_rpc_version(&self.jsonrpc)?;
        if self.method.is_empty() {
            return Err(TransportError::InvalidRequest("method is empty"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcNotification<P = Value> {
    pub jsonrpc: String,
    pub method: String,
    pub params: P,
}

impl<P> JsonRpcNotification<P> {
    #[must_use]
    pub fn new(method: impl Into<String>, params: P) -> Self {
        Self {
            jsonrpc: JSON_RPC_VERSION.into(),
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcResponse<R = Value> {
    Success(JsonRpcSuccess<R>),
    Failure(JsonRpcFailure),
}

impl<R> JsonRpcResponse<R> {
    #[must_use]
    pub fn success(id: u64, result: R) -> Self {
        Self::Success(JsonRpcSuccess {
            jsonrpc: JSON_RPC_VERSION.into(),
            id,
            result,
        })
    }

    #[must_use]
    pub fn failure(id: u64, error: RpcError) -> Self {
        Self::Failure(JsonRpcFailure {
            jsonrpc: JSON_RPC_VERSION.into(),
            id,
            error,
        })
    }

    #[must_use]
    pub const fn id(&self) -> u64 {
        match self {
            Self::Success(response) => response.id,
            Self::Failure(response) => response.id,
        }
    }

    /// Return the successful result or the remote RPC error.
    ///
    /// # Errors
    ///
    /// Returns the contained [`RpcError`] for a failed response.
    pub fn into_result(self) -> Result<R, RpcError> {
        match self {
            Self::Success(response) => Ok(response.result),
            Self::Failure(response) => Err(response.error),
        }
    }

    fn jsonrpc(&self) -> &str {
        match self {
            Self::Success(response) => &response.jsonrpc,
            Self::Failure(response) => &response.jsonrpc,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcSuccess<R = Value> {
    pub jsonrpc: String,
    pub id: u64,
    pub result: R,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcFailure {
    pub jsonrpc: String,
    pub id: u64,
    pub error: RpcError,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<AppError>,
}

impl RpcError {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
    pub const APPLICATION_ERROR: i32 = -32000;

    #[must_use]
    pub fn application(error: AppError) -> Self {
        Self {
            code: Self::APPLICATION_ERROR,
            message: error.code.to_string(),
            data: Some(error),
        }
    }

    #[must_use]
    pub fn protocol(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RPC error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

/// Errors raised before an RPC reaches application business logic.
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("local IPC I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("invalid IPC JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("IPC frame is {size} bytes, maximum is {max}")]
    FrameTooLarge { size: usize, max: usize },
    #[error("RPC call timed out after {0:?}")]
    Timeout(Duration),
    #[error("system.hello must be completed before other RPC methods")]
    HandshakeRequired,
    #[error("peer speaks protocol {actual}, expected {expected}")]
    ProtocolMismatch { expected: u32, actual: u32 },
    #[error("response id {actual} did not match request id {expected}")]
    ResponseIdMismatch { expected: u64, actual: u64 },
    #[error("invalid JSON-RPC version {0:?}")]
    InvalidJsonRpcVersion(String),
    #[error("invalid JSON-RPC request: {0}")]
    InvalidRequest(&'static str),
    #[error("RPC failed with code {}: {}", .0.code, .0.message)]
    Rpc(#[from] RpcError),
}

/// Write one length-prefixed JSON frame.
///
/// # Errors
///
/// Returns [`TransportError::FrameTooLarge`] when the serialized payload
/// exceeds [`MAX_FRAME_SIZE`], or an I/O/JSON error from serialization/write.
pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
    T: Serialize + ?Sized,
{
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(TransportError::FrameTooLarge {
            size: payload.len(),
            max: MAX_FRAME_SIZE,
        });
    }
    let length = u32::try_from(payload.len()).map_err(|_| TransportError::FrameTooLarge {
        size: payload.len(),
        max: MAX_FRAME_SIZE,
    })?;
    writer.write_all(&length.to_be_bytes()).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one length-prefixed JSON frame.
///
/// # Errors
///
/// Returns [`TransportError::FrameTooLarge`] before allocating an oversized
/// payload, or an I/O/JSON error from reading/deserialization.
pub async fn read_frame<R, T>(reader: &mut R) -> Result<T, TransportError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut header = [0_u8; 4];
    reader.read_exact(&mut header).await?;
    let size = u32::from_be_bytes(header) as usize;
    if size > MAX_FRAME_SIZE {
        return Err(TransportError::FrameTooLarge {
            size,
            max: MAX_FRAME_SIZE,
        });
    }
    let mut payload = vec![0_u8; size];
    reader.read_exact(&mut payload).await?;
    Ok(serde_json::from_slice(&payload)?)
}

fn validate_json_rpc_version(version: &str) -> Result<(), TransportError> {
    if version == JSON_RPC_VERSION {
        Ok(())
    } else {
        Err(TransportError::InvalidJsonRpcVersion(version.into()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcEndpoint {
    Filesystem(PathBuf),
    Namespaced(String),
}

impl IpcEndpoint {
    #[must_use]
    pub fn filesystem(path: impl Into<PathBuf>) -> Self {
        Self::Filesystem(path.into())
    }

    #[must_use]
    pub fn namespaced(name: impl Into<String>) -> Self {
        Self::Namespaced(name.into())
    }

    fn to_name(&self) -> io::Result<interprocess::local_socket::Name<'_>> {
        match self {
            Self::Filesystem(path) => path.as_path().to_fs_name::<GenericFilePath>(),
            Self::Namespaced(name) => name.as_str().to_ns_name::<GenericNamespaced>(),
        }
    }
}

impl fmt::Display for IpcEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Filesystem(path) => write!(f, "{}", path.display()),
            Self::Namespaced(name) => write!(f, "{name}"),
        }
    }
}

impl From<PathBuf> for IpcEndpoint {
    fn from(path: PathBuf) -> Self {
        Self::Filesystem(path)
    }
}

impl From<&Path> for IpcEndpoint {
    fn from(path: &Path) -> Self {
        Self::Filesystem(path.to_owned())
    }
}

/// Resolve the default endpoint, honoring `HSIN_HOME` first.
#[must_use]
pub fn default_endpoint() -> IpcEndpoint {
    let hsin_home = hsin_home_override();
    resolve_endpoint(
        hsin_home.as_deref().map(Path::as_os_str),
        env::var_os("XDG_RUNTIME_DIR").as_deref(),
        env::var_os("XDG_DATA_HOME").as_deref(),
        env::var_os("LOCALAPPDATA").as_deref(),
        env::var_os("HOME").as_deref(),
        env::var_os("USER")
            .or_else(|| env::var_os("USERNAME"))
            .as_deref(),
    )
}

/// Resolve an explicit or portable installation home. Installed binaries live
/// in `<HSIN_HOME>/bin`, so helpers launched without shell environment can
/// still find the correct daemon instance.
#[must_use]
pub fn hsin_home_override() -> Option<PathBuf> {
    env::var_os("HSIN_HOME")
        .map(PathBuf::from)
        .or_else(installed_home_from_current_exe)
}

/// Resolve the data home shared by storage, service installation, and IPC.
#[must_use]
pub fn data_home() -> PathBuf {
    hsin_home_override().unwrap_or_else(|| {
        platform_data_home(
            env::var_os("XDG_DATA_HOME").as_deref(),
            env::var_os("LOCALAPPDATA").as_deref(),
            env::var_os("HOME").as_deref(),
        )
    })
}

fn installed_home_from_current_exe() -> Option<PathBuf> {
    let executable = env::current_exe().ok()?;
    let name = executable.file_stem()?.to_str()?;
    if !matches!(name, "hsin" | "hsind") {
        return None;
    }
    let bin = executable.parent()?;
    if bin.file_name()? != "bin" {
        return None;
    }
    let home = bin.parent()?;
    let marker = std::fs::read_to_string(home.join(INSTALL_HOME_MARKER)).ok()?;
    (marker == INSTALL_HOME_MARKER_CONTENT).then(|| home.to_path_buf())
}

/// A path-shaped representation retained for command-line overrides.
#[must_use]
pub fn default_socket_path() -> PathBuf {
    match default_endpoint() {
        IpcEndpoint::Filesystem(path) => path,
        IpcEndpoint::Namespaced(name) => {
            #[cfg(windows)]
            {
                PathBuf::from(format!(r"\\.\pipe\{name}"))
            }
            #[cfg(not(windows))]
            {
                PathBuf::from(name)
            }
        }
    }
}

fn resolve_endpoint(
    hsin_home: Option<&OsStr>,
    xdg_runtime: Option<&OsStr>,
    xdg_data: Option<&OsStr>,
    local_app_data: Option<&OsStr>,
    home: Option<&OsStr>,
    user: Option<&OsStr>,
) -> IpcEndpoint {
    #[cfg(windows)]
    {
        // Named pipes are not filesystem entries. Include the current user and
        // an HSIN_HOME scope for portable/test instances. The daemon also
        // installs a current-user ACL.
        let _ = xdg_runtime;
        let user = user
            .and_then(OsStr::to_str)
            .map(sanitize_name)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "user".into());
        let default_home = platform_data_home(xdg_data, local_app_data, home);
        let scope = hsin_home
            .map(PathBuf::from)
            .filter(|value| value != &default_home)
            .map(|value| home_scope(&value))
            .map_or_else(String::new, |value| format!("-{value}"));
        IpcEndpoint::Namespaced(format!("hsin-{user}{scope}-hsind"))
    }
    #[cfg(not(windows))]
    {
        let _ = (local_app_data, user, xdg_runtime);
        if let Some(root) = hsin_home {
            return IpcEndpoint::Filesystem(PathBuf::from(root).join(DEFAULT_SOCKET_FILE));
        }

        let root = platform_data_home(xdg_data, local_app_data, home);

        IpcEndpoint::Filesystem(root.join(DEFAULT_SOCKET_FILE))
    }
}

fn platform_data_home(
    xdg_data: Option<&OsStr>,
    local_app_data: Option<&OsStr>,
    home: Option<&OsStr>,
) -> PathBuf {
    #[cfg(windows)]
    {
        let _ = (xdg_data, home);
        local_app_data
            .map(PathBuf::from)
            .map(|path| path.join("hsin"))
            .unwrap_or_else(|| PathBuf::from(r"C:\hsin"))
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (xdg_data, local_app_data);
        home.map(PathBuf::from).map_or_else(
            || PathBuf::from("/tmp/hsin"),
            |path| path.join("Library/Application Support/hsin"),
        )
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let _ = local_app_data;
        xdg_data
            .map(PathBuf::from)
            .map(|path| path.join("hsin"))
            .or_else(|| {
                home.map(PathBuf::from)
                    .map(|path| path.join(".local/share/hsin"))
            })
            .unwrap_or_else(|| PathBuf::from("/tmp/hsin"))
    }
}

/// Return a stable, non-secret identifier for a data-home path.
#[must_use]
pub fn home_scope(path: &Path) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(windows)]
fn sanitize_name(input: &str) -> String {
    input
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(64)
        .collect()
}

/// A serialized client connection. `&mut self` intentionally prevents
/// interleaving two request/response pairs on the same stream.
pub struct IpcClient {
    stream: TokioLocalStream,
    next_id: u64,
    handshake_complete: bool,
    call_timeout: Duration,
}

impl fmt::Debug for IpcClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IpcClient")
            .field("next_id", &self.next_id)
            .field("handshake_complete", &self.handshake_complete)
            .field("call_timeout", &self.call_timeout)
            .finish_non_exhaustive()
    }
}

impl IpcClient {
    /// Connect to [`default_endpoint`].
    ///
    /// # Errors
    ///
    /// Returns a transport error when the endpoint cannot be resolved or
    /// connected.
    pub async fn connect_default() -> Result<Self, TransportError> {
        Self::connect(default_endpoint()).await
    }

    /// Connect to an explicit local endpoint.
    ///
    /// # Errors
    ///
    /// Returns a transport error when the endpoint cannot be resolved or
    /// connected.
    pub async fn connect(endpoint: impl Into<IpcEndpoint>) -> Result<Self, TransportError> {
        let endpoint = endpoint.into();
        let stream = TokioLocalStream::connect(endpoint.to_name()?).await?;
        Ok(Self::from_stream(stream))
    }

    #[must_use]
    pub fn from_stream(stream: TokioLocalStream) -> Self {
        Self {
            stream,
            next_id: 1,
            handshake_complete: false,
            call_timeout: DEFAULT_CALL_TIMEOUT,
        }
    }

    #[must_use]
    pub const fn handshake_complete(&self) -> bool {
        self.handshake_complete
    }

    pub fn set_call_timeout(&mut self, duration: Duration) {
        self.call_timeout = duration;
    }

    /// Perform the mandatory protocol and capability handshake.
    ///
    /// # Errors
    ///
    /// Returns an RPC/transport error, or [`TransportError::ProtocolMismatch`]
    /// when the daemon implements a different protocol version.
    pub async fn hello(&mut self, params: &HelloParams) -> Result<HelloResult, TransportError> {
        let result: HelloResult = self.call_inner(method::SYSTEM_HELLO, params, true).await?;
        if result.protocol_version != PROTOCOL_VERSION {
            return Err(TransportError::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                actual: result.protocol_version,
            });
        }
        self.handshake_complete = true;
        Ok(result)
    }

    /// Call a daemon method after a successful [`Self::hello`].
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::HandshakeRequired`] before hello, plus any
    /// serialization, I/O, timeout, protocol, or remote RPC error.
    pub async fn call<P, R>(&mut self, method: &str, params: &P) -> Result<R, TransportError>
    where
        P: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        self.call_inner(method, params, false).await
    }

    async fn call_inner<P, R>(
        &mut self,
        method: &str,
        params: &P,
        allow_before_handshake: bool,
    ) -> Result<R, TransportError>
    where
        P: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        if !allow_before_handshake && !self.handshake_complete {
            return Err(TransportError::HandshakeRequired);
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let request = JsonRpcRequest::new(id, method, params);
        let exchange = async {
            write_frame(&mut self.stream, &request).await?;
            let response: JsonRpcResponse<Value> = read_frame(&mut self.stream).await?;
            validate_json_rpc_version(response.jsonrpc())?;
            if response.id() != id {
                return Err(TransportError::ResponseIdMismatch {
                    expected: id,
                    actual: response.id(),
                });
            }
            let value = response.into_result()?;
            Ok(serde_json::from_value(value)?)
        };
        timeout(self.call_timeout, exchange)
            .await
            .map_err(|_| TransportError::Timeout(self.call_timeout))?
    }
}

/// Tokio local listener with platform endpoint resolution and peer checks.
pub struct IpcListener {
    listener: TokioLocalListener,
    endpoint: IpcEndpoint,
}

impl fmt::Debug for IpcListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IpcListener")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl IpcListener {
    /// Bind [`default_endpoint`] with owner-only permissions where supported.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if endpoint setup or binding fails.
    pub fn bind_default() -> Result<Self, TransportError> {
        Self::bind(default_endpoint())
    }

    /// Bind an explicit endpoint with owner-only permissions where supported.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if directory creation, permission tightening, or
    /// listener creation fails.
    pub fn bind(endpoint: impl Into<IpcEndpoint>) -> Result<Self, TransportError> {
        let endpoint = endpoint.into();
        if let IpcEndpoint::Filesystem(path) = &endpoint
            && let Some(parent) = path.parent()
        {
            let parent_existed = parent.exists();
            std::fs::create_dir_all(parent)?;
            // Do not unexpectedly chmod an existing caller-selected parent
            // such as `/tmp`. Directories created for hsin are owner-only.
            if !parent_existed {
                set_owner_only_directory(parent)?;
            }
        }
        let options = ListenerOptions::new().name(endpoint.to_name()?);
        #[cfg(windows)]
        let options = options.security_descriptor(current_user_security_descriptor()?);
        let listener = options.create_tokio()?;
        if let IpcEndpoint::Filesystem(path) = &endpoint {
            set_owner_only_socket(path)?;
        }
        Ok(Self { listener, endpoint })
    }

    #[must_use]
    pub const fn endpoint(&self) -> &IpcEndpoint {
        &self.endpoint
    }

    /// Accept a connection and verify that a Unix peer has the daemon's UID.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when accepting or authenticating the peer fails.
    pub async fn accept(&self) -> Result<TokioLocalStream, TransportError> {
        let stream = self.listener.accept().await?;
        validate_peer(&stream)?;
        Ok(stream)
    }
}

#[cfg(unix)]
fn set_owner_only_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_owner_only_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_socket(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_owner_only_socket(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn validate_peer(stream: &TokioLocalStream) -> io::Result<()> {
    let peer_uid = stream.peer_creds()?.euid().ok_or_else(|| {
        io::Error::new(io::ErrorKind::PermissionDenied, "peer UID is unavailable")
    })?;
    let current_uid = rustix::process::getuid().as_raw();
    if peer_uid != current_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("peer UID {peer_uid} does not match daemon UID {current_uid}"),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_peer(_stream: &TokioLocalStream) -> io::Result<()> {
    // Windows access is enforced by the current-user DACL applied when the
    // named pipe listener is created.
    Ok(())
}

#[cfg(windows)]
fn current_user_security_descriptor() -> io::Result<SecurityDescriptor> {
    let output = std::process::Command::new("whoami.exe")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "whoami.exe could not resolve the current user SID",
        ));
    }
    let output = String::from_utf8_lossy(&output.stdout);
    let start = output.find("S-1-").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "whoami.exe returned no current user SID",
        )
    })?;
    let sid = output[start..]
        .split(|character: char| character == '"' || character == ',' || character.is_whitespace())
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid current user SID"))?;
    let sddl = U16CString::from_str(format!("D:P(A;;GA;;;{sid})"))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    SecurityDescriptor::deserialize(sddl.as_ucstr())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::{AsyncWriteExt, duplex};

    #[tokio::test]
    async fn frame_round_trip() {
        let (mut left, mut right) = duplex(4096);
        let expected = JsonRpcRequest::new(7, method::STATUS, serde_json::json!({}));
        let writer = tokio::spawn(async move { write_frame(&mut left, &expected).await });
        let actual: JsonRpcRequest = read_frame(&mut right).await.unwrap();
        writer.await.unwrap().unwrap();
        assert_eq!(actual.id, 7);
        assert_eq!(actual.method, method::STATUS);
        assert_eq!(actual.jsonrpc, JSON_RPC_VERSION);
    }

    #[tokio::test]
    async fn oversized_inbound_frame_is_rejected_before_payload_read() {
        let (mut left, mut right) = duplex(16);
        let oversize = u32::try_from(MAX_FRAME_SIZE).unwrap() + 1;
        left.write_all(&oversize.to_be_bytes()).await.unwrap();
        let result = read_frame::<_, Value>(&mut right).await;
        assert!(matches!(
            result,
            Err(TransportError::FrameTooLarge {
                size,
                max: MAX_FRAME_SIZE
            }) if size == MAX_FRAME_SIZE + 1
        ));
    }

    #[tokio::test]
    async fn oversized_outbound_frame_is_rejected() {
        let (mut left, _right) = duplex(16);
        let value = "x".repeat(MAX_FRAME_SIZE + 1);
        assert!(matches!(
            write_frame(&mut left, &value).await,
            Err(TransportError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn rpc_response_has_exclusive_result_or_error_shape() {
        let success = JsonRpcResponse::success(3, serde_json::json!({"ok": true}));
        let json = serde_json::to_value(success).unwrap();
        assert!(json.get("result").is_some());
        assert!(json.get("error").is_none());

        let failure: JsonRpcResponse = JsonRpcResponse::failure(
            3,
            RpcError::protocol(RpcError::METHOD_NOT_FOUND, "unknown method"),
        );
        let json = serde_json::to_value(failure).unwrap();
        assert!(json.get("error").is_some());
        assert!(json.get("result").is_none());
    }

    #[test]
    fn hsin_home_has_endpoint_precedence() {
        let endpoint = resolve_endpoint(
            Some(OsStr::new("/custom/hsin")),
            Some(OsStr::new("/run/user/1000")),
            None,
            None,
            Some(OsStr::new("/home/test")),
            Some(OsStr::new("test")),
        );
        #[cfg(not(windows))]
        assert_eq!(
            endpoint,
            IpcEndpoint::Filesystem(PathBuf::from("/custom/hsin/hsind.sock"))
        );
        #[cfg(windows)]
        assert_eq!(
            endpoint,
            IpcEndpoint::Namespaced(format!(
                "hsin-test-{}-hsind",
                home_scope(Path::new("/custom/hsin"))
            ))
        );
    }

    #[test]
    fn hello_defaults_to_current_protocol() {
        let hello = HelloParams::new("hsin", "0.1.0");
        assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
        assert!(hello.capabilities.is_empty());
    }

    #[tokio::test]
    async fn local_transport_enforces_hello_before_calls() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("hsin-ipc-{}-{nonce}", std::process::id()));
        #[cfg(not(windows))]
        let endpoint = IpcEndpoint::filesystem(root.join(DEFAULT_SOCKET_FILE));
        #[cfg(windows)]
        let endpoint = IpcEndpoint::namespaced(format!("hsin-ipc-test-{nonce}"));
        let listener = IpcListener::bind(endpoint.clone()).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.join(DEFAULT_SOCKET_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let hello: JsonRpcRequest = read_frame(&mut stream).await.unwrap();
            hello.validate().unwrap();
            assert_eq!(hello.method, method::SYSTEM_HELLO);
            write_frame(
                &mut stream,
                &JsonRpcResponse::success(
                    hello.id,
                    HelloResult {
                        protocol_version: PROTOCOL_VERSION,
                        daemon_version: "0.1.0".into(),
                        capabilities: vec![capability::PROVIDERS.into()],
                    },
                ),
            )
            .await
            .unwrap();

            let status: JsonRpcRequest = read_frame(&mut stream).await.unwrap();
            status.validate().unwrap();
            assert_eq!(status.method, method::STATUS);
            write_frame(
                &mut stream,
                &JsonRpcResponse::success(status.id, serde_json::json!({"ok": true})),
            )
            .await
            .unwrap();
        });

        let mut client = IpcClient::connect(endpoint).await.unwrap();
        let empty_params = serde_json::json!({});
        let before_hello = client.call::<_, Value>(method::STATUS, &empty_params);
        assert!(matches!(
            before_hello.await,
            Err(TransportError::HandshakeRequired)
        ));
        client
            .hello(&HelloParams::new("hsin-test", "0.1.0"))
            .await
            .unwrap();
        let status: Value = client
            .call(method::STATUS, &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(status, serde_json::json!({"ok": true}));
        server.await.unwrap();

        drop(client);
        let _ = std::fs::remove_dir_all(root);
    }
}
