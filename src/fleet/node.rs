//! `workbench node serve`: the node's API, root's, which answers the
//! front's `WireGuard` address only, and this machine through a Unix socket
//! on a machine with both roles. Requests and answers are JSON, and none
//! runs a command the caller names.
//!
//! | Method | Path | What |
//! |---|---|---|
//! | `GET` | `/host` | memory, pools, the disks' file system, the version |
//! | `GET` | `/workspaces` | every workspace, as `workbench status` collects it, with `booted.json` |
//! | `PUT` | `/workspaces/<ws>` | makes it from a record: VM user, tap, seed and, unless for a move, disk |
//! | `DELETE` | `/workspaces/<ws>` | stops it and deletes its disk, VM user, tap and seed |
//! | `POST` | `/workspaces/<ws>/start`, `stop`, `restart` | as `workbench start`, `stop` and `restart`; `?wait=1` until systemd is done |
//! | `POST` | `/workspaces/<ws>/open-link` | the agent's `open-link` |
//! | `GET` | `/workspaces/<ws>/console` | WebSocket to the shared serial console; `?viewer=<name>` |
//! | `GET` | `/workspaces/<ws>/disk` | the disk of a stopped workspace, as [`super::disk`] streams it |
//! | `PUT` | `/workspaces/<ws>/disk` | takes such a stream for a workspace made for a move; its sha256 |

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail, ensure};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use tokio::sync::mpsc;
use vmkit::hostutil::{self, Account};
use vmkit::memory::MIB;
use workbench_model::fleet::{
    DiskAnswer, HostInfo, MakeRequest, NodeWorkspace, OpenLinkAnswer, OpenLinkRequest, PoolInfo, PoolStatus,
};
use workbench_model::{Verb, is_workspace_name};

use super::client::BoxFuture;
use super::http::{self, Peer, Status, error, failure};
use super::{NodeSettings, VERSION, disk};
use crate::config::{self, Config};
use crate::console::Consoles;
use crate::self_service::{self, Build, Template};
use crate::state::{self, OpenLinkError};

/// What a node does for the front; the server is tested against a fake.
pub trait NodeOps: Send + Sync + 'static {
    fn host(&self) -> BoxFuture<'_, anyhow::Result<HostInfo>>;
    fn workspaces(&self) -> BoxFuture<'_, anyhow::Result<Vec<NodeWorkspace>>>;
    fn make(&self, req: MakeRequest) -> BoxFuture<'_, anyhow::Result<()>>;
    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, anyhow::Result<()>>;
    fn control<'a>(&'a self, name: &'a str, verb: Verb, wait: bool) -> BoxFuture<'a, anyhow::Result<()>>;
    fn open_link<'a>(&'a self, name: &'a str, app: &'a str) -> BoxFuture<'a, anyhow::Result<OpenLinkAnswer>>;
    /// The disk of a workspace that doesn't run, to send.
    fn disk<'a>(&'a self, name: &'a str) -> BoxFuture<'a, anyhow::Result<PathBuf>>;
    /// Where a disk received for a workspace goes, and whose it becomes.
    fn disk_target(&self, name: &str) -> anyhow::Result<(PathBuf, Account)>;
    /// A workspace's serial console socket.
    fn serial(&self, name: &str) -> anyhow::Result<PathBuf>;
    /// Forgets what is shared of a deleted workspace.
    fn deleted(&self, _name: &str) {}
}

struct Server {
    ops: Arc<dyn NodeOps>,
    /// The only address it answers, besides the Unix socket.
    front: Ipv4Addr,
    consoles: Arc<Consoles>,
}

/// The node's API.
pub fn router(ops: Arc<dyn NodeOps>, front: Ipv4Addr, consoles: Arc<Consoles>) -> Router {
    let server = Arc::new(Server { ops, front, consoles });
    Router::new()
        .route("/host", get(host))
        .route("/workspaces", get(workspaces))
        .route("/workspaces/{ws}", put(make).delete(delete))
        .route("/workspaces/{ws}/{action}", post(action))
        .route("/workspaces/{ws}/console", get(console))
        .route("/workspaces/{ws}/disk", get(disk_out).put(disk_in))
        .layer(middleware::from_fn_with_state(server.clone(), only_the_front))
        .layer(middleware::from_fn(http::versioned))
        .with_state(server)
}

/// Refuses everyone but the front: its `WireGuard` address, or this machine
/// through the socket.
async fn only_the_front(State(s): State<Arc<Server>>, req: Request, next: Next) -> Response {
    match http::peer(&req) {
        Some(Peer::Local) => next.run(req).await,
        Some(Peer::Ip(IpAddr::V4(ip))) if ip == s.front => next.run(req).await,
        Some(Peer::Ip(ip)) => error(StatusCode::FORBIDDEN, format!("{ip} isn't the front")),
        None => error(StatusCode::FORBIDDEN, "who is asking?"),
    }
}

fn checked(name: &str) -> Result<(), Box<Response>> {
    if is_workspace_name(name) {
        Ok(())
    } else {
        Err(Box::new(error(StatusCode::BAD_REQUEST, "invalid workspace name")))
    }
}

async fn host(State(s): State<Arc<Server>>) -> Response {
    match s.ops.host().await {
        Ok(h) => axum::Json(h).into_response(),
        Err(e) => failure(e),
    }
}

async fn workspaces(State(s): State<Arc<Server>>) -> Response {
    match s.ops.workspaces().await {
        Ok(all) => axum::Json(all).into_response(),
        Err(e) => failure(e),
    }
}

/// The largest request body, but a disk's.
const MAX_REQUEST: usize = 1 << 20;

async fn json_of<T: serde::de::DeserializeOwned>(body: Body) -> Result<T, Response> {
    let raw = axum::body::to_bytes(body, MAX_REQUEST)
        .await
        .map_err(|e| error(StatusCode::BAD_REQUEST, format!("the request: {e}")))?;
    serde_json::from_slice(&raw).map_err(|e| error(StatusCode::BAD_REQUEST, format!("the request: {e}")))
}

async fn make(State(s): State<Arc<Server>>, UrlPath(name): UrlPath<String>, body: Body) -> Response {
    let req: MakeRequest = match json_of(body).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    if req.record.name != name {
        return error(StatusCode::BAD_REQUEST, format!("the record names {:?}", req.record.name));
    }
    if let Err(e) = req.record.validate() {
        return error(StatusCode::BAD_REQUEST, e);
    }
    match s.ops.make(req).await {
        Ok(()) => axum::Json(serde_json::json!({})).into_response(),
        Err(e) => failure(e),
    }
}

async fn delete(State(s): State<Arc<Server>>, UrlPath(name): UrlPath<String>) -> Response {
    if let Err(r) = checked(&name) {
        return *r;
    }
    let r = s.ops.delete(&name).await;
    if r.is_ok() {
        s.consoles.forget(&name);
        s.ops.deleted(&name);
    }
    match r {
        Ok(()) => axum::Json(serde_json::json!({})).into_response(),
        Err(e) => failure(e),
    }
}

#[derive(serde::Deserialize)]
struct Wait {
    #[serde(default)]
    wait: Option<String>,
}

async fn action(
    State(s): State<Arc<Server>>,
    UrlPath((name, action)): UrlPath<(String, String)>,
    Query(q): Query<Wait>,
    body: Body,
) -> Response {
    if let Err(r) = checked(&name) {
        return *r;
    }
    if action == "open-link" {
        let req: OpenLinkRequest = match json_of(body).await {
            Ok(r) => r,
            Err(r) => return r,
        };
        return match s.ops.open_link(&name, &req.app).await {
            Ok(a) => axum::Json(a).into_response(),
            Err(e) => failure(e),
        };
    }
    let Ok(verb) = action.parse::<Verb>() else { return error(StatusCode::NOT_FOUND, "no such action") };
    let wait = q.wait.is_some_and(|w| w == "1" || w == "true");
    match s.ops.control(&name, verb, wait).await {
        Ok(()) => axum::Json(serde_json::json!({})).into_response(),
        Err(e) => failure(e),
    }
}

#[derive(serde::Deserialize)]
struct Viewer {
    #[serde(default)]
    viewer: String,
}

async fn console(
    State(s): State<Arc<Server>>,
    UrlPath(name): UrlPath<String>,
    Query(q): Query<Viewer>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Err(r) = checked(&name) {
        return *r;
    }
    let socket = match s.ops.serial(&name) {
        Ok(p) => p,
        Err(e) => return failure(e),
    };
    let shared = s.consoles.get(&name, &socket);
    let who = if workbench_model::is_user_name(&q.viewer) { q.viewer } else { "someone".to_owned() };
    upgrade.on_upgrade(move |ws| crate::console::watch(ws, shared, who))
}

async fn disk_out(State(s): State<Arc<Server>>, UrlPath(name): UrlPath<String>) -> Response {
    if let Err(r) = checked(&name) {
        return *r;
    }
    let path = match s.ops.disk(&name).await {
        Ok(p) => p,
        Err(e) => return failure(e),
    };
    let (tx, rx) = mpsc::channel::<Bytes>(8);
    tokio::task::spawn_blocking(move || {
        // A stream cut short ends without its hash, which the other side
        // refuses.
        if let Err(e) = disk::send(&path, &tx) {
            eprintln!("sending {}: {e:#}", path.display());
        }
    });
    let pieces =
        futures_util::stream::unfold(
            rx,
            |mut rx| async move { rx.recv().await.map(|b| (Ok::<_, std::io::Error>(b), rx)) },
        );
    Body::from_stream(pieces).into_response()
}

async fn disk_in(State(s): State<Arc<Server>>, UrlPath(name): UrlPath<String>, body: Body) -> Response {
    if let Err(r) = checked(&name) {
        return *r;
    }
    let (path, owner) = match s.ops.disk_target(&name) {
        Ok(t) => t,
        Err(e) => return failure(e),
    };
    let tmp = path.with_extension("raw.new");
    let _ = std::fs::remove_file(&tmp);
    let hash = match disk::receive(body.into_data_stream(), &tmp).await {
        Ok(h) => h,
        Err(e) => return error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    if let Err(e) = install(&tmp, &path, &owner, sync_dir) {
        let _ = std::fs::remove_file(&tmp);
        return error(StatusCode::INTERNAL_SERVER_ERROR, format!("{}: {e}", path.display()));
    }
    axum::Json(DiskAnswer { sha256: disk::hex(&hash) }).into_response()
}

/// Makes a received disk the workspace's, its VM user's and no one else's,
/// for good: renamed into place, then the directory synced, so a node that
/// loses power after it answered still has it when the source's copy goes.
pub(crate) fn install(
    tmp: &Path,
    path: &Path,
    owner: &Account,
    sync: impl Fn(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::os::unix::fs::chown(tmp, Some(owner.uid), Some(owner.gid))?;
    std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(tmp, path)?;
    sync(path.parent().unwrap_or(Path::new("/")))
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// The node as it is: its configs, systemd, its VMs and its files.
pub struct HostNode {
    pub settings: NodeSettings,
}

impl HostNode {
    fn dirs(&self) -> Vec<PathBuf> {
        self.settings.config_dirs()
    }

    fn template(&self) -> anyhow::Result<Template> {
        Template::load(Path::new(&self.settings.template))
    }

    fn load(&self, name: &str) -> anyhow::Result<Config> {
        config::load(&self.dirs(), name).map_err(|e| Status(StatusCode::NOT_FOUND, format!("{e:#}")).into())
    }

    async fn not_running(name: &str) -> anyhow::Result<()> {
        let unit = state::unit_name(name);
        // An error counts as running: nothing is read or deleted under a VM.
        if self_service::unit_running(&unit).await {
            bail!(Status(StatusCode::CONFLICT, format!("{unit} is running or about to start")));
        }
        Ok(())
    }

    async fn make_it(&self, req: MakeRequest) -> anyhow::Result<()> {
        let r = req.record;
        ensure!(
            !workbench_model::self_service::is_reserved(&r.name),
            Status(StatusCode::CONFLICT, format!("{} is a name the node keeps for itself", r.name))
        );
        let t = self.template()?;
        let paths = t.paths();
        let _lock = paths.lock()?;
        self_service::finish_pending(&paths).await?;
        let root = self.settings.state_root();
        if r.declared {
            // Nix made the rest of it here: only a move asks for it.
            let c = self.load(&r.name)?;
            ensure!(!c.self_service, Status(StatusCode::CONFLICT, format!("{} was made at run time here", r.name)));
            ensure!(
                c.ip == r.ip,
                Status(StatusCode::CONFLICT, format!("Nix gives {} {} here, not {}", r.name, c.ip, r.ip))
            );
            ensure!(!c.disk().exists(), Status(StatusCode::CONFLICT, format!("{} has a disk here already", r.name)));
            state::mark_stopped(&root, &r.name)?;
            let vm_user = hostutil::lookup_account(&c.user).context("VM user")?;
            return crate::launch::prepare_state_dir(&c, &vm_user);
        }
        let dirs = self.dirs();
        let (all, unreadable) = self_service::every_config(&dirs);
        ensure!(unreadable.is_empty(), "configs the node can't read: {}", unreadable.join("; "));
        let leftover = [Path::new(&t.state_dir).join(&r.name), paths.seed(&r.name)];
        if all.iter().any(|c| c.name == r.name) || leftover.iter().any(|p| p.exists()) {
            bail!(Status(StatusCode::CONFLICT, format!("{} is taken on this node", r.name)));
        }
        // The run-time half of this node's subnet: never its own address,
        // nor a declared workspace's.
        let ip: Ipv4Addr = r.ip.parse().context("address")?;
        let in_half = self_service::MADE_IPS.contains(&ip.octets()[3]);
        ensure!(
            in_half && r.ip.rsplit_once('.').is_some_and(|(net, _)| net == t.subnet),
            Status(StatusCode::CONFLICT, format!("{} isn't in the run-time half of {}.0/24", r.ip, t.subnet))
        );
        if all.iter().any(|c| c.ip == r.ip) {
            bail!(Status(StatusCode::CONFLICT, format!("{} is another workspace's address here", r.ip)));
        }
        let uids = all.iter().map(|c| c.uid).collect();
        let uid = self_service::free_uid(&uids, |u| {
            nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(u)).ok().flatten().is_some()
        })?;
        let c = self_service::render(&t, &r, uid)?;
        let how = Build { disk: !req.for_move, stopped: req.for_move, secret_environment: None };
        self_service::build(&t, &paths, &c, how).await
    }

    async fn delete_it(&self, name: &str) -> anyhow::Result<()> {
        let t = self.template()?;
        let paths = t.paths();
        let found = config::load(&self.dirs(), name);
        if found.as_ref().is_ok_and(|c| c.self_service) {
            // It takes the lock itself, as on a host on its own: a lock
            // taken twice in one process waits for good.
            return self_service::take_away(&paths, name).await;
        }
        let _lock = paths.lock()?;
        self_service::finish_pending(&paths).await?;
        let root = self.settings.state_root();
        if let Ok(c) = found {
            // Declared here: Nix keeps the rest; the disk goes.
            stop(name).await?;
            Self::not_running(name).await?;
            return remove(Path::new(&c.state_dir));
        }
        // Only its disk is left, as Nix removed it.
        let dir = root.join(name);
        if !dir.exists() {
            return Ok(());
        }
        stop(name).await?;
        Self::not_running(name).await?;
        remove(&dir)?;
        remove(&state::stop_marker(&root, name))
    }

    async fn collect(&self) -> anyhow::Result<Vec<NodeWorkspace>> {
        let dirs = self.dirs();
        let root = self.settings.state_root();
        let all = state::collect(&dirs, &root).await?;
        Ok(all
            .into_iter()
            .map(|ws| {
                let c = config::load(&dirs, &ws.name).ok();
                let running = matches!(ws.unit.as_str(), "active" | "deactivating" | "activating" | "reloading");
                NodeWorkspace {
                    booted: c.as_ref().filter(|_| running).and_then(state::booted),
                    next: c.as_ref().map(Config::boot).unwrap_or_default(),
                    stopped: state::stop_marker(&root, &ws.name).exists(),
                    workspace: ws,
                }
            })
            .collect())
    }

    fn host_info(&self) -> anyhow::Result<HostInfo> {
        let info = hostutil::meminfo_kib()?;
        let field = |k: &str| info.get(k).copied().unwrap_or(0) << 10;
        let total = field("MemTotal");
        let reserve = vmkit::memory::reserve(total, self.settings.reserve_percent, self.settings.reserve_min_gib);
        let fs = nix::sys::statvfs::statvfs(Path::new(&self.settings.state_dir))?;
        #[allow(clippy::useless_conversion)]
        let (frsize, blocks, avail) =
            (u64::from(fs.fragment_size()), u64::from(fs.blocks()), u64::from(fs.blocks_available()));
        let dirs = self.dirs();
        let max_memory = config::configured(&dirs)
            .iter()
            .filter_map(|n| config::load(&dirs, n).ok())
            .map(|c| c.max_mem_mib() * MIB)
            .sum();
        let pools = self
            .settings
            .pools
            .iter()
            .map(|p| PoolInfo {
                name: p.name.clone(),
                status: hostutil::read_bounded(&p.status, vmkit::memory::STATUS_MAX_BYTES)
                    .ok()
                    .and_then(|raw| serde_json::from_slice::<PoolStatus>(&raw).ok()),
            })
            .collect();
        Ok(HostInfo {
            version: VERSION.to_owned(),
            mem_total: total,
            mem_available: field("MemAvailable"),
            reserve,
            pools,
            disk_total: blocks * frsize,
            disk_free: avail * frsize,
            max_memory,
        })
    }
}

async fn stop(name: &str) -> anyhow::Result<()> {
    self_service::run_tool("systemctl", &["stop", &state::unit_name(name)]).await
}

fn remove(p: &Path) -> anyhow::Result<()> {
    self_service::remove(p).with_context(|| p.display().to_string())
}

impl NodeOps for HostNode {
    fn host(&self) -> BoxFuture<'_, anyhow::Result<HostInfo>> {
        Box::pin(async move { self.host_info() })
    }

    fn workspaces(&self) -> BoxFuture<'_, anyhow::Result<Vec<NodeWorkspace>>> {
        Box::pin(self.collect())
    }

    fn make(&self, req: MakeRequest) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(self.make_it(req))
    }

    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(self.delete_it(name))
    }

    fn control<'a>(&'a self, name: &'a str, verb: Verb, wait: bool) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            // A workspace removed from the configuration may still be
            // stopped.
            if verb != Verb::Stop {
                self.load(name)?;
            }
            state::control(&self.settings.state_root(), verb, name, wait).await
        })
    }

    fn open_link<'a>(&'a self, name: &'a str, app: &'a str) -> BoxFuture<'a, anyhow::Result<OpenLinkAnswer>> {
        Box::pin(async move {
            let c = self.load(name)?;
            Ok(match state::open_link(&c, app).await {
                Ok(link) => OpenLinkAnswer::Link { link },
                Err(OpenLinkError::Starting(detail)) => OpenLinkAnswer::Starting { detail },
                Err(OpenLinkError::Failed { exit_code, detail }) => OpenLinkAnswer::Failed { exit_code, detail },
                Err(OpenLinkError::NotALink) => OpenLinkAnswer::NotALink,
                Err(OpenLinkError::Timeout) => OpenLinkAnswer::Timeout,
            })
        })
    }

    fn disk<'a>(&'a self, name: &'a str) -> BoxFuture<'a, anyhow::Result<PathBuf>> {
        Box::pin(async move {
            let path = match config::load(&self.dirs(), name) {
                Ok(c) => c.disk(),
                Err(_) => self.settings.state_root().join(name).join("disk.raw"),
            };
            ensure!(path.is_file(), Status(StatusCode::NOT_FOUND, format!("{name} has no disk here")));
            Self::not_running(name).await?;
            Ok(path)
        })
    }

    fn disk_target(&self, name: &str) -> anyhow::Result<(PathBuf, Account)> {
        let c = self.load(name)?;
        let path = c.disk();
        ensure!(!path.exists(), Status(StatusCode::CONFLICT, format!("{name} has a disk here already")));
        ensure!(
            Path::new(&c.state_dir).is_dir(),
            Status(StatusCode::CONFLICT, format!("{name} wasn't made for a move"))
        );
        let owner = hostutil::lookup_account(&c.user).context("VM user")?;
        Ok((path, owner))
    }

    fn serial(&self, name: &str) -> anyhow::Result<PathBuf> {
        Ok(self.load(name)?.serial())
    }
}

/// `workbench node serve`: the API on the node's `WireGuard` address and,
/// on a machine with both roles, its socket; and the shared consoles for
/// `workbench console` on the node.
pub async fn serve(settings: NodeSettings) -> anyhow::Result<()> {
    let consoles = Arc::new(Consoles::default());
    let front = settings.front_address;
    let (listen, socket, group) = (settings.listen, settings.socket.clone(), settings.socket_group.clone());
    let dirs = settings.config_dirs();
    let ops: Arc<dyn NodeOps> = Arc::new(HostNode { settings });
    let app = router(ops, front, consoles.clone());
    let tcp = async {
        match listen {
            Some(addr) => http::serve_tcp(addr, app.clone()).await,
            None => std::future::pending().await,
        }
    };
    let unix = async {
        if socket.is_empty() {
            std::future::pending().await
        } else {
            http::serve_unix(Path::new(&socket), &group, app.clone()).await
        }
    };
    let local = consoles.serve_local(Path::new(super::NODE_CONSOLES), "root on the node", move |name| {
        Ok((config::load(&dirs, name)?.serial(), None))
    });
    tokio::select! {
        r = tcp => r.context("the API"),
        r = unix => r.context("the API's socket"),
        r = local => r.context("the shared consoles"),
    }
}

#[cfg(test)]
mod tests;
