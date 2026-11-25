//! The server functions the pages call: on the server they read the host
//! through [`crate::server`], in the browser they are a `POST` to it. Their
//! forms work without JavaScript too.

use leptos::prelude::*;
use leptos::server_fn::codec::GetUrl;
use workbench_model::apps::DeviceLink;
use workbench_model::connect::{ConnectView, DeviceRow};
use workbench_model::self_service::NewView;
use workbench_model::{HostView, Snapshot, Viewer};

/// What the viewer sees of the host's workspaces.
#[server(prefix = "/api", endpoint = "snapshot", input = GetUrl)]
pub async fn get_snapshot() -> Result<Snapshot, ServerFnError> {
    let (state, viewer) = crate::server::request()?;
    // Signing in to the UI confirms the viewer's groups for their devices.
    state.devices.seen(&viewer.name, &crate::server::request_groups());
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

/// Opens an app of a workspace in a browser it signs in: the answer sends
/// it to the app's one-time link, or to the app's open page with why there
/// is none.
#[server(prefix = "/api", endpoint = "open")]
pub async fn open(ws: String, app: String) -> Result<(), ServerFnError> {
    let (state, viewer) = crate::server::request()?;
    state.open_action(&viewer, &ws, &app).await
}

/// A one-time link to an app, and its QR code, for another device.
#[server(prefix = "/api", endpoint = "device-link")]
pub async fn make_device_link(ws: String, app: String) -> Result<DeviceLink, ServerFnError> {
    let (state, viewer) = crate::server::request()?;
    state.device_link_action(&viewer, &ws, &app).await
}

/// Gives a workspace made at run time the apps picked, and with `restart`
/// restarts it into them. A form comes back to the list with it selected,
/// or to its apps page with why not.
#[server(prefix = "/api", endpoint = "apps")]
pub async fn set_apps(
    ws: String,
    #[server(default)] apps: Vec<String>,
    #[server(default)] restart: bool,
) -> Result<(), ServerFnError> {
    let (state, viewer) = crate::server::request()?;
    state.apps_action(&viewer, &ws, apps, restart).await
}

/// For admins: gives a workspace made at run time the most memory it grows
/// to, and with `restart` restarts it into it. A form comes back to the
/// list with it selected, or to its memory page with why not.
#[server(prefix = "/api", endpoint = "memory")]
pub async fn set_memory(ws: String, memory_gib: String, #[server(default)] restart: bool) -> Result<(), ServerFnError> {
    let (state, viewer) = crate::server::request()?;
    state.memory_action(&viewer, &ws, &memory_gib, restart).await
}

/// What the New workspace page offers the viewer.
#[server(prefix = "/api", endpoint = "new", input = GetUrl)]
pub async fn get_new() -> Result<(Snapshot, NewView), ServerFnError> {
    let (state, viewer) = crate::server::request()?;
    let new = state.new_view(&viewer).await?;
    let snap = state.snapshot_for(&viewer).await.map_err(crate::server::failed)?;
    Ok((snap, new))
}

/// Makes a workspace of the viewer's own. A form comes back to the list
/// with it selected, or to the New workspace page with why not.
#[server(prefix = "/api", endpoint = "create")]
pub async fn create(
    name: String,
    os: String,
    #[server(default)] ssh_keys: String,
    memory_gib: String,
    disk_gib: String,
    #[server(default)] apps: Vec<String>,
) -> Result<(), ServerFnError> {
    let (state, viewer) = crate::server::request()?;
    let form = crate::server::CreateForm { name, os, ssh_keys, memory_gib, disk_gib, apps };
    state.create_action(&viewer, form).await
}

/// Destroys a workspace made at run time, once its name is typed again.
#[server(prefix = "/api", endpoint = "destroy")]
pub async fn destroy(ws: String, #[server(default)] confirm: String) -> Result<(), ServerFnError> {
    let (state, viewer) = crate::server::request()?;
    state.destroy_action(&viewer, &ws, &confirm).await
}

/// Sets a person's quota, for admins.
// A server function is async, whether it waits or not.
#[allow(clippy::unused_async)]
#[server(prefix = "/api", endpoint = "quota")]
pub async fn quota(user: String, quota: String) -> Result<(), ServerFnError> {
    let (state, viewer) = crate::server::request()?;
    state.quota_action(&viewer, &user, &quota)
}

/// The Connect page of a workspace's app without an address: how to set a
/// computer up, with a fresh pairing code, and the viewer's devices.
#[server(prefix = "/api", endpoint = "connect", input = GetUrl)]
pub async fn get_connect(ws: String) -> Result<(Snapshot, ConnectView), ServerFnError> {
    let (state, viewer) = crate::server::request()?;
    let view = state.connect_view(&viewer, &crate::server::request_groups(), &ws).await?;
    let snap = state.snapshot_for(&viewer).await.map_err(crate::server::failed)?;
    Ok((snap, view))
}

/// The viewer's devices.
// A server function is async, whether it waits or not.
#[allow(clippy::unused_async)]
#[server(prefix = "/api", endpoint = "devices", input = GetUrl)]
pub async fn get_devices() -> Result<(Viewer, Vec<DeviceRow>), ServerFnError> {
    let (state, viewer) = crate::server::request()?;
    state.devices.seen(&viewer.name, &crate::server::request_groups());
    Ok((viewer.clone(), state.device_rows(&viewer.name)))
}

/// Revokes one of the viewer's devices; a form comes back to `back`.
// A server function is async, whether it waits or not.
#[allow(clippy::unused_async)]
#[server(prefix = "/api", endpoint = "revoke-device")]
pub async fn revoke_device(id: String, #[server(default)] back: String) -> Result<(), ServerFnError> {
    let (state, viewer) = crate::server::request()?;
    state.revoke_action(&viewer, &id, &back)
}
