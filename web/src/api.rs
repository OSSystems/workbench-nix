//! The server functions the pages call: on the server they read the host
//! through [`crate::server`], in the browser they are a `POST` to it. Their
//! forms work without JavaScript too.

use leptos::prelude::*;
use leptos::server_fn::codec::GetUrl;
use workbench_model::{HostView, Snapshot};

/// What the viewer sees of the host's workspaces.
#[server(prefix = "/api", endpoint = "snapshot", input = GetUrl)]
pub async fn get_snapshot() -> Result<Snapshot, ServerFnError> {
    let (state, viewer) = crate::server::request()?;
    state.snapshot_for(&viewer).await.map_err(crate::server::failed)
}

/// The host's memory, for admins, with what the side navigation shows.
#[server(prefix = "/api", endpoint = "host", input = GetUrl)]
pub async fn get_host() -> Result<(Snapshot, HostView), ServerFnError> {
    let (state, viewer) = crate::server::request()?;
    if !viewer.admin {
        crate::server::status(axum::http::StatusCode::FORBIDDEN);
        return Err(ServerFnError::new("the host page is for admins"));
    }
    let snap = state.snapshot_for(&viewer).await.map_err(crate::server::failed)?;
    let host = state.host_view().await.map_err(crate::server::failed)?;
    Ok((snap, host))
}

/// Starts, stops or restarts a workspace. A form comes back to the list
/// with the workspace selected, or to the action's page with why it was
/// refused; with `stay`, the caller keeps its page.
#[server(prefix = "/api", endpoint = "control")]
pub async fn control(ws: String, action: String, #[server(default)] stay: bool) -> Result<(), ServerFnError> {
    let (state, viewer) = crate::server::request()?;
    state.control_action(&viewer, &ws, &action, stay).await
}

/// Signs a browser in to a workspace's app: the answer sends it to the
/// app's one-time link, or to the sign-in page with why there is none.
#[server(prefix = "/api", endpoint = "sign-in")]
pub async fn sign_in(ws: String) -> Result<(), ServerFnError> {
    let (state, viewer) = crate::server::request()?;
    state.sign_in_action(&viewer, &ws).await
}
