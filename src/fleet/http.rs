//! HTTP between a front and its nodes, over `WireGuard` or a Unix socket:
//! one request per connection, JSON bodies, the version on every request
//! and answer, and errors as `{"error": …}` with a status.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderValue, Method, Request, Response, StatusCode};
use axum::middleware::Next;
use axum::response::IntoResponse;
use http_body_util::BodyExt as _;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::mpsc;
use workbench_model::fleet::{ApiError, VERSION_HEADER};

use super::{Endpoint, VERSION};

/// How long a connection may take to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A connection to an API: TCP or a Unix socket.
pub enum Conn {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl AsyncRead for Conn {
    fn poll_read(self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Conn::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            Conn::Unix(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Conn {
    fn poll_write(self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Conn::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            Conn::Unix(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Conn::Tcp(s) => Pin::new(s).poll_flush(cx),
            Conn::Unix(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Conn::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            Conn::Unix(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Opens a connection to an endpoint.
pub async fn connect(e: &Endpoint) -> io::Result<Conn> {
    let open = async {
        Ok(match e {
            Endpoint::Tcp(a) => Conn::Tcp(TcpStream::connect(a).await?),
            Endpoint::Unix(p) => Conn::Unix(UnixStream::connect(p).await?),
        })
    };
    tokio::time::timeout(CONNECT_TIMEOUT, open)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, format!("{e} took too long to answer")))?
}

/// Why a call to another machine's API failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// It can't be reached, or the exchange broke or took too long.
    Unreachable(String),
    /// It is of another version, which isn't controlled.
    Version(String),
    /// It refused, or failed, with its status and why.
    Refused { status: u16, error: String },
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Unreachable(why) => write!(f, "doesn't answer: {why}"),
            CallError::Version(v) => write!(f, "is of version {v:?}, not {VERSION}"),
            CallError::Refused { error, .. } => f.write_str(error),
        }
    }
}

impl std::error::Error for CallError {}

impl CallError {
    fn unreachable(e: impl std::fmt::Display) -> Self {
        CallError::Unreachable(e.to_string())
    }

    /// A refusal's status, if it was one.
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        match self {
            CallError::Refused { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// A request's body.
pub type ReqBody = http_body_util::combinators::BoxBody<Bytes, io::Error>;

pub fn empty() -> ReqBody {
    http_body_util::Empty::new().map_err(|never| match never {}).boxed()
}

pub fn json_body<T: serde::Serialize>(v: &T) -> ReqBody {
    let raw = serde_json::to_vec(v).unwrap_or_default();
    http_body_util::Full::new(Bytes::from(raw)).map_err(|never| match never {}).boxed()
}

/// A body of what comes through `rx`, ending when its sender goes.
pub fn stream_body(rx: mpsc::Receiver<Bytes>) -> ReqBody {
    let frames = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|b| (Ok::<_, io::Error>(hyper::body::Frame::data(b)), rx))
    });
    http_body_util::StreamBody::new(frames).boxed()
}

/// Sends one request on a connection of its own, within `timeout` for
/// its answer's head, and checks the answer's version.
pub async fn send(
    e: &Endpoint,
    method: Method,
    path: &str,
    body: ReqBody,
    timeout: Duration,
) -> Result<Response<hyper::body::Incoming>, CallError> {
    let exchange = async {
        let conn = connect(e).await.map_err(CallError::unreachable)?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(conn))
            .await
            .map_err(CallError::unreachable)?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "workbench")
            .header(VERSION_HEADER, VERSION)
            .header("content-type", "application/json")
            .body(body)
            .map_err(CallError::unreachable)?;
        sender.send_request(req).await.map_err(CallError::unreachable)
    };
    let resp = tokio::time::timeout(timeout, exchange)
        .await
        .map_err(|_| CallError::Unreachable(format!("no answer within {}s", timeout.as_secs())))??;
    let theirs = resp.headers().get(VERSION_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("none");
    if theirs != VERSION {
        return Err(CallError::Version(theirs.to_owned()));
    }
    Ok(resp)
}

/// Reads an answer's body, up to `limit` bytes, within `timeout`.
pub async fn read_body(
    resp: Response<hyper::body::Incoming>,
    limit: usize,
    timeout: Duration,
) -> Result<(StatusCode, Bytes), CallError> {
    let status = resp.status();
    let read = http_body_util::Limited::new(resp.into_body(), limit).collect();
    let body = tokio::time::timeout(timeout, read)
        .await
        .map_err(|_| CallError::Unreachable("the answer took too long".into()))?
        .map_err(|e| CallError::Unreachable(format!("the answer: {e}")))?
        .to_bytes();
    Ok((status, body))
}

/// A refusal from its answer.
pub fn refusal(status: StatusCode, body: &[u8]) -> CallError {
    let error = serde_json::from_slice::<ApiError>(body)
        .map_or_else(|_| String::from_utf8_lossy(&body[..body.len().min(4096)]).trim().to_owned(), |e| e.error);
    CallError::Refused { status: status.as_u16(), error }
}

/// Sends a request and reads its JSON answer.
pub async fn call<T: serde::de::DeserializeOwned>(
    e: &Endpoint,
    method: Method,
    path: &str,
    body: ReqBody,
    timeout: Duration,
    limit: usize,
) -> Result<T, CallError> {
    let resp = send(e, method, path, body, timeout).await?;
    let (status, body) = read_body(resp, limit, timeout).await?;
    if !status.is_success() {
        return Err(refusal(status, &body));
    }
    serde_json::from_slice(&body).map_err(|e| CallError::Unreachable(format!("an answer that isn't JSON: {e}")))
}

/// An error answer: a status and why, as JSON.
pub fn error(status: StatusCode, msg: impl Into<String>) -> axum::response::Response {
    (status, axum::Json(ApiError { error: msg.into() })).into_response()
}

/// Something a handler refuses with a status of its own.
#[derive(Debug)]
pub struct Status(pub StatusCode, pub String);

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

impl std::error::Error for Status {}

/// The answer to a failed operation: its own status, 409 for a refusal
/// people are shown, else 500.
#[allow(clippy::needless_pass_by_value)]
pub fn failure(e: anyhow::Error) -> axum::response::Response {
    if let Some(Status(code, msg)) = e.downcast_ref::<Status>() {
        return error(*code, msg.clone());
    }
    if let Some(r) = e.downcast_ref::<crate::self_service::Refused>() {
        return error(StatusCode::CONFLICT, r.0.clone());
    }
    error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// Refuses a request of another version, and marks every answer with
/// this one's.
pub async fn versioned(req: Request<Body>, next: Next) -> axum::response::Response {
    let theirs = req.headers().get(VERSION_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("none").to_owned();
    let mut resp = if theirs == VERSION {
        next.run(req).await
    } else {
        error(StatusCode::CONFLICT, format!("version {theirs:?} asked version {VERSION}; deploy them together"))
    };
    // A build identifier that isn't a header's characters is no one's.
    let ours = HeaderValue::from_str(VERSION).unwrap_or(HeaderValue::from_static("invalid"));
    resp.headers_mut().insert(VERSION_HEADER, ours);
    resp
}

/// Who a request came from: this machine, through the Unix socket, or an
/// address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Peer {
    Local,
    Ip(IpAddr),
}

/// Marks requests that came through the Unix socket.
#[derive(Debug, Clone, Copy)]
pub struct Local;

/// Who a request came from; none for a request that says neither.
#[must_use]
pub fn peer<B>(req: &Request<B>) -> Option<Peer> {
    if req.extensions().get::<Local>().is_some() {
        return Some(Peer::Local);
    }
    req.extensions().get::<ConnectInfo<SocketAddr>>().map(|ConnectInfo(a)| Peer::Ip(a.ip()))
}

/// Serves a router on an address, with each request's peer.
pub async fn serve_tcp(addr: SocketAddr, router: axum::Router) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| anyhow::anyhow!("{addr}: {e}"))?;
    serve_listener(listener, router).await
}

/// Serves a router on a bound TCP listener, with each request's peer.
pub async fn serve_listener(listener: tokio::net::TcpListener, router: axum::Router) -> anyhow::Result<()> {
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

/// Serves a router on a Unix socket only root, and `group` if any, may
/// use; its requests are this machine's.
pub async fn serve_unix(path: &Path, group: &str, router: axum::Router) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::remove_file(path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let listener = tokio::net::UnixListener::bind(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    let mode = if group.is_empty() {
        0o600
    } else {
        let gid = nix::unistd::Group::from_name(group)?.ok_or_else(|| anyhow::anyhow!("no group {group:?}"))?.gid;
        nix::unistd::chown(path, None, Some(gid))?;
        0o660
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    axum::serve(listener, router.layer(axum::Extension(Local))).await?;
    Ok(())
}
