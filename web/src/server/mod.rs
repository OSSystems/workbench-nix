//! The web UI's server: it renders the pages and answers their server
//! functions, streams live updates, relays the consoles and answers nginx's
//! access check for every workspace. It runs on the front as its own user,
//! on [`Fleet`]: the registry, and the nodes, which it asks for every
//! workspace's state and what is done to one. polkit lets it start the
//! front's units that make, change, move and destroy workspaces, and
//! nothing else.

pub mod authz;
pub mod connect;
pub mod console;
mod routes;
mod self_service;
mod view;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use leptos::config::LeptosOptions;
use leptos::prelude::*;
use workbench::config::Config;
pub use workbench::fleet::front::Fleet;
use workbench::self_service::Paths;
use workbench::state::OpenLinkError;
use workbench_model::apps::{BootApp, DeviceLink};
use workbench_model::fleet::{NodeRefusal, NodeView};
use workbench_model::self_service::Limits;
use workbench_model::{GIB, HostView, OpenBlocker, Snapshot, State, Verb, Viewer, Workspace, is_user_name};

pub use connect::connect_router;
pub use routes::{authz_router, ui_router};
pub use self_service::CreateForm;
pub use workbench::self_service::Paths as SelfServicePaths;

/// A future a [`Backend`] returns.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the UI does on the front; tests replace it.
pub trait Backend: Send + Sync + 'static {
    /// Every workspace's status.
    fn list(&self) -> BoxFuture<'_, anyhow::Result<Vec<Workspace>>>;
    /// Starts, stops or restarts a workspace, without waiting for it.
    fn control<'a>(&'a self, verb: Verb, name: &'a str) -> BoxFuture<'a, anyhow::Result<()>>;
    /// A workspace's config.
    fn config(&self, name: &str) -> anyhow::Result<Config>;
    /// The apps a browser may reach now: the config's, and those the VM
    /// that runs still has.
    fn reachable(&self, c: &Config) -> Vec<BootApp>;
    /// A link that signs a browser in to an app.
    fn open_link(&self, c: Config, app: String) -> BoxFuture<'_, Result<String, OpenLinkError>>;
    /// Who a browser's cookies are signed in as, and their groups; no one
    /// has an empty name.
    fn whoami<'a>(&'a self, cookie: &'a str) -> BoxFuture<'a, anyhow::Result<Identity>>;
    /// Starts a unit and waits for it to finish: the units that make and
    /// destroy workspaces.
    fn run_unit<'a>(&'a self, unit: &'a str) -> BoxFuture<'a, anyhow::Result<()>>;
    /// The host CA's public key, which signs the workspaces' host keys.
    fn host_ca(&self) -> anyhow::Result<String>;
    /// An SSH public key signed by the user CA for `user`, for a few hours.
    fn sign_user_key<'a>(&'a self, user: &'a str, id: &'a str, key: &'a str) -> BoxFuture<'a, anyhow::Result<String>>;
    /// Where a workspace's SSH server listens: its address, port 22.
    fn ssh_address(&self, c: &Config) -> String;
    /// The fleet: the nodes, closing them, moves and the consoles go
    /// through it.
    fn fleet(&self) -> &Fleet;
}

/// The groups single sign-on gave the person asking, as the UI's
/// requests carry them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Groups(pub Vec<String>);

/// Who single sign-on says someone is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    pub user: String,
    pub groups: Vec<String>,
}

/// What lets people make their own workspaces.
#[derive(Debug, Clone)]
pub struct SelfService {
    pub limits: Limits,
    /// Where requests, outcomes and quotas go.
    pub paths: Paths,
}

/// How `workbench-web` is set up.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The UI's own origin, `https://<domain>`; every `POST` must come from
    /// it.
    pub origin: String,
    pub admins: Vec<String>,
    /// Groups whose members are admins.
    pub admin_groups: Vec<String>,
    /// Groups whose members make workspaces of their own.
    pub creator_groups: Vec<String>,
    /// Set when the front lets people make workspaces.
    pub self_service: Option<SelfService>,
    /// Where the access check asks who a cookie belongs to.
    pub oauth2_proxy: String,
    /// The single sign-on cookie, which no workspace's app ever gets.
    pub cookie_name: String,
    /// Where the paired devices are kept.
    pub devices: PathBuf,
    /// `ssh-keygen`, the user CA's private key, which it signs people's
    /// keys with, and the host CA's public key.
    pub ssh_keygen: String,
    pub user_ca: PathBuf,
    pub host_ca: PathBuf,
}

impl Options {
    /// The domain workspaces are subdomains of.
    #[must_use]
    pub fn domain(&self) -> &str {
        self.origin.trim_start_matches("https://").trim_start_matches("http://")
    }
}

/// The front as the UI sees it: every workspace from the registry and its
/// node, and what is done to one done through its node. Who owns a
/// workspace and where it is are the registry's, never a node's: the
/// access check and the tunnel read it, and answer while a node doesn't.
/// Who a cookie is, the units and the user CA are the front's own.
pub struct FleetBackend {
    pub opts: Options,
    pub fleet: Fleet,
    /// The apps each workspace's VM ran at the last list, by id: one that
    /// goes with its next restart still opens until then.
    running: Mutex<HashMap<String, Vec<String>>>,
}

impl FleetBackend {
    #[must_use]
    pub fn new(opts: Options, fleet: Fleet) -> Self {
        FleetBackend { opts, fleet, running: Mutex::default() }
    }
}

impl Backend for FleetBackend {
    fn list(&self) -> BoxFuture<'_, anyhow::Result<Vec<Workspace>>> {
        Box::pin(async move {
            let all = self.fleet.list().await?;
            let running = all
                .iter()
                .filter(|w| w.vm != "-")
                .map(|w| (w.name.clone(), w.running_apps().map(|a| a.id.clone()).collect()))
                .collect();
            *lock(&self.running) = running;
            Ok(all)
        })
    }

    fn control<'a>(&'a self, verb: Verb, name: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(self.fleet.control(name, verb))
    }

    fn config(&self, name: &str) -> anyhow::Result<Config> {
        self.fleet.config(name)
    }

    /// The registry's apps, then those the VM ran at the last list that
    /// the registry no longer has, as the fleet offers them.
    fn reachable(&self, c: &Config) -> Vec<BootApp> {
        let ran = lock(&self.running).get(&c.name).cloned().unwrap_or_default();
        let gone: Vec<String> = ran.into_iter().filter(|id| !c.apps.iter().any(|a| a.id == *id)).collect();
        c.apps.iter().cloned().chain(self.fleet.settings.apps_of(&gone)).collect()
    }

    fn open_link(&self, c: Config, app: String) -> BoxFuture<'_, Result<String, OpenLinkError>> {
        Box::pin(async move { self.fleet.open_link(&c.name, &app).await })
    }

    fn whoami<'a>(&'a self, cookie: &'a str) -> BoxFuture<'a, anyhow::Result<Identity>> {
        Box::pin(authz::ask_oauth2_proxy(&self.opts.oauth2_proxy, cookie))
    }

    fn run_unit<'a>(&'a self, unit: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let run = tokio::process::Command::new("systemctl")
                .args(["--no-ask-password", "start", unit])
                .kill_on_drop(true)
                .output();
            let out = tokio::time::timeout(UNIT_TIMEOUT, run)
                .await
                .map_err(|_| anyhow::anyhow!("{unit} took too long"))??;
            anyhow::ensure!(out.status.success(), "{unit}: {}", String::from_utf8_lossy(&out.stderr).trim());
            Ok(())
        })
    }

    fn host_ca(&self) -> anyhow::Result<String> {
        let raw = vmkit::hostutil::read_bounded(&self.opts.host_ca, 64 << 10)
            .with_context(|| self.opts.host_ca.display().to_string())?;
        Ok(String::from_utf8_lossy(&raw).trim().to_owned())
    }

    fn sign_user_key<'a>(&'a self, user: &'a str, id: &'a str, key: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(connect::sign_with(&self.opts.ssh_keygen, &self.opts.user_ca, user, id, key))
    }

    /// The registry's address, which the front reaches through `WireGuard`
    /// or, for its own node's, the tap.
    fn ssh_address(&self, c: &Config) -> String {
        format!("{}:22", c.ip)
    }

    fn fleet(&self) -> &Fleet {
        &self.fleet
    }
}

/// How long the UI waits for making or destroying a workspace: a new disk
/// is a copy of the image. Longer than the units' own 10 minutes, so their
/// outcome is there to read.
const UNIT_TIMEOUT: Duration = Duration::from_secs(660);

/// How long a collected state is reused, whoever asks.
const SNAPSHOT_FOR: Duration = Duration::from_secs(1);
/// How old a state may be and still be shown at once while a new one is
/// collected: older, no one has looked for a while, the live updates
/// included, and a new one is waited for as an action's answer is.
const SHOWN_FOR: Duration = Duration::from_secs(5);
/// How long an answer after an action waits for the state it left, before
/// it makes do with the last: a node that doesn't answer holds the
/// collection up to its timeouts.
const COLLECT_WAIT: Duration = Duration::from_secs(3);
/// How long a restart someone asked for shows as restarting at most.
const RESTART_SHOWN: Duration = Duration::from_secs(180);
/// How long after asking a workspace may still look as it did before.
const RESTART_GRACE: Duration = Duration::from_secs(15);

/// Everything the handlers share.
#[derive(Clone)]
pub struct AppState(Arc<Inner>);

pub struct Inner {
    pub opts: Options,
    pub backend: Box<dyn Backend>,
    /// The computers people paired, and the codes waiting.
    pub devices: connect::Devices,
    pub leptos: LeptosOptions,
    cache: tokio::sync::Mutex<Cache>,
    /// Held while the state is collected: one collection at a time, which
    /// no one but who waits for it waits for.
    collecting: Arc<tokio::sync::Mutex<()>>,
    pub(crate) authz: Mutex<HashMap<String, (Identity, Instant)>>,
    /// The names being made now, so two requests for one never overwrite
    /// each other's.
    pub(crate) making: Mutex<std::collections::HashSet<String>>,
}

struct Cache {
    /// When the state was collected; none once an action changed it.
    at: Option<Instant>,
    /// One more at each action: a collection begun before it is no answer
    /// to it.
    era: u64,
    /// Whether the state was ever collected.
    collected: bool,
    all: Result<Vec<Workspace>, String>,
    /// When someone asked for a restart, by workspace.
    restarts: HashMap<String, Instant>,
    /// Every node, as it said of itself with the list.
    nodes: Vec<NodeView>,
    /// What each workspace's node last said of it, which it keeps while its
    /// node doesn't answer.
    known: HashMap<String, Workspace>,
}

impl Default for Cache {
    fn default() -> Self {
        Cache {
            at: None,
            era: 0,
            collected: false,
            all: Ok(vec![]),
            restarts: HashMap::new(),
            nodes: vec![],
            known: HashMap::new(),
        }
    }
}

/// A workspace its node said nothing of just now, with what it last said:
/// its system, its sizes and its apps.
fn keep_known(ws: &mut Workspace, known: &Workspace) {
    if ws.os.is_empty() {
        ws.os.clone_from(&known.os);
    }
    if ws.apps.is_empty() {
        ws.apps.clone_from(&known.apps);
    }
    ws.memory_bytes = known.memory_bytes;
    ws.boot_memory_bytes = known.boot_memory_bytes;
    if ws.max_memory_bytes == 0 {
        ws.max_memory_bytes = known.max_memory_bytes;
        ws.memory_auto = known.memory_auto;
    }
    ws.disk_bytes = known.disk_bytes;
    ws.disk_used_bytes = known.disk_used_bytes;
}

impl std::ops::Deref for AppState {
    type Target = Inner;

    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl axum::extract::FromRef<AppState> for LeptosOptions {
    fn from_ref(state: &AppState) -> Self {
        state.leptos.clone()
    }
}

/// What a text says to `viewer`: an admin reads it as it is; anyone else
/// reads the node's refusal in it as `NodeRefusal::told` words it for
/// them, with no node's name.
pub(crate) fn told(viewer: &Viewer, text: String, node: Option<&NodeRefusal>) -> String {
    match node {
        Some(n) if !viewer.admin => text.replace(&n.told(true), &n.told(false)),
        _ => text,
    }
}

/// An error, as `viewer` is told it.
pub(crate) fn told_error(viewer: &Viewer, e: &anyhow::Error) -> String {
    told(viewer, format!("{e:#}"), e.chain().find_map(|c| c.downcast_ref::<NodeRefusal>()))
}

/// How a unit's request went, as `viewer` is told it.
pub(crate) fn told_outcome(viewer: &Viewer, o: &workbench_model::self_service::Outcome) -> String {
    told(viewer, o.error.clone(), o.node.as_ref())
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Why a server function refuses.
fn refuse(msg: &str, code: axum::http::StatusCode) -> ServerFnError {
    status(code);
    ServerFnError::new(msg)
}

/// The groups of who is asking, in a server function or a page.
#[must_use]
pub fn request_groups() -> Vec<String> {
    use_context::<axum::http::request::Parts>()
        .and_then(|p| p.extensions.get::<Groups>().cloned())
        .map(|g| g.0)
        .unwrap_or_default()
}

/// The app state and who is asking, in a server function or a page.
pub fn request() -> Result<(AppState, Viewer), ServerFnError> {
    let state = use_context::<AppState>().ok_or_else(|| ServerFnError::new("no app state"))?;
    let parts = use_context::<axum::http::request::Parts>().ok_or_else(|| ServerFnError::new("no request"))?;
    let viewer = parts.extensions.get::<Viewer>().cloned().ok_or_else(|| ServerFnError::new("not signed in"))?;
    Ok((state, viewer))
}

/// The host could not be read: a `502`.
#[allow(clippy::needless_pass_by_value)]
pub fn failed(e: anyhow::Error) -> ServerFnError {
    refuse(&format!("{e:#}"), axum::http::StatusCode::BAD_GATEWAY)
}

/// Sets the status of the response being made.
pub fn status(code: axum::http::StatusCode) {
    if let Some(res) = use_context::<leptos_axum::ResponseOptions>() {
        res.set_status(code);
    }
}

/// Sends the browser on: a form gets `303 See Other`, the page's own
/// script follows it itself.
fn see_other(to: &str) {
    leptos_axum::redirect(to);
    let form = use_context::<axum::http::request::Parts>().is_some_and(|p| {
        p.headers.get(axum::http::header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("text/html"))
    });
    if form {
        status(axum::http::StatusCode::SEE_OTHER);
    }
}

impl AppState {
    pub fn new(opts: Options, backend: Box<dyn Backend>, leptos: LeptosOptions) -> anyhow::Result<Self> {
        let devices = connect::Devices::load(&opts.devices)?;
        Ok(AppState(Arc::new(Inner {
            opts,
            backend,
            devices,
            leptos,
            cache: tokio::sync::Mutex::default(),
            collecting: Arc::default(),
            authz: Mutex::default(),
            making: Mutex::default(),
        })))
    }

    /// Who `name`, a member of `groups`, is here.
    #[must_use]
    pub fn viewer(&self, name: &str, groups: &[String]) -> Option<Viewer> {
        let o = &self.opts;
        let in_any = |of: &[String]| groups.iter().any(|g| of.contains(g));
        let admin = o.admins.iter().any(|a| a == name) || in_any(&o.admin_groups);
        let creates = o.self_service.is_some() && (admin || in_any(&o.creator_groups));
        is_user_name(name).then(|| Viewer { name: name.to_owned(), admin, creates })
    }

    /// Every workspace, collected at most once a second whoever asks, with
    /// every node. The collection runs apart: while it does, everyone gets
    /// the last state, so a node slow to answer slows no page. Only the
    /// first ever, the answer to an action and a state older than
    /// [`SHOWN_FOR`], which no one looked at since, wait for a new one, all
    /// but the first no longer than [`COLLECT_WAIT`].
    pub async fn all(&self) -> anyhow::Result<Vec<Workspace>> {
        loop {
            let (wait, collected) = {
                let cache = self.cache.lock().await;
                match cache.at {
                    Some(at) if at.elapsed() < SNAPSHOT_FOR => return cache.all.clone().map_err(anyhow::Error::msg),
                    at => (at.is_none_or(|at| at.elapsed() >= SHOWN_FOR), cache.collected),
                }
            };
            if let Ok(one) = Arc::clone(&self.collecting).try_lock_owned() {
                let state = self.clone();
                tokio::spawn(async move {
                    state.collect().await;
                    drop(one);
                });
            }
            if !wait {
                return self.last().await;
            }
            match tokio::time::timeout(COLLECT_WAIT, self.collecting.lock()).await {
                Ok(done) => drop(done),
                Err(_) if collected => return self.last().await,
                Err(_) => drop(self.collecting.lock().await),
            }
        }
    }

    /// Whether the state was ever collected.
    pub(crate) async fn collected(&self) -> bool {
        self.cache.lock().await.collected
    }

    /// Collects the state again unless it is under a second old, and waits
    /// for it at most [`COLLECT_WAIT`]; everyone else keeps the state they
    /// had meanwhile. Whether the state is under a second old now.
    pub(crate) async fn refresh(&self) -> bool {
        let fresh = |c: &Cache| c.at.is_some_and(|at| at.elapsed() < SNAPSHOT_FOR);
        let collected = async {
            loop {
                if fresh(&*self.cache.lock().await) {
                    return;
                }
                let one = Arc::clone(&self.collecting).lock_owned().await;
                if fresh(&*self.cache.lock().await) {
                    return;
                }
                self.collect().await;
                drop(one);
            }
        };
        tokio::time::timeout(COLLECT_WAIT, collected).await.is_ok()
    }

    /// The state last collected.
    async fn last(&self) -> anyhow::Result<Vec<Workspace>> {
        self.cache.lock().await.all.clone().map_err(anyhow::Error::msg)
    }

    /// Collects the state, outside the cache's lock, then keeps it.
    async fn collect(&self) {
        let era = self.cache.lock().await.era;
        let (all, nodes) = tokio::join!(self.backend.list(), self.backend.fleet().nodes_view());
        let mut cache = self.cache.lock().await;
        cache.all = all.map_err(|e| format!("{e:#}"));
        cache.nodes = nodes;
        cache.collected = true;
        // One begun before an action says nothing of it.
        cache.at = (cache.era == era).then(Instant::now);
        {
            let Cache { all, restarts, known, .. } = &mut *cache;
            if let Ok(all) = &mut *all {
                for ws in all.iter_mut() {
                    // Its node has it now, or said nothing of it.
                    if ws.unit != "-" {
                        known.insert(ws.name.clone(), ws.clone());
                    } else if let (State::Unreachable | State::Moving, Some(k)) = (ws.state, known.get(&ws.name)) {
                        keep_known(ws, k);
                    }
                }
                known.retain(|name, _| all.iter().any(|w| w.name == *name));
            }
            if let Ok(all) = all {
                for ws in all.iter_mut() {
                    let Some(at) = restarts.get(&ws.name) else { continue };
                    let age = at.elapsed();
                    // Right after the request the unit may not have begun
                    // stopping yet.
                    // A restart ends running, failing or, if something
                    // stopped it, stopped.
                    if age > RESTART_SHOWN
                        || (age > RESTART_GRACE && matches!(ws.state, State::Running | State::Failing | State::Stopped))
                    {
                        restarts.remove(&ws.name);
                    } else if matches!(ws.state, State::Stopping | State::Starting | State::Stopped) {
                        ws.state = State::Restarting;
                    }
                }
            }
        }
    }

    /// Every node, as it said of itself with the list.
    pub async fn nodes(&self) -> Vec<NodeView> {
        let _ = self.all().await;
        self.cache.lock().await.nodes.clone()
    }

    /// What people may make, with `auto` the largest node's memory.
    pub async fn limits(&self) -> Option<Limits> {
        Some(self.limits_of(self.opts.self_service.as_ref()?).await)
    }

    /// What `ss` lets people make, with `auto` the largest node's memory.
    async fn limits_of(&self, ss: &SelfService) -> Limits {
        if ss.limits.memory_auto() { ss.limits.resolved(&self.nodes().await) } else { ss.limits.clone() }
    }

    /// Shows a workspace as restarting until it is up again, after asking
    /// for its restart.
    async fn restarted(&self, name: &str) {
        self.cache.lock().await.restarts.insert(name.to_owned(), Instant::now());
        self.forget().await;
    }

    /// Forgets the collected state, after an action changed it.
    async fn forget(&self) {
        let mut cache = self.cache.lock().await;
        cache.at = None;
        cache.era += 1;
    }

    /// What `viewer` sees, and the apps a workspace made at run time may
    /// have.
    pub async fn snapshot_for(&self, viewer: &Viewer) -> anyhow::Result<Snapshot> {
        let all = self.all().await?;
        let mut snap = Snapshot::of(viewer.clone(), all.into_iter().map(view::row));
        if let Some(limits) = self.limits().await {
            snap.apps.clone_from(&limits.apps);
            snap.memory_gib = Some((limits.min_memory_gib(), limits.max_memory_gib()));
            snap.memory_auto = limits.memory_auto();
            if viewer.admin {
                snap.node_memory_gib =
                    self.nodes().await.into_iter().filter_map(|n| Some((n.name, n.host?.mem_total / GIB))).collect();
            }
        }
        Ok(snap)
    }

    /// The host page: a section per node.
    pub async fn host_view(&self) -> anyhow::Result<HostView> {
        let all = self.all().await?;
        let root = self.backend.fleet().state_root();
        let mut host = view::host(&self.nodes().await, &all, |node| workbench::fleet::is_closed(&root, node));
        host.quotas = self.quotas(&all)?;
        Ok(host)
    }

    /// A workspace `viewer` may control, or a 403 alike for one that is
    /// not theirs and one that does not exist.
    async fn find(&self, viewer: &Viewer, name: &str) -> Result<Workspace, ServerFnError> {
        let all = self.all().await.map_err(|e| refuse(&format!("{e:#}"), axum::http::StatusCode::BAD_GATEWAY))?;
        all.into_iter()
            .find(|ws| ws.name == name && ws.configured && viewer.may(ws))
            .ok_or_else(|| refuse("this workspace isn't yours", axum::http::StatusCode::FORBIDDEN))
    }

    /// The `control` server function.
    pub async fn control_action(
        &self,
        viewer: &Viewer,
        name: &str,
        action: &str,
        stay: bool,
    ) -> Result<(), ServerFnError> {
        let verb: Verb = action.parse().map_err(|e: String| refuse(&e, axum::http::StatusCode::BAD_REQUEST))?;
        let ws = self.find(viewer, name).await?;
        let back = format!("/w/{name}/{verb}");
        if ws.state.changing() {
            status(axum::http::StatusCode::CONFLICT);
            if !stay {
                see_other(&format!("{back}?error=busy"));
            }
            return Err(ServerFnError::new(format!("{name}{}", ws.state.busy_suffix())));
        }
        if let Err(e) = self.backend.control(verb, name).await {
            let detail = told_error(viewer, &e);
            status(axum::http::StatusCode::BAD_GATEWAY);
            if !stay {
                see_other(&format!("{back}?error=failed&detail={}", encode(&detail)));
            }
            return Err(ServerFnError::new(detail));
        }
        if verb == Verb::Restart {
            self.restarted(name).await;
        } else {
            self.cache.lock().await.restarts.remove(name);
            self.forget().await;
        }
        if !stay {
            see_other(&format!("/?w={name}"));
        }
        Ok(())
    }

    /// An app's link, for a workspace `viewer` may use: one that opens now
    /// and has a sign-in hook. Otherwise, why not, as the open page names
    /// it, and what to show of it.
    async fn link(
        &self,
        viewer: &Viewer,
        name: &str,
        app: &str,
    ) -> Result<Result<String, (&'static str, String)>, ServerFnError> {
        let ws = self.find(viewer, name).await?;
        let why = match ws.open_blocker(app) {
            Some(OpenBlocker::Unknown) => "unknown",
            Some(OpenBlocker::Stopped) => "stopped",
            Some(OpenBlocker::Starting) => "starting",
            Some(OpenBlocker::Coming) => "coming",
            Some(OpenBlocker::NoAddress) => "no-address",
            None if !ws.app(app).is_some_and(|a| a.open_link) => "no-hook",
            None => "",
        };
        if !why.is_empty() {
            return Ok(Err((why, String::new())));
        }
        let c =
            self.backend.config(name).map_err(|e| refuse(&format!("{e:#}"), axum::http::StatusCode::BAD_GATEWAY))?;
        Ok(self.backend.open_link(c, app.to_owned()).await.map_err(|e| {
            let (kind, detail) = match e {
                OpenLinkError::Starting(_) => ("starting", String::new()),
                OpenLinkError::Timeout => ("timeout", String::new()),
                OpenLinkError::NotALink => ("not-a-link", String::new()),
                OpenLinkError::Failed { exit_code: 0, detail } => ("failed", detail),
                OpenLinkError::Failed { exit_code, detail } => ("failed", format!("exit status {exit_code}\n{detail}")),
            };
            (kind, detail.chars().take(2000).collect())
        }))
    }

    /// The `open` server function: a `303` to the app's one-time link,
    /// which is never shown or logged, or to its open page with why not.
    pub async fn open_action(&self, viewer: &Viewer, name: &str, app: &str) -> Result<(), ServerFnError> {
        let page = format!("/w/{name}/open/{}", encode(app));
        match self.link(viewer, name, app).await? {
            Ok(link) => {
                no_store_redirect();
                see_other(&link);
            }
            // The page says these itself, from the workspace's state.
            Err(("stopped" | "starting" | "coming" | "no-hook" | "no-address" | "unknown", _)) => see_other(&page),
            Err((kind, detail)) => see_other(&format!("{page}?error={kind}&detail={}", encode(&detail))),
        }
        Ok(())
    }

    /// The `device_link` server function: the link itself and its QR code,
    /// for another device to open the app with.
    pub async fn device_link_action(
        &self,
        viewer: &Viewer,
        name: &str,
        app: &str,
    ) -> Result<DeviceLink, ServerFnError> {
        match self.link(viewer, name, app).await? {
            Ok(link) => {
                no_store();
                let (size, qr) =
                    qr_path(&link).map_err(|e| refuse(&e, axum::http::StatusCode::INTERNAL_SERVER_ERROR))?;
                Ok(DeviceLink { link, size, qr })
            }
            Err((kind, detail)) => {
                status(axum::http::StatusCode::CONFLICT);
                Err(ServerFnError::new(if detail.is_empty() { kind.to_owned() } else { format!("{kind}: {detail}") }))
            }
        }
    }
}

/// Keeps a response out of every cache.
fn no_store() {
    if let Some(res) = use_context::<leptos_axum::ResponseOptions>() {
        res.insert_header(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
    }
}

/// Keeps a redirect to a link out of every cache, and the link out of the
/// referrer of the request that follows it. Only a redirect's: a page's
/// policy is the rest of the UI's too, since its links move within one
/// document, and `no-referrer` sends every form after it with `Origin: null`.
fn no_store_redirect() {
    if let Some(res) = use_context::<leptos_axum::ResponseOptions>() {
        res.insert_header(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
        res.insert_header(axum::http::header::REFERRER_POLICY, axum::http::HeaderValue::from_static("no-referrer"));
    }
}

/// A QR code of `text` as one SVG path, a unit square per dark module, and
/// the side of its grid: the code and a quiet zone of 4 around it.
fn qr_path(text: &str) -> Result<(u32, String), String> {
    let code = qrcode::QrCode::new(text.as_bytes()).map_err(|e| e.to_string())?;
    let width = code.width();
    let mut d = String::new();
    for (i, c) in code.to_colors().iter().enumerate() {
        if *c == qrcode::Color::Dark {
            let _ = write!(d, "M{} {}h1v1h-1z", i % width + 4, i / width + 4);
        }
    }
    Ok((u32::try_from(width + 8).unwrap_or(0), d))
}

/// Percent-encodes a query value.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// The UI's content security policy. Scripts are the UI's own and the
/// page's hydration script, by its nonce; `style_elem` is where style
/// elements may come from, which xterm.js makes inline.
#[must_use]
pub fn csp(opts: &Options, nonce: Option<&str>, style_elem: &str) -> String {
    let nonce = nonce.map(|n| format!(" 'nonce-{n}'")).unwrap_or_default();
    format!(
        "default-src 'self'; script-src 'self'{nonce} 'wasm-unsafe-eval'; style-src 'self'; style-src-elem {style_elem}; \
         style-src-attr 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; \
         form-action 'self' https://*.{}",
        opts.domain()
    )
}

/// The version of the site the UI serves: its directory is a new store
/// path with every build, so the assets' URLs change with it.
#[must_use]
pub fn asset_version(site: &std::path::Path) -> String {
    use std::hash::{Hash, Hasher};
    let site = site.canonicalize().unwrap_or_else(|_| site.to_owned());
    let mut h = std::hash::DefaultHasher::new();
    site.hash(&mut h);
    format!("{:012x}", h.finish() & 0xffff_ffff_ffff)
}

/// Sets the page's policy with its nonce: only the console's allows
/// inline style elements, for xterm.js.
pub fn page_csp(console: bool) {
    let (Some(state), Some(res)) = (use_context::<AppState>(), use_context::<leptos_axum::ResponseOptions>()) else {
        return;
    };
    let nonce = leptos::nonce::use_nonce();
    let style = if console { "'self' 'unsafe-inline'" } else { "'self'" };
    if let Ok(v) = csp(&state.opts, nonce.as_deref(), style).parse() {
        res.insert_header(axum::http::header::CONTENT_SECURITY_POLICY, v);
    }
}

#[cfg(test)]
mod tests;
