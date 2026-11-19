//! Each VM's serial console, shared: one connection to its socket, what it
//! printed last, and every viewer, in the browser or `workbench console` on
//! the host. Everyone types into the same console.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use workbench::config;
use workbench_model::{Viewer, is_workspace_name};

use super::{AppState, lock};

/// How much of a console's output new viewers get first.
const KEEP: usize = 256 << 10;

/// The shared consoles, by workspace.
pub struct Consoles {
    dirs: Vec<PathBuf>,
    m: Mutex<HashMap<String, Arc<Shared>>>,
}

/// One VM's console.
pub struct Shared {
    socket: PathBuf,
    inner: Mutex<Inner>,
    writer: tokio::sync::Mutex<Option<OwnedWriteHalf>>,
}

#[derive(Default)]
struct Inner {
    kept: Vec<u8>,
    viewers: HashMap<u64, ViewerChan>,
    next: u64,
    connected: bool,
}

struct ViewerChan {
    name: String,
    out: mpsc::Sender<Vec<u8>>,
    status: mpsc::Sender<ConsoleStatus>,
}

/// What a viewer is told besides the console's bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConsoleStatus {
    pub connected: bool,
    pub viewers: Vec<String>,
}

/// One viewer's side: the console's bytes and its status, until detached.
pub struct Attached {
    pub id: u64,
    pub kept: Vec<u8>,
    pub out: mpsc::Receiver<Vec<u8>>,
    pub status: mpsc::Receiver<ConsoleStatus>,
}

impl Consoles {
    #[must_use]
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        Consoles { dirs, m: Mutex::default() }
    }

    /// A workspace's console, connecting it the first time.
    pub fn get(&self, name: &str) -> anyhow::Result<Arc<Shared>> {
        let mut m = lock(&self.m);
        if let Some(c) = m.get(name) {
            return Ok(c.clone());
        }
        let c = config::load(&self.dirs, name)?;
        let shared = Arc::new(Shared::new(c.serial()));
        m.insert(name.to_owned(), shared.clone());
        tokio::spawn(shared.clone().run());
        Ok(shared)
    }

    async fn serve_local_conn(&self, conn: UnixStream) {
        let (r, mut w) = conn.into_split();
        let mut r = BufReader::new(r);
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(10), (&mut r).take(64).read_line(&mut line)).await;
        let name = line.trim().to_owned();
        if !matches!(read, Ok(Ok(_))) || !is_workspace_name(&name) {
            let _ = w.write_all(b"ERR invalid workspace name\n").await;
            return;
        }
        let shared = match self.get(&name) {
            Ok(s) => s,
            Err(e) => {
                let _ = w.write_all(format!("ERR {e:#}\n").as_bytes()).await;
                return;
            }
        };
        if w.write_all(b"OK\n").await.is_err() {
            return;
        }
        let mut a = shared.attach("root on the host");
        let id = a.id;
        if w.write_all(&a.kept).await.is_err() {
            shared.detach(id);
            return;
        }
        let input = {
            let shared = shared.clone();
            tokio::spawn(async move {
                let mut buf = [0; 1024];
                while let Ok(n @ 1..) = r.read(&mut buf).await {
                    shared.input(&buf[..n]).await;
                }
                shared.detach(id);
            })
        };
        while let Some(p) = a.out.recv().await {
            if w.write_all(&p).await.is_err() {
                break;
            }
        }
        input.abort();
        shared.detach(id);
    }
}

impl AppState {
    /// Shares the consoles with `workbench console` on the host: it sends
    /// the workspace's name and a newline, gets `OK` or `ERR <why>` back,
    /// and then the connection is the console. Only root and this user
    /// reach the socket.
    pub async fn serve_consoles(&self, path: &Path) -> anyhow::Result<()> {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        loop {
            match listener.accept().await {
                Ok((conn, _)) => {
                    let me = self.clone();
                    tokio::spawn(async move { me.consoles.serve_local_conn(conn).await });
                }
                // Out of descriptors, or a connection gone before it was
                // taken: the consoles stay shared.
                Err(e) => {
                    eprintln!("accepting a console connection: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
}

impl Shared {
    fn new(socket: PathBuf) -> Self {
        Shared { socket, inner: Mutex::default(), writer: tokio::sync::Mutex::default() }
    }

    /// Keeps the console connected while the VM runs, and reconnects when
    /// it starts again.
    async fn run(self: Arc<Self>) {
        loop {
            let Ok(conn) = UnixStream::connect(&self.socket).await else {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            };
            let (mut r, w) = conn.into_split();
            *self.writer.lock().await = Some(w);
            lock(&self.inner).connected = true;
            self.announce();
            let mut buf = vec![0; 32 << 10];
            while let Ok(n @ 1..) = r.read(&mut buf).await {
                self.output(&buf[..n]);
            }
            *self.writer.lock().await = None;
            lock(&self.inner).connected = false;
            self.output(b"\r\n\x1b[2m[the VM's console closed; reconnecting when it starts]\x1b[0m\r\n");
            self.announce();
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// Keeps what the console printed and sends it to every viewer; one
    /// that can't keep up is dropped, and may reconnect.
    pub(crate) fn output(&self, p: &[u8]) {
        let dropped = {
            let mut inner = lock(&self.inner);
            inner.kept.extend_from_slice(p);
            if let Some(over) = inner.kept.len().checked_sub(KEEP).filter(|&o| o > 0) {
                inner.kept.drain(..over);
            }
            let before = inner.viewers.len();
            inner.viewers.retain(|_, v| v.out.try_send(p.to_vec()).is_ok());
            inner.viewers.len() < before
        };
        if dropped {
            self.announce();
        }
    }

    fn status_locked(inner: &Inner) -> ConsoleStatus {
        let mut viewers: Vec<String> = inner.viewers.values().map(|v| v.name.clone()).collect();
        viewers.sort();
        ConsoleStatus { connected: inner.connected, viewers }
    }

    fn announce(&self) {
        let inner = lock(&self.inner);
        let st = Self::status_locked(&inner);
        for v in inner.viewers.values() {
            let _ = v.status.try_send(st.clone());
        }
    }

    /// Adds a viewer: it gets the kept output first, then everything the
    /// console prints.
    pub fn attach(&self, name: &str) -> Attached {
        let (out_tx, out) = mpsc::channel(256);
        let (status_tx, status) = mpsc::channel(4);
        let (id, kept) = {
            let mut inner = lock(&self.inner);
            let id = inner.next;
            inner.next += 1;
            inner.viewers.insert(id, ViewerChan { name: name.to_owned(), out: out_tx, status: status_tx });
            (id, inner.kept.clone())
        };
        self.announce();
        Attached { id, kept, out, status }
    }

    pub fn detach(&self, id: u64) {
        let removed = lock(&self.inner).viewers.remove(&id).is_some();
        if removed {
            self.announce();
        }
    }

    /// Types into the console, if it is connected.
    pub async fn input(&self, p: &[u8]) {
        if let Some(w) = self.writer.lock().await.as_mut() {
            let _ = w.write_all(p).await;
        }
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
    upgrade.on_upgrade(move |ws| watch(ws, shared, viewer.name))
}

async fn watch(ws: WebSocket, shared: Arc<Shared>, who: String) {
    let mut a = shared.attach(&who);
    let id = a.id;
    let (mut tx, mut rx) = ws.split();
    if !a.kept.is_empty() && tx.send(Message::Binary(std::mem::take(&mut a.kept).into())).await.is_err() {
        shared.detach(id);
        return;
    }
    let input = {
        let shared = shared.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = rx.next().await {
                match msg {
                    Message::Binary(p) => shared.input(&p).await,
                    Message::Text(t) => shared.input(t.as_bytes()).await,
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            shared.detach(id);
        })
    };
    loop {
        let sent = tokio::select! {
            p = a.out.recv() => match p {
                Some(p) => tx.send(Message::Binary(p.into())).await,
                None => break,
            },
            st = a.status.recv() => match st {
                Some(st) => tx.send(Message::Text(serde_json::to_string(&st).unwrap_or_default().into())).await,
                None => break,
            },
        };
        if sent.is_err() {
            break;
        }
    }
    input.abort();
    shared.detach(id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fans_out_and_keeps_the_tail() {
        let shared = Shared::new("/nonexistent".into());
        shared.output(b"early ");
        let mut a = shared.attach("alice");
        assert_eq!(a.kept, b"early ");
        let mut b = shared.attach("bob");
        let st = b.status.recv().await.unwrap();
        assert_eq!(st.viewers, ["alice", "bob"]);
        shared.output(b"boot");
        assert_eq!(a.out.recv().await.unwrap(), b"boot");
        assert_eq!(b.out.recv().await.unwrap(), b"boot");
        shared.output(&vec![b'x'; KEEP + 10]);
        assert_eq!(lock(&shared.inner).kept.len(), KEEP);
        shared.detach(b.id);
        assert_eq!(Shared::status_locked(&lock(&shared.inner)).viewers, ["alice"]);
    }

    /// A viewer that stops reading is dropped rather than slowing the rest.
    #[tokio::test]
    async fn drops_a_stuck_viewer() {
        let shared = Shared::new("/nonexistent".into());
        let _stuck = shared.attach("stuck");
        let mut live = shared.attach("live");
        for _ in 0..300 {
            shared.output(b"x");
            let _ = live.out.try_recv();
        }
        let st = Shared::status_locked(&lock(&shared.inner));
        assert_eq!(st.viewers, ["live"]);
        let mut told = None;
        while let Ok(st) = live.status.try_recv() {
            told = Some(st);
        }
        assert_eq!(told.map(|s| s.viewers), Some(vec!["live".to_owned()]), "the others are told");
    }
}
