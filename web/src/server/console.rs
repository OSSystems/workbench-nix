//! Each VM's serial console, shared with every viewer, in the browser or
//! `workbench console` on the host: `workbench::console`'s sharing, for
//! the workspaces of this host's configs, each its owner's. On a front,
//! the browser's console is relayed to its node's, which shares it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as NodeMessage;
use workbench::config;
pub use workbench::console::{Attached, ConsoleStatus, Shared};
use workbench::fleet::client::Conn;
use workbench_model::Viewer;

use super::AppState;

/// The shared consoles, by workspace.
pub struct Consoles {
    dirs: Vec<PathBuf>,
    shared: Arc<workbench::console::Consoles>,
}

impl Consoles {
    #[must_use]
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        let configs = dirs.clone();
        let owner_of = Arc::new(move |name: &str| config::load(&configs, name).map(|c| c.owner).unwrap_or_default());
        Consoles { dirs, shared: Arc::new(workbench::console::Consoles::of_owners(owner_of)) }
    }

    /// A workspace's console, connecting it the first time.
    pub fn get(&self, name: &str) -> anyhow::Result<Arc<Shared>> {
        let c = config::load(&self.dirs, name)?;
        Ok(self.shared.get_owned(name, &c.serial(), &c.owner))
    }
}

impl AppState {
    /// Shares the consoles with `workbench console` on the host: it sends
    /// the workspace's name and a newline, gets `OK` or `ERR <why>` back,
    /// and then the connection is the console. Only root and this user
    /// reach the socket.
    pub async fn serve_consoles(&self, path: &Path) -> anyhow::Result<()> {
        let dirs = self.consoles.dirs.clone();
        let serial = move |name: &str| {
            let c = config::load(&dirs, name)?;
            Ok((c.serial(), Some(c.owner)))
        };
        self.consoles.shared.clone().serve_local(path, "root on the host", serial).await
    }
}

/// `GET /w/<ws>/console/socket`: a WebSocket to the shared console. Binary
/// frames carry the console's bytes both ways, text frames its status as
/// JSON.
pub async fn socket(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    axum::Extension(viewer): axum::Extension<Viewer>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let origin = headers.get("origin").and_then(|v| v.to_str().ok());
    if origin != Some(state.opts.origin.as_str()) {
        return (StatusCode::FORBIDDEN, "cross-origin request refused").into_response();
    }
    let mine =
        state.all().await.is_ok_and(|all| all.iter().any(|ws| ws.name == name && ws.configured && viewer.may(ws)));
    if !mine {
        return (StatusCode::FORBIDDEN, "not yours").into_response();
    }
    if let Some(fleet) = state.backend.fleet() {
        return match fleet.console(&name, &viewer.name).await {
            Ok(node) => upgrade.on_upgrade(move |ws| relay(ws, node)),
            Err(e) => (StatusCode::BAD_GATEWAY, super::told_error(&viewer, &e)).into_response(),
        };
    }
    let shared = match state.consoles.get(&name) {
        Ok(s) => s,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    };
    upgrade.on_upgrade(move |ws| workbench::console::watch(ws, shared, viewer.name))
}

/// Passes a browser's console messages to its node's shared console, and
/// the node's back, until either end closes: bytes both ways, and the
/// console's status to the browser.
async fn relay(browser: WebSocket, node: WebSocketStream<Conn>) {
    let (mut to_browser, mut from_browser) = browser.split();
    let (mut to_node, mut from_node) = node.split();
    let up = async {
        while let Some(Ok(msg)) = from_browser.next().await {
            let msg = match msg {
                Message::Binary(b) => NodeMessage::Binary(b),
                Message::Text(t) => NodeMessage::Text(t.as_str().into()),
                Message::Close(_) => break,
                _ => continue,
            };
            if to_node.send(msg).await.is_err() {
                break;
            }
        }
        let _ = to_node.close().await;
    };
    let down = async {
        while let Some(Ok(msg)) = from_node.next().await {
            let msg = match msg {
                NodeMessage::Binary(b) => Message::Binary(b),
                NodeMessage::Text(t) => Message::Text(t.as_str().into()),
                NodeMessage::Close(_) => break,
                _ => continue,
            };
            if to_browser.send(msg).await.is_err() {
                break;
            }
        }
        let _ = to_browser.close().await;
    };
    tokio::select! {
        () = up => {}
        () = down => {}
    }
}
