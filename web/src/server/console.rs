//! Each VM's serial console, shared with every viewer, in the browser or
//! `workbench console` on the host: `workbench::console`'s sharing, for
//! the workspaces of this host's configs, each its owner's.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use workbench::config;
pub use workbench::console::{Attached, ConsoleStatus, Shared};
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
    let shared = match state.consoles.get(&name) {
        Ok(s) => s,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    };
    upgrade.on_upgrade(move |ws| workbench::console::watch(ws, shared, viewer.name))
}
