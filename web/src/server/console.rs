//! Each VM's serial console in the browser: its node shares it, with
//! `workbench console` on the node too, and the front relays it.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as NodeMessage;
use workbench::fleet::client::Conn;
use workbench_model::Viewer;

use super::AppState;

/// `GET /w/<ws>/console/socket`: a WebSocket to the console its node
/// shares. Binary frames carry the console's bytes both ways, text frames
/// its status as JSON.
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
    match state.backend.fleet().console(&name, &viewer.name).await {
        Ok(node) => upgrade.on_upgrade(move |ws| relay(ws, node)),
        Err(e) => (StatusCode::BAD_GATEWAY, super::told_error(&viewer, &e)).into_response(),
    }
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
