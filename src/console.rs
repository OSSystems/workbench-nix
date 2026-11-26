//! Each VM's serial console, shared: one connection to its socket, what it
//! printed last, and every viewer, who all type into the same console. A
//! node shares its workspaces' consoles with the front, and the web UI of
//! a host on its own with the browser; both with `workbench console`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use workbench_model::is_workspace_name;

/// How much of a console's output new viewers get first.
pub const KEEP: usize = 256 << 10;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Who owns a workspace now, by its name; empty for no one.
pub type OwnerOf = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// A workspace's serial socket, and its owner where consoles are owners'.
type Serial = dyn Fn(&str) -> anyhow::Result<(PathBuf, Option<String>)> + Sync;

/// The shared consoles, by workspace.
#[derive(Default)]
pub struct Consoles {
    m: Mutex<HashMap<String, Arc<Shared>>>,
    /// Read again whenever a console reconnects: a VM of the name may be
    /// another owner's workspace by then.
    owner_of: Option<OwnerOf>,
}

/// One VM's console.
pub struct Shared {
    socket: PathBuf,
    name: String,
    owner_of: Option<OwnerOf>,
    inner: Mutex<Inner>,
    writer: tokio::sync::Mutex<Option<OwnedWriteHalf>>,
}

#[derive(Default)]
struct Inner {
    /// Whose workspace it was when last asked: another owner's, after a
    /// workspace of the name was destroyed and made again, gets none of it.
    owner: String,
    kept: Vec<u8>,
    viewers: HashMap<u64, ViewerChan>,
    next: u64,
    connected: bool,
    /// Gone with its workspace: it no longer reconnects.
    gone: bool,
}

struct ViewerChan {
    name: String,
    out: mpsc::Sender<Vec<u8>>,
    status: mpsc::Sender<ConsoleStatus>,
}

/// What a viewer is told besides the console's bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
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
    /// Consoles that are their workspaces' owners': a console another owner
    /// had is forgotten, and its viewers let go.
    #[must_use]
    pub fn of_owners(owner_of: OwnerOf) -> Self {
        Consoles { m: Mutex::default(), owner_of: Some(owner_of) }
    }

    /// A workspace's console on `socket`, connecting it the first time.
    pub fn get(&self, name: &str, socket: &Path) -> Arc<Shared> {
        let mut m = lock(&self.m);
        if let Some(shared) = m.get(name).filter(|s| s.socket == socket) {
            return shared.clone();
        }
        let shared = Arc::new(Shared::new(socket.to_owned(), name, self.owner_of.clone()));
        if let Some(old) = m.insert(name.to_owned(), shared.clone()) {
            old.close();
        }
        tokio::spawn(shared.clone().run());
        shared
    }

    /// A workspace's console, `owner`'s now.
    pub fn get_owned(&self, name: &str, socket: &Path, owner: &str) -> Arc<Shared> {
        let shared = self.get(name, socket);
        shared.owned_by(owner);
        shared
    }

    /// Forgets a workspace's console, which was deleted: a workspace made
    /// again with its name shows nothing of it, and its viewers go.
    pub fn forget(&self, name: &str) {
        if let Some(old) = lock(&self.m).remove(name) {
            old.close();
        }
    }

    /// Shares the consoles on a Unix socket, for `workbench console`: it
    /// sends the workspace's name and a newline, gets `OK` or `ERR <why>`
    /// back, and then the connection is the console, its viewer `viewer`
    /// to the others. `serial` finds a workspace's serial socket and its
    /// owner, if consoles are owners'. Only root and this process's user
    /// reach it.
    pub async fn serve_local<F>(self: Arc<Self>, path: &Path, viewer: &'static str, serial: F) -> anyhow::Result<()>
    where
        F: Fn(&str) -> anyhow::Result<(PathBuf, Option<String>)> + Send + Sync + 'static,
    {
        let _ = std::fs::remove_file(path);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        let serial = Arc::new(serial);
        loop {
            match listener.accept().await {
                Ok((conn, _)) => {
                    let (me, serial) = (self.clone(), serial.clone());
                    tokio::spawn(async move { me.serve_local_conn(conn, viewer, &*serial).await });
                }
                Err(e) => {
                    eprintln!("accepting a console connection: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    async fn serve_local_conn(&self, conn: UnixStream, viewer: &str, serial: &Serial) {
        let (r, mut w) = conn.into_split();
        let mut r = BufReader::new(r);
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(10), (&mut r).take(64).read_line(&mut line)).await;
        let name = line.trim().to_owned();
        if !matches!(read, Ok(Ok(_))) || !is_workspace_name(&name) {
            let _ = w.write_all(b"ERR invalid workspace name\n").await;
            return;
        }
        let shared = match serial(&name) {
            Ok((socket, None)) => self.get(&name, &socket),
            Ok((socket, Some(owner))) => self.get_owned(&name, &socket, &owner),
            Err(e) => {
                let _ = w.write_all(format!("ERR {e:#}\n").as_bytes()).await;
                return;
            }
        };
        if w.write_all(b"OK\n").await.is_err() {
            return;
        }
        let mut a = shared.attach(viewer);
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
                    shared.input(id, &buf[..n]).await;
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

impl Shared {
    fn new(socket: PathBuf, name: &str, owner_of: Option<OwnerOf>) -> Self {
        Shared {
            socket,
            name: name.to_owned(),
            owner_of,
            inner: Mutex::default(),
            writer: tokio::sync::Mutex::default(),
        }
    }

    /// Makes it `owner`'s: a console another owner had is forgotten, and
    /// its viewers let go.
    pub fn owned_by(&self, owner: &str) {
        let mut inner = lock(&self.inner);
        if inner.owner != owner {
            owner.clone_into(&mut inner.owner);
            inner.kept.clear();
            inner.viewers.clear();
        }
    }

    /// Lets its viewers go and stops reconnecting.
    fn close(&self) {
        let mut inner = lock(&self.inner);
        inner.gone = true;
        inner.kept.clear();
        inner.viewers.clear();
    }

    /// Keeps the console connected while the VM runs, and reconnects when
    /// it starts again.
    async fn run(self: Arc<Self>) {
        while !lock(&self.inner).gone {
            let Ok(conn) = UnixStream::connect(&self.socket).await else {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            };
            // A VM of the name may be another workspace's now.
            if let Some(owner_of) = &self.owner_of {
                self.owned_by(&owner_of(&self.name));
            }
            let (mut r, w) = conn.into_split();
            *self.writer.lock().await = Some(w);
            lock(&self.inner).connected = true;
            self.announce();
            let mut buf = vec![0; 32 << 10];
            while let Ok(n @ 1..) = r.read(&mut buf).await {
                if lock(&self.inner).gone {
                    break;
                }
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
    pub fn output(&self, p: &[u8]) {
        let dropped = {
            let mut inner = lock(&self.inner);
            if inner.gone {
                return;
            }
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

    /// Adds a viewer, by the name the others see: it gets the kept output
    /// first, then everything the console prints.
    pub fn attach(&self, name: &str) -> Attached {
        let (out_tx, out) = mpsc::channel(256);
        let (status_tx, status) = mpsc::channel(4);
        let (id, kept) = {
            let mut inner = lock(&self.inner);
            let id = inner.next;
            inner.next += 1;
            if !inner.gone {
                inner.viewers.insert(id, ViewerChan { name: name.to_owned(), out: out_tx, status: status_tx });
            }
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

    /// Types what viewer `id` sent, while it is one: a viewer let go types
    /// into nothing.
    pub async fn input(&self, id: u64, p: &[u8]) {
        if !lock(&self.inner).viewers.contains_key(&id) {
            return;
        }
        if let Some(w) = self.writer.lock().await.as_mut() {
            let _ = w.write_all(p).await;
        }
    }
}

/// Relays a shared console to one viewer: binary frames carry its bytes
/// both ways, text frames its status as JSON.
pub async fn watch(ws: WebSocket, shared: Arc<Shared>, who: String) {
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
                    Message::Binary(p) => shared.input(id, &p).await,
                    Message::Text(t) => shared.input(id, t.as_bytes()).await,
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
        let shared = Shared::new("/nonexistent".into(), "alice", None);
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

    /// A deleted workspace's console shows nothing of it to one made again
    /// with its name, and lets its viewers go.
    #[tokio::test]
    async fn a_deleted_workspace_starts_afresh() {
        let consoles = Consoles::default();
        let socket = Path::new("/nonexistent/serial.sock");
        let old = consoles.get("alice", socket);
        old.output(b"alice's secrets");
        let mut a = old.attach("alice");
        assert!(Arc::ptr_eq(&old, &consoles.get("alice", socket)), "one console per workspace");
        consoles.forget("alice");
        while a.status.try_recv().is_ok() {}
        assert!(a.out.recv().await.is_none(), "its viewer is let go");
        let (ours, mut vm) = UnixStream::pair().unwrap();
        *old.writer.lock().await = Some(ours.into_split().1);
        old.input(a.id, b"rm -rf ~\n").await;
        let fresh = consoles.get("alice", socket);
        let b = fresh.attach("bob");
        assert!(b.kept.is_empty(), "the new one shows nothing of the old");
        drop(old);
        let mut got = Vec::new();
        let _ = tokio::time::timeout(Duration::from_millis(50), vm.read_to_end(&mut got)).await;
        assert!(got.is_empty(), "a viewer let go typed into the VM");
    }

    /// A workspace of the name made again for someone else shows them
    /// nothing of the one before, and lets its viewers go.
    #[tokio::test]
    async fn another_owner_starts_afresh() {
        let shared = Shared::new("/nonexistent".into(), "alice", None);
        shared.owned_by("alice");
        shared.output(b"alice's secrets");
        let mut a = shared.attach("alice");
        shared.owned_by("alice");
        assert_eq!(lock(&shared.inner).kept, b"alice's secrets", "the same owner keeps it");
        shared.owned_by("bob");
        assert!(lock(&shared.inner).kept.is_empty());
        while a.status.try_recv().is_ok() {}
        assert!(a.out.recv().await.is_none(), "alice's viewer is let go");
        let (ours, mut vm) = UnixStream::pair().unwrap();
        *shared.writer.lock().await = Some(ours.into_split().1);
        shared.input(a.id, b"rm -rf ~\n").await;
        let b = shared.attach("bob");
        assert!(b.kept.is_empty());
        shared.input(b.id, b"ls\n").await;
        let mut got = [0; 3];
        vm.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"ls\n", "only bob types into bob's VM");
    }

    /// A viewer that stops reading is dropped rather than slowing the rest.
    #[tokio::test]
    async fn drops_a_stuck_viewer() {
        let shared = Shared::new("/nonexistent".into(), "alice", None);
        let _stuck = shared.attach("stuck");
        let mut live = shared.attach("live");
        for _ in 0..300 {
            shared.output(b"x");
            let _ = live.out.try_recv();
        }
        let st = Shared::status_locked(&lock(&shared.inner));
        assert_eq!(st.viewers, ["live"]);
    }
}
