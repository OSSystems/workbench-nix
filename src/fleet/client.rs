//! The client of the nodes' and the front's APIs. [`Node`] is what the
//! front asks a node, [`NodeClient`] asks it over HTTP and tests replace
//! it; [`FrontClient`] is what a node asks the front; [`console`] opens a
//! node's shared console.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::Method;
use http_body_util::BodyExt as _;
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use workbench_model::Verb;
use workbench_model::fleet::{
    DiskAnswer, HostInfo, MakeRequest, NodeWorkspace, OpenLinkAnswer, OpenLinkRequest, VERSION_HEADER,
};
use workbench_model::{is_user_name, is_workspace_name};

use super::contract::StartAnswer;
use super::http::{self, empty, json_body, stream_body};
pub use super::http::{CallError, Conn};
use super::{Endpoint, VERSION};

/// A future a [`Node`] returns.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What comes of a node's disk: the stream's pieces, and an error if it
/// broke.
pub type DiskOut = mpsc::Receiver<Result<Bytes, String>>;

/// What the front asks a node.
pub trait Node: Send + Sync {
    /// Its memory, disk and version.
    fn host(&self) -> BoxFuture<'_, Result<HostInfo, CallError>>;
    /// Every workspace it has.
    fn workspaces(&self) -> BoxFuture<'_, Result<Vec<NodeWorkspace>, CallError>>;
    /// Makes a workspace from its record.
    fn make<'a>(&'a self, req: &'a MakeRequest) -> BoxFuture<'a, Result<(), CallError>>;
    /// Stops a workspace and deletes it: disk, VM user, tap and seed.
    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), CallError>>;
    /// Starts, stops or restarts it; with `wait`, until systemd is done.
    fn control<'a>(&'a self, name: &'a str, verb: Verb, wait: bool) -> BoxFuture<'a, Result<(), CallError>>;
    /// A link that signs a browser in to one of its apps.
    fn open_link<'a>(&'a self, name: &'a str, app: &'a str) -> BoxFuture<'a, Result<OpenLinkAnswer, CallError>>;
    /// The disk of a stopped workspace, as a stream.
    fn disk_out<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<DiskOut, CallError>>;
    /// Takes a disk stream for a workspace made for a move; its sha256.
    fn disk_in<'a>(&'a self, name: &'a str, input: mpsc::Receiver<Bytes>) -> BoxFuture<'a, Result<String, CallError>>;
}

/// How long a quick question may take.
const QUICK: Duration = Duration::from_secs(10);
/// How long making or deleting a workspace, or a stop that waits, may take:
/// a new disk is a copy of the image.
const SLOW: Duration = Duration::from_secs(660);
/// How long a sign-in link may take, the agent's minute and some.
const OPEN_LINK: Duration = Duration::from_secs(100);
/// How long a disk stream's head may take; the stream takes what it does.
const STREAM_HEAD: Duration = Duration::from_secs(60);
/// The largest answer, but a disk's or a start's.
const MAX_ANSWER: usize = 16 << 20;
/// The largest start answer: up to 64 MiB of secrets, in base64.
const MAX_START: usize = 128 << 20;

/// A node's API, over HTTP.
#[derive(Debug, Clone)]
pub struct NodeClient {
    pub endpoint: Endpoint,
}

impl NodeClient {
    #[must_use]
    pub fn new(endpoint: Endpoint) -> Self {
        NodeClient { endpoint }
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: http::ReqBody,
        timeout: Duration,
    ) -> Result<T, CallError> {
        http::call(&self.endpoint, method, path, body, timeout, MAX_ANSWER).await
    }

    async fn done(&self, method: Method, path: &str, body: http::ReqBody, timeout: Duration) -> Result<(), CallError> {
        let _: serde_json::Value = self.call(method, path, body, timeout).await?;
        Ok(())
    }
}

fn ws_path(name: &str) -> Result<String, CallError> {
    if is_workspace_name(name) {
        Ok(format!("/workspaces/{name}"))
    } else {
        Err(CallError::Refused { status: 400, error: format!("invalid workspace name {name:?}") })
    }
}

impl Node for NodeClient {
    fn host(&self) -> BoxFuture<'_, Result<HostInfo, CallError>> {
        Box::pin(self.call(Method::GET, "/host", empty(), QUICK))
    }

    fn workspaces(&self) -> BoxFuture<'_, Result<Vec<NodeWorkspace>, CallError>> {
        Box::pin(self.call(Method::GET, "/workspaces", empty(), QUICK))
    }

    fn make<'a>(&'a self, req: &'a MakeRequest) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async move { self.done(Method::PUT, &ws_path(&req.record.name)?, json_body(req), SLOW).await })
    }

    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async move { self.done(Method::DELETE, &ws_path(name)?, empty(), SLOW).await })
    }

    fn control<'a>(&'a self, name: &'a str, verb: Verb, wait: bool) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async move {
            let path = format!("{}/{verb}{}", ws_path(name)?, if wait { "?wait=1" } else { "" });
            self.done(Method::POST, &path, empty(), if wait { SLOW } else { QUICK }).await
        })
    }

    fn open_link<'a>(&'a self, name: &'a str, app: &'a str) -> BoxFuture<'a, Result<OpenLinkAnswer, CallError>> {
        Box::pin(async move {
            let body = json_body(&OpenLinkRequest { app: app.to_owned() });
            self.call(Method::POST, &format!("{}/open-link", ws_path(name)?), body, OPEN_LINK).await
        })
    }

    fn disk_out<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<DiskOut, CallError>> {
        Box::pin(async move {
            let path = format!("{}/disk", ws_path(name)?);
            let resp = http::send(&self.endpoint, Method::GET, &path, empty(), STREAM_HEAD).await?;
            if !resp.status().is_success() {
                let (status, body) = http::read_body(resp, MAX_ANSWER, QUICK).await?;
                return Err(http::refusal(status, &body));
            }
            let (tx, rx) = mpsc::channel(8);
            tokio::spawn(async move {
                let mut body = resp.into_body();
                // A source gone silent, as one that lost power is, breaks
                // the stream rather than holding it open for good.
                loop {
                    let frame = match tokio::time::timeout(super::disk::IDLE, body.frame()).await {
                        Ok(Some(frame)) => frame,
                        Ok(None) => return,
                        Err(_) => {
                            let _ = tx.send(Err("the source said nothing for too long".into())).await;
                            return;
                        }
                    };
                    let piece = match frame {
                        Ok(f) => match f.into_data() {
                            Ok(data) => Ok(data),
                            Err(_) => continue,
                        },
                        Err(e) => Err(e.to_string()),
                    };
                    let broke = piece.is_err();
                    if tx.send(piece).await.is_err() || broke {
                        return;
                    }
                }
            });
            Ok(rx)
        })
    }

    fn disk_in<'a>(&'a self, name: &'a str, input: mpsc::Receiver<Bytes>) -> BoxFuture<'a, Result<String, CallError>> {
        Box::pin(async move {
            let path = format!("{}/disk", ws_path(name)?);
            // The answer comes once the whole stream is in.
            let resp = http::send(&self.endpoint, Method::PUT, &path, stream_body(input), Duration::MAX).await?;
            let (status, body) = http::read_body(resp, MAX_ANSWER, QUICK).await?;
            if !status.is_success() {
                return Err(http::refusal(status, &body));
            }
            let a: DiskAnswer = serde_json::from_slice(&body)
                .map_err(|e| CallError::Unreachable(format!("an answer that isn't JSON: {e}")))?;
            Ok(a.sha256)
        })
    }
}

/// The front's API, as a node asks it.
#[derive(Debug, Clone)]
pub struct FrontClient {
    pub endpoint: Endpoint,
}

impl FrontClient {
    /// What starting a workspace needs: refused unless the registry has it
    /// on this node and it isn't moving away.
    pub async fn start(&self, name: &str) -> Result<StartAnswer, CallError> {
        if !is_workspace_name(name) {
            return Err(CallError::Refused { status: 400, error: format!("invalid workspace name {name:?}") });
        }
        http::call(&self.endpoint, Method::POST, &format!("/start/{name}"), empty(), Duration::from_secs(60), MAX_START)
            .await
    }
}

/// A node's shared console: binary messages carry its bytes both ways,
/// text messages its status, as JSON, to the viewer. `viewer` is the name
/// the console's other viewers see.
pub async fn console(endpoint: &Endpoint, name: &str, viewer: &str) -> Result<WebSocketStream<Conn>, CallError> {
    let path = ws_path(name)?;
    let viewer = if is_user_name(viewer) { viewer } else { "someone" };
    let mut req = format!("ws://workbench{path}/console?viewer={viewer}")
        .into_client_request()
        .map_err(|e| CallError::Unreachable(e.to_string()))?;
    req.headers_mut().insert(VERSION_HEADER, VERSION.parse().map_err(|_| CallError::Unreachable("version".into()))?);
    let conn = http::connect(endpoint).await.map_err(|e| CallError::Unreachable(e.to_string()))?;
    match tokio_tungstenite::client_async(req, conn).await {
        Ok((ws, resp)) => {
            let theirs = resp.headers().get(VERSION_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("none");
            if theirs != VERSION {
                return Err(CallError::Version(theirs.to_owned()));
            }
            Ok(ws)
        }
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            let status = resp.status();
            let body = resp.body().clone().unwrap_or_default();
            Err(http::refusal(status, &body))
        }
        Err(e) => Err(CallError::Unreachable(e.to_string())),
    }
}
