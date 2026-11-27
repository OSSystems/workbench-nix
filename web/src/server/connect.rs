//! The way in from a terminal, without single sign-on: a person signed in
//! to the UI makes a pairing code; `workbench-connect` on their computer
//! trades it, once, for a device credential, and the credential for SSH
//! certificates in the person's name, and for the tunnel to a workspace's
//! SSH server, which `ssh` reaches through `workbench-connect tunnel`.
//! These endpoints are on a socket of their own, `connect.sock`, which
//! nginx reaches without single sign-on: each checks what it is given
//! itself.
//!
//! The devices are a file of the UI's, `devices.json`, which keeps a hash
//! of each credential, never the credential. Codes live in memory: one
//! the UI made before it restarted is lost, and another is a click away.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, FromRequestParts, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::{SinkExt, StreamExt};
use jiff::Timestamp;
use leptos::prelude::ServerFnError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vmkit::hostutil;
use workbench_model::Viewer;
use workbench_model::connect::{
    CERT_HOURS, CODE_MINUTES, CODE_PREFIX, CREDENTIAL_PREFIX, Cert, CertRequest, DEVICE_DAYS, GROUPS_DAYS, MAX_DEVICES,
    NO_TERMINAL_APP, PairRequest, Paired, Refusal, clean_label, credential_id, is_code,
};
use workbench_model::connect::{ConnectView, DeviceRow};
use workbench_model::self_service::parse_ssh_keys;

use super::{AppState, lock};

/// The most codes a person has waiting; making another drops the oldest.
const MAX_CODES: usize = 8;
/// How often a device's last use is written down.
const USE_EVERY: i64 = 60;
/// How often single sign-on saying the same groups again is written down.
const SEEN_EVERY: i64 = 3600;
/// How often an open tunnel asks again whether it may stay open.
const RECHECK_EVERY: Duration = Duration::from_secs(30);
/// The largest request the endpoints read.
const MAX_BODY: usize = 16 << 10;
/// The largest devices file read.
const MAX_FILE: u64 = 4 << 20;

/// A person's computer, paired with a code they made.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    /// The public part of its credential.
    pub id: String,
    pub user: String,
    /// The person's groups as single sign-on last said them, for the owner
    /// rule of workspaces made at run time.
    #[serde(default)]
    pub groups: Vec<String>,
    pub label: String,
    /// The SHA-256 of its credential, in hex.
    pub hash: String,
    /// In seconds since the epoch, as the others.
    pub created: i64,
    #[serde(default)]
    pub last_used: i64,
    pub expires: i64,
    /// When single sign-on last said the person's groups. A device works
    /// only while that was within [`GROUPS_DAYS`]: someone it no longer
    /// knows, or who left a group, loses the way in as they would in the
    /// browser, once they haven't signed in to the UI for that long.
    #[serde(default)]
    pub groups_seen: i64,
}

impl Device {
    /// Whether it works at `at`: not expired, and its groups confirmed.
    fn works(&self, at: i64) -> bool {
        self.expires > at && self.groups_seen + GROUPS_DAYS * 86_400 > at
    }
}

/// Why a credential doesn't work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unusable {
    /// It expired, was revoked, or never was.
    Unknown,
    /// Single sign-on hasn't said the person's groups for [`GROUPS_DAYS`].
    Unconfirmed,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    devices: Vec<Device>,
}

/// A code the UI made, waiting to be traded.
struct Pending {
    user: String,
    groups: Vec<String>,
    until: Instant,
}

/// The paired devices and the codes waiting.
pub struct Devices {
    file: PathBuf,
    store: Mutex<Store>,
    /// By the hash of the code.
    codes: Mutex<HashMap<String, Pending>>,
}

/// Why pairing refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairError {
    /// The code expired, was used, or never was.
    Code,
    /// The person has all the devices they may.
    TooMany,
    Failed(String),
}

fn hash(secret: &str) -> String {
    hex(&Sha256::digest(secret.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// `n` random bytes, in hex.
fn random_hex(n: usize) -> anyhow::Result<String> {
    let mut buf = vec![0; n];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    Ok(hex(&buf))
}

/// Compares two hashes in time that doesn't depend on where they differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn now() -> i64 {
    Timestamp::now().as_second()
}

impl Devices {
    /// The devices in `file`, none if it doesn't exist yet.
    pub fn load(file: &Path) -> anyhow::Result<Self> {
        let store = match hostutil::read_bounded(file, MAX_FILE) {
            Ok(raw) => serde_json::from_slice(&raw).with_context(|| file.display().to_string())?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Store::default(),
            Err(e) => return Err(e).with_context(|| file.display().to_string()),
        };
        Ok(Devices { file: file.to_owned(), store: Mutex::new(store), codes: Mutex::default() })
    }

    fn save(&self, store: &Store) -> anyhow::Result<()> {
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        hostutil::atomic_write(&self.file, &serde_json::to_vec_pretty(store)?, 0o600)
            .with_context(|| self.file.display().to_string())
    }

    /// A pairing code for `user`, which works once, for 10 minutes.
    pub fn make_code(&self, user: &str, groups: &[String]) -> anyhow::Result<String> {
        self.make_code_for(user, groups, Duration::from_secs(u64::try_from(CODE_MINUTES * 60).unwrap_or(600)))
    }

    pub(crate) fn make_code_for(&self, user: &str, groups: &[String], lasts: Duration) -> anyhow::Result<String> {
        let code = format!("{CODE_PREFIX}{}", random_hex(16)?);
        let mut codes = lock(&self.codes);
        codes.retain(|_, p| p.until > Instant::now());
        let mut theirs: Vec<(String, Instant)> =
            codes.iter().filter(|(_, p)| p.user == user).map(|(h, p)| (h.clone(), p.until)).collect();
        theirs.sort_by_key(|(_, until)| *until);
        for (h, _) in theirs.iter().take((theirs.len() + 1).saturating_sub(MAX_CODES)) {
            codes.remove(h);
        }
        let until = Instant::now() + lasts;
        codes.insert(hash(&code), Pending { user: user.to_owned(), groups: groups.to_vec(), until });
        Ok(code)
    }

    /// Trades a code, once, for a new device's credential.
    pub fn pair(&self, code: &str, label: &str) -> Result<(String, Device), PairError> {
        if !is_code(code) {
            return Err(PairError::Code);
        }
        let pending = lock(&self.codes).remove(&hash(code)).ok_or(PairError::Code)?;
        if pending.until <= Instant::now() {
            return Err(PairError::Code);
        }
        let failed = |e: anyhow::Error| PairError::Failed(format!("{e:#}"));
        let (id, secret) = (random_hex(8).map_err(failed)?, random_hex(32).map_err(failed)?);
        let credential = format!("{CREDENTIAL_PREFIX}{id}-{secret}");
        let at = now();
        let device = Device {
            id,
            user: pending.user,
            groups: pending.groups,
            label: clean_label(label),
            hash: hash(&credential),
            created: at,
            last_used: 0,
            expires: at + DEVICE_DAYS * 86_400,
            groups_seen: at,
        };
        let mut store = lock(&self.store);
        store.devices.retain(|d| d.expires > at);
        if store.devices.iter().filter(|d| d.user == device.user).count() >= MAX_DEVICES {
            return Err(PairError::TooMany);
        }
        store.devices.push(device.clone());
        self.save(&store).map_err(failed)?;
        Ok((credential, device))
    }

    /// The device a credential is of, while it works; its use is noted.
    pub fn authenticate(&self, credential: &str) -> Result<Device, Unusable> {
        let id = credential_id(credential).ok_or(Unusable::Unknown)?;
        let h = hash(credential);
        let at = now();
        let mut store = lock(&self.store);
        let d = store.devices.iter_mut().find(|d| d.id == id).ok_or(Unusable::Unknown)?;
        if !same(&d.hash, &h) || d.expires <= at {
            return Err(Unusable::Unknown);
        }
        if !d.works(at) {
            return Err(Unusable::Unconfirmed);
        }
        let write = at - d.last_used >= USE_EVERY;
        d.last_used = at;
        let device = d.clone();
        if write && let Err(e) = self.save(&store) {
            eprintln!("noting the use of a device: {e:#}");
        }
        Ok(device)
    }

    /// A device, by its id, as it is now, while it works: for what it
    /// opened earlier, such as a tunnel.
    pub fn working(&self, id: &str) -> Option<Device> {
        let at = now();
        lock(&self.store).devices.iter().find(|d| d.id == id && d.works(at)).cloned()
    }

    /// A person's devices that still work, newest first.
    pub fn of(&self, user: &str) -> Vec<Device> {
        let at = now();
        let mut theirs: Vec<Device> =
            lock(&self.store).devices.iter().filter(|d| d.user == user && d.expires > at).cloned().collect();
        theirs.sort_by_key(|d| std::cmp::Reverse(d.created));
        theirs
    }

    /// Revokes one of a person's devices: its credential works no more.
    pub fn revoke(&self, user: &str, id: &str) -> anyhow::Result<bool> {
        let mut store = lock(&self.store);
        let before = store.devices.len();
        store.devices.retain(|d| !(d.user == user && d.id == id));
        let gone = store.devices.len() != before;
        if gone {
            self.save(&store)?;
        }
        Ok(gone)
    }

    /// What single sign-on says of a person's groups now, for their
    /// devices, which it confirms for [`GROUPS_DAYS`] more.
    pub fn seen(&self, user: &str, groups: &[String]) {
        let at = now();
        let mut store = lock(&self.store);
        let mut changed = false;
        for d in store.devices.iter_mut().filter(|d| d.user == user) {
            if d.groups != groups || at - d.groups_seen >= SEEN_EVERY {
                d.groups = groups.to_vec();
                d.groups_seen = at;
                changed = true;
            }
        }
        if changed && let Err(e) = self.save(&store) {
            eprintln!("noting {user}'s groups for their devices: {e:#}");
        }
    }
}

impl AppState {
    /// The Connect page of a workspace's app without an address: a fresh
    /// code for the viewer and their devices, whose groups are noted as
    /// single sign-on says them now. Only for the workspace's owner, as
    /// its SSH server takes only the owner's certificates; and a code only
    /// for an app there is to reach, as each one made drops the oldest
    /// past [`MAX_CODES`].
    pub async fn connect_view(
        &self,
        viewer: &Viewer,
        groups: &[String],
        name: &str,
        app: &str,
    ) -> Result<ConnectView, ServerFnError> {
        let ws = self.find(viewer, name).await?;
        if !viewer.owns(&ws) {
            return Err(super::refuse(
                "this workspace isn't yours: SSH to it signs in as its owner",
                StatusCode::FORBIDDEN,
            ));
        }
        if ws.app(app).is_none_or(|a| a.url.is_some()) {
            return Err(super::refuse(NO_TERMINAL_APP, StatusCode::NOT_FOUND));
        }
        self.devices.seen(&viewer.name, groups);
        let code = self.devices.make_code(&viewer.name, groups).map_err(super::failed)?;
        super::no_store();
        Ok(ConnectView {
            origin: self.opts.origin.clone(),
            host: ws.host().to_owned(),
            code,
            devices: self.device_rows(&viewer.name),
        })
    }

    /// A person's devices, as their page lists them.
    pub fn device_rows(&self, user: &str) -> Vec<DeviceRow> {
        let at = Timestamp::now();
        let when = |s: i64| Timestamp::from_second(s).map(|t| super::view::when(t, at)).unwrap_or_default();
        self.devices
            .of(user)
            .into_iter()
            .map(|d| DeviceRow {
                id: d.id,
                label: d.label,
                created: when(d.created),
                last_used: if d.last_used == 0 { String::new() } else { super::view::ago_s(d.last_used, at) },
                expires: Timestamp::from_second(d.expires).map(|t| super::view::until(t, at)).unwrap_or_default(),
            })
            .collect()
    }

    /// The `revoke_device` server function: one of the viewer's devices
    /// works no more; a form comes back to `back`. So does one for a device
    /// that is gone already, sent twice or from a page left open, or isn't
    /// theirs, which stays as it is.
    pub fn revoke_action(&self, viewer: &Viewer, id: &str, back: &str) -> Result<(), ServerFnError> {
        self.devices.revoke(&viewer.name, id).map_err(super::failed)?;
        // A path of this origin: browsers read `/\host` as `//host`, and
        // drop tabs and newlines, so `/<tab>/host` too.
        let local = back.starts_with('/')
            && !back.starts_with("//")
            && back.bytes().all(|b| b.is_ascii_graphic() && b != b'\\');
        let back = if local { back } else { "/devices" };
        super::see_other(back);
        Ok(())
    }
}

/// The endpoints on `connect.sock`.
pub fn connect_router(state: AppState) -> Router {
    Router::new()
        .route("/connect/pair", post(pair))
        .route("/connect/cert", post(cert))
        .route("/connect/ssh/{ws}", get(tunnel))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(state)
}

fn refused(code: StatusCode, error: &str) -> Response {
    (code, [(header::CACHE_CONTROL, "no-store")], axum::Json(Refusal { error: error.to_owned() })).into_response()
}

/// The device a request's bearer credential is of, while it works.
pub(crate) fn device_of(state: &AppState, headers: &HeaderMap) -> Result<Device, Unusable> {
    let bearer =
        headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    state.devices.authenticate(bearer.ok_or(Unusable::Unknown)?.trim())
}

/// The answer to a credential that doesn't work.
pub(crate) fn unusable(state: &AppState, why: Unusable) -> Response {
    match why {
        Unusable::Unknown => refused(
            StatusCode::UNAUTHORIZED,
            "This computer's credential doesn't work: it expired or was revoked. Set it up again from a workspace's Connect page.",
        ),
        Unusable::Unconfirmed => refused(
            StatusCode::UNAUTHORIZED,
            &format!(
                "You haven't signed in to {} for {GROUPS_DAYS} days: open it in your browser, then try again.",
                state.opts.origin
            ),
        ),
    }
}

/// `POST /connect/pair`: a code, once, for a device credential.
async fn pair(State(state): State<AppState>, body: axum::body::Bytes) -> Response {
    let Ok(req) = serde_json::from_slice::<PairRequest>(&body) else {
        return refused(StatusCode::BAD_REQUEST, "not a pairing request");
    };
    let host_ca = match state.backend.host_ca() {
        Ok(ca) => ca,
        Err(e) => return refused(StatusCode::SERVICE_UNAVAILABLE, &format!("the host has no SSH CA yet: {e:#}")),
    };
    match state.devices.pair(req.code.trim(), &req.label) {
        Ok((credential, d)) => {
            let paired = Paired {
                credential,
                user: d.user,
                domain: state.opts.domain().to_owned(),
                host_ca: host_ca.trim().to_owned(),
                expires: d.expires,
            };
            ([(header::CACHE_CONTROL, "no-store")], axum::Json(paired)).into_response()
        }
        Err(PairError::Code) => refused(
            StatusCode::FORBIDDEN,
            "This code doesn't work: it expired, was used, or never was. Make another on a workspace's Connect page.",
        ),
        Err(PairError::TooMany) => refused(
            StatusCode::CONFLICT,
            "You have all the computers you may. Revoke one on your devices page, then try again.",
        ),
        Err(PairError::Failed(e)) => refused(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// `POST /connect/cert`, with a device's credential: its public key,
/// signed for the person for a few hours.
async fn cert(State(state): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let device = match device_of(&state, &headers) {
        Ok(device) => device,
        Err(why) => return unusable(&state, why),
    };
    let Ok(req) = serde_json::from_slice::<CertRequest>(&body) else {
        return refused(StatusCode::BAD_REQUEST, "not a certificate request");
    };
    let key = match parse_ssh_keys(&req.public_key) {
        Ok(keys) if keys.len() == 1 => keys.into_iter().next().unwrap_or_default(),
        _ => return refused(StatusCode::BAD_REQUEST, "that isn't one SSH public key"),
    };
    let id = format!("{} {}", device.user, device.id);
    match state.backend.sign_user_key(&device.user, &id, &key).await {
        Ok(certificate) => {
            let valid_before = now() + CERT_HOURS * 3600;
            ([(header::CACHE_CONTROL, "no-store")], axum::Json(Cert { certificate, valid_before })).into_response()
        }
        Err(e) => refused(StatusCode::INTERNAL_SERVER_ERROR, &format!("signing the key: {e:#}")),
    }
}

/// `GET /connect/ssh/<ws>`, a WebSocket with a device's credential: a
/// tunnel to the workspace's SSH server, for its owner and the admins, by
/// the owner rule of the access check, while its VM runs. Both are asked
/// at every connection, and the device and the owner rule every
/// [`RECHECK_EVERY`] while it is open: revoking the device closes it. The
/// SSH server then lets in the person the device's certificate names, by
/// name.
async fn tunnel(
    State(state): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    req: axum::extract::Request,
) -> Response {
    let (mut parts, _) = req.into_parts();
    let device = match device_of(&state, &parts.headers) {
        Ok(device) => device,
        Err(why) => return unusable(&state, why),
    };
    let Some(c) = reachable(&state, &device, &name) else {
        return refused(StatusCode::FORBIDDEN, &format!("{name} isn't yours, or doesn't exist."));
    };
    let ws = match state.all().await {
        Ok(all) => all.into_iter().find(|ws| ws.name == c.name),
        Err(e) => return refused(StatusCode::BAD_GATEWAY, &format!("couldn't read the workspaces: {e:#}")),
    };
    // A node that doesn't answer can't say its VM runs.
    match ws {
        Some(ws) if ws.vm == "Running" => {}
        Some(ws) if ws.state == workbench_model::State::Unreachable => {
            let why =
                format!("{name}{} just now, so the UI can't tell it runs. Try again later.", ws.state.busy_suffix());
            return refused(StatusCode::SERVICE_UNAVAILABLE, &why);
        }
        Some(ws) if ws.state == workbench_model::State::Moving => {
            let why = format!("{name}{}. Try again once it's there.", ws.state.busy_suffix());
            return refused(StatusCode::CONFLICT, &why);
        }
        _ => return refused(StatusCode::CONFLICT, &format!("{name} isn't running. Start it from its panel.")),
    }
    let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(ws) => ws,
        Err(e) => return e.into_response(),
    };
    let address = state.backend.ssh_address(&c);
    ws.on_upgrade(move |socket| async move {
        match tokio::net::TcpStream::connect(&address).await {
            Ok(tcp) => {
                tokio::select! {
                    () = pump(socket, tcp) => {}
                    () = until_refused(&state, &device.id, &name) => {
                        eprintln!("closed the tunnel of {} to {name}: no longer theirs, or the device works no more", device.user);
                    }
                }
            }
            Err(e) => eprintln!("the tunnel of {} to {address}: {e}", device.user),
        }
    })
}

/// The workspace `name`, if the owner rule of the access check lets the
/// device's person reach it, with the groups single sign-on last gave
/// them.
fn reachable(state: &AppState, device: &Device, name: &str) -> Option<workbench::config::Config> {
    let viewer = state.viewer(&device.user, &device.groups)?;
    super::authz::reachable_by(state, &viewer, name)
}

/// Whether the device `id` still works and may still reach the workspace
/// `name`, as the device is now.
pub(crate) fn still_reaches(state: &AppState, id: &str, name: &str) -> bool {
    state.devices.working(id).is_some_and(|d| reachable(state, &d, name).is_some())
}

/// Ends once the device `id` no longer works or reaches `name`, asked
/// every [`RECHECK_EVERY`].
async fn until_refused(state: &AppState, id: &str, name: &str) {
    let mut every = tokio::time::interval(RECHECK_EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    every.tick().await;
    loop {
        every.tick().await;
        if !still_reaches(state, id, name) {
            return;
        }
    }
}

/// Passes the WebSocket's binary messages to the SSH server and its bytes
/// back, until either end closes.
async fn pump(socket: WebSocket, tcp: tokio::net::TcpStream) {
    let (mut to_client, mut from_client) = socket.split();
    let (mut from_server, mut to_server) = tcp.into_split();
    let up = async {
        while let Some(Ok(msg)) = from_client.next().await {
            match msg {
                Message::Binary(b) if to_server.write_all(&b).await.is_err() => break,
                Message::Close(_) => break,
                _ => {}
            }
        }
        let _ = to_server.shutdown().await;
    };
    let down = async {
        let mut buf = vec![0; 32 << 10];
        loop {
            match from_server.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to_client.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = to_client.close().await;
    };
    tokio::select! {
        () = up => {}
        () = down => {}
    }
}

/// Signs `key` with the user CA for `user`, for a few hours, with
/// `ssh-keygen`.
pub async fn sign_with(keygen: &str, user_ca: &Path, user: &str, id: &str, key: &str) -> anyhow::Result<String> {
    let dir = std::env::temp_dir().join(format!("workbench-sign-{}", random_hex(8)?));
    std::fs::create_dir(&dir)?;
    let signed = async {
        let public = dir.join("key.pub");
        std::fs::write(&public, format!("{key}\n"))?;
        let out = tokio::process::Command::new(keygen)
            .args(["-q", "-s"])
            .arg(user_ca)
            .args(["-I", id, "-n", user, "-V", &format!("-5m:+{CERT_HOURS}h")])
            .arg(&public)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .context("ssh-keygen")?;
        anyhow::ensure!(out.status.success(), "ssh-keygen: {}", String::from_utf8_lossy(&out.stderr).trim());
        Ok(std::fs::read_to_string(dir.join("key-cert.pub"))?.trim().to_owned())
    }
    .await;
    let _ = std::fs::remove_dir_all(&dir);
    signed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_work_once_and_not_for_long() {
        let dir = tempfile::tempdir().unwrap();
        let d = Devices::load(&dir.path().join("devices.json")).unwrap();
        let code = d.make_code("alice", &["workbench".into()]).unwrap();
        let (credential, device) = d.pair(&code, " laptop\n").unwrap();
        assert_eq!((device.user.as_str(), device.label.as_str()), ("alice", "laptop"));
        assert_eq!(device.groups, ["workbench"]);
        assert_eq!(d.pair(&code, "again"), Err(PairError::Code), "a code works once");
        let late = d.make_code_for("alice", &[], Duration::ZERO).unwrap();
        assert_eq!(d.pair(&late, "late"), Err(PairError::Code), "nor after it expired");
        assert_eq!(d.authenticate(&credential).map(|x| x.id), Ok(device.id.clone()));
        // The file keeps the hash, never the credential.
        let file = std::fs::read_to_string(dir.path().join("devices.json")).unwrap();
        assert!(!file.contains(&credential) && file.contains(&hash(&credential)), "{file}");
        let again = Devices::load(&dir.path().join("devices.json")).unwrap();
        assert!(again.authenticate(&credential).is_ok(), "it lasts a restart");
        let last = if credential.ends_with('a') { 'b' } else { 'a' };
        let wrong = format!("{}{last}", &credential[..credential.len() - 1]);
        assert_eq!(d.authenticate(&wrong).err(), Some(Unusable::Unknown), "another secret of the same id");
        assert!(!d.revoke("bob", &device.id).unwrap(), "only its person revokes it");
        assert!(d.revoke("alice", &device.id).unwrap());
        assert_eq!(d.authenticate(&credential).err(), Some(Unusable::Unknown), "a revoked credential");
        assert!(d.working(&device.id).is_none(), "nor what it opened");
    }

    #[test]
    fn credentials_expire() {
        let dir = tempfile::tempdir().unwrap();
        let d = Devices::load(&dir.path().join("devices.json")).unwrap();
        let (credential, device) = d.pair(&d.make_code("alice", &[]).unwrap(), "x").unwrap();
        lock(&d.store).devices[0].expires = now() - 1;
        assert_eq!(d.authenticate(&credential).err(), Some(Unusable::Unknown));
        assert!(d.of("alice").is_empty(), "nor listed");
        assert_eq!(device.expires - device.created, DEVICE_DAYS * 86_400);
    }

    /// A device works while single sign-on has said its person's groups
    /// within a week, which signing in to the UI does again.
    #[test]
    fn groups_need_confirming() {
        let dir = tempfile::tempdir().unwrap();
        let d = Devices::load(&dir.path().join("devices.json")).unwrap();
        let (credential, device) = d.pair(&d.make_code("alice", &["admins".into()]).unwrap(), "x").unwrap();
        assert!(d.working(&device.id).is_some());
        lock(&d.store).devices[0].groups_seen = now() - GROUPS_DAYS * 86_400;
        assert_eq!(d.authenticate(&credential).err(), Some(Unusable::Unconfirmed));
        assert!(d.working(&device.id).is_none(), "nor what it opened");
        assert_eq!(d.of("alice").len(), 1, "still listed, to revoke");
        d.seen("alice", &[]);
        let again = d.authenticate(&credential).unwrap();
        assert!(again.groups.is_empty(), "as single sign-on says them now");
        // The same groups again are written down at most hourly.
        let seen = again.groups_seen;
        lock(&d.store).devices[0].groups_seen = seen - 10;
        d.seen("alice", &[]);
        assert_eq!(lock(&d.store).devices[0].groups_seen, seen - 10);
        lock(&d.store).devices[0].groups_seen = seen - SEEN_EVERY;
        d.seen("alice", &[]);
        assert!(lock(&d.store).devices[0].groups_seen >= seen);
    }

    /// The host signs with ssh-keygen: a user certificate, for the person
    /// by name, for a few hours.
    #[tokio::test]
    async fn certificates_are_the_persons() {
        let dir = tempfile::tempdir().unwrap();
        let keygen = |args: &[&str]| {
            let out = std::process::Command::new("ssh-keygen").args(args).output().expect("ssh-keygen on the PATH");
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        for name in ["user-ca", "laptop"] {
            keygen(&["-q", "-t", "ed25519", "-N", "", "-f", &dir.path().join(name).to_string_lossy()]);
        }
        let key = std::fs::read_to_string(dir.path().join("laptop.pub")).unwrap();
        let cert =
            sign_with("ssh-keygen", &dir.path().join("user-ca"), "alice", "alice 0123", key.trim()).await.unwrap();
        std::fs::write(dir.path().join("laptop-cert.pub"), &cert).unwrap();
        let shown = keygen(&["-L", "-f", &dir.path().join("laptop-cert.pub").to_string_lossy()]);
        assert!(shown.contains("user certificate") && shown.contains("Key ID: \"alice 0123\""), "{shown}");
        let principals = shown.split("Principals:").nth(1).unwrap().split("Critical").next().unwrap();
        assert_eq!(principals.split_whitespace().collect::<Vec<_>>(), ["alice"], "{shown}");
        assert!(sign_with("ssh-keygen", &dir.path().join("nope"), "alice", "x", key.trim()).await.is_err());
    }

    #[test]
    fn a_person_has_a_few_codes_and_devices() {
        let dir = tempfile::tempdir().unwrap();
        let d = Devices::load(&dir.path().join("devices.json")).unwrap();
        let first = d.make_code("alice", &[]).unwrap();
        for _ in 0..MAX_CODES {
            d.make_code("alice", &[]).unwrap();
        }
        assert_eq!(d.pair(&first, "x"), Err(PairError::Code), "the oldest went");
        for i in 0..MAX_DEVICES {
            d.pair(&d.make_code("alice", &[]).unwrap(), &format!("{i}")).unwrap();
        }
        assert_eq!(d.pair(&d.make_code("alice", &[]).unwrap(), "one more"), Err(PairError::TooMany));
        d.pair(&d.make_code("bob", &[]).unwrap(), "bob's").unwrap();
        d.seen("alice", &["staff".into()]);
        assert!(d.of("alice").iter().all(|x| x.groups == ["staff"]));
        assert!(d.of("bob").iter().all(|x| x.groups.is_empty()));
    }
}
