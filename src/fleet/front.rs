//! The front: [`Fleet`], what it knows of every workspace and does through
//! the nodes, which the web UI and the front's units call; and
//! `workbench front serve`, its API, root's, which answers the nodes'
//! `WireGuard` addresses only:
//!
//! | Method | Path | What |
//! |---|---|---|
//! | `POST` | `/start/<ws>` | what starting it needs, for a workspace the registry has on the asking node and that isn't moving away from it |

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use axum::Router;
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use tokio_tungstenite::WebSocketStream;
use vmkit::hostutil;
use vmkit::memory::GIB;
use workbench_model::apps::{self, App, Boot, BootApp, app_url};
use workbench_model::fleet::{NodeRefusal, NodeState, NodeView, NodeWorkspace, OpenLinkAnswer, Record};
use workbench_model::memory::Ceiling;
use workbench_model::{State as WsState, Verb, Workspace};

use super::client::{CallError, Conn, Node, NodeClient};
use super::contract::{B64, StartAnswer, WireHostKey};
use super::http::{self, Peer, error};
use super::registry::Registry;
use super::{FrontSettings, is_closed};
use crate::config::Config;
use crate::self_service::{Paths, Refused, left_marker, refuse};
use crate::state::OpenLinkError;

/// Where a declared workspace's secrets are when Nix says nothing.
const SECRETS_ROOT: &str = "/var/lib/workbench-secrets";
/// How long the list waits for a node.
const LIST_TIMEOUT: Duration = Duration::from_secs(8);

/// The front: its settings, its registry and its nodes.
pub struct Fleet {
    pub settings: FrontSettings,
    pub registry: Registry,
    nodes: BTreeMap<String, Arc<dyn Node>>,
    /// One host certificate signed at a time.
    signing: tokio::sync::Mutex<()>,
    /// How long a moved workspace's agent may take to answer.
    pub start_wait: Duration,
    /// How long a disk stream may be silent before a move gives up on it.
    pub copy_idle: Duration,
}

impl Fleet {
    /// The fleet its settings describe, each node asked over HTTP.
    #[must_use]
    pub fn new(settings: FrontSettings) -> Self {
        let nodes = settings
            .nodes
            .iter()
            .map(|(name, n)| (name.clone(), Arc::new(NodeClient::new(n.api.clone())) as Arc<dyn Node>))
            .collect();
        Fleet::with_nodes(settings, nodes)
    }

    /// A fleet whose nodes are these: tests give fakes.
    #[must_use]
    pub fn with_nodes(settings: FrontSettings, nodes: BTreeMap<String, Arc<dyn Node>>) -> Self {
        let registry = Registry::of(&settings);
        Fleet {
            settings,
            registry,
            nodes,
            signing: tokio::sync::Mutex::default(),
            start_wait: super::moving::START_WAIT,
            copy_idle: super::disk::IDLE,
        }
    }

    /// This machine's fleet, if it is a front.
    pub fn of_this_machine() -> anyhow::Result<Option<Self>> {
        Ok(FrontSettings::of_this_machine()?.map(Fleet::new))
    }

    #[must_use]
    pub fn state_root(&self) -> PathBuf {
        self.settings.state_root()
    }

    /// Where the UI's requests, their outcomes and the quotas are.
    #[must_use]
    pub fn paths(&self) -> Paths {
        Paths(self.state_root())
    }

    /// A node by name.
    pub fn node(&self, name: &str) -> anyhow::Result<&Arc<dyn Node>> {
        self.nodes.get(name).with_context(|| format!("{name:?} isn't a node of this fleet"))
    }

    /// A workspace's record, or a refusal people are shown.
    pub fn record(&self, name: &str) -> anyhow::Result<Record> {
        match self.registry.get(name)? {
            Some(r) if !r.pending => Ok(r),
            Some(_) => Err(refuse(format!("{name} is being made or taken away just now."))),
            None => Err(refuse(format!("There is no workspace {name}."))),
        }
    }

    /// Every node, with what it says of itself, or why it says nothing.
    pub async fn nodes_view(&self) -> Vec<NodeView> {
        let root = self.state_root();
        let root = &root;
        let asks = self.nodes.iter().map(|(name, node)| async move {
            let closed = is_closed(root, name);
            match node.host().await {
                Ok(h) => NodeView {
                    name: name.clone(),
                    state: if closed { NodeState::Closed } else { NodeState::Up },
                    host: Some(h),
                    detail: String::new(),
                },
                Err(e @ CallError::Version(_)) => {
                    NodeView { name: name.clone(), state: NodeState::OtherVersion, host: None, detail: e.to_string() }
                }
                Err(e) => {
                    NodeView { name: name.clone(), state: NodeState::Unreachable, host: None, detail: e.to_string() }
                }
            }
        });
        futures_util::future::join_all(asks).await
    }

    /// What every node has, at once; an error for a node that doesn't
    /// answer.
    async fn node_workspaces(&self) -> BTreeMap<String, Result<Vec<NodeWorkspace>, CallError>> {
        let asks = self.nodes.iter().map(|(name, node)| async move {
            let got = tokio::time::timeout(LIST_TIMEOUT, node.workspaces())
                .await
                .unwrap_or_else(|_| Err(CallError::Unreachable("no answer in time".into())));
            (name.clone(), got)
        });
        futures_util::future::join_all(asks).await.into_iter().collect()
    }

    /// Every workspace, as `/api/workspaces` has them: each one's owner,
    /// address and node from the registry, its state from its node,
    /// `moving` while it moves and `unreachable` while its node doesn't
    /// answer; then the disks nodes have that the registry doesn't, the
    /// admins'.
    pub async fn list(&self) -> anyhow::Result<Vec<Workspace>> {
        let (records, unreadable) = self.registry.all();
        for e in &unreadable {
            eprintln!("the registry: {e}");
        }
        let on = self.node_workspaces().await;
        let known: BTreeSet<&str> = records.iter().map(|r| r.name.as_str()).collect();
        let mut out: Vec<Workspace> = records
            .iter()
            .filter(|r| !r.pending)
            .map(|r| {
                let answer = on.get(&r.node);
                let has =
                    answer.and_then(|a| a.as_ref().ok()).and_then(|l| l.iter().find(|w| w.workspace.name == r.name));
                self.workspace_of(r, has, answer.is_some_and(Result::is_ok))
            })
            .collect();
        for (node, answer) in &on {
            for w in answer.iter().flatten().filter(|w| !known.contains(w.workspace.name.as_str())) {
                out.push(Workspace {
                    configured: false,
                    owner: String::new(),
                    url: String::new(),
                    self_service: false,
                    node: node.clone(),
                    ..w.workspace.clone()
                });
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// One workspace, from its record and what its node said of it.
    fn workspace_of(&self, r: &Record, has: Option<&NodeWorkspace>, answered: bool) -> Workspace {
        let mut ws = has.map_or_else(
            || Workspace { name: r.name.clone(), unit: "-".into(), vm: "-".into(), ..Workspace::default() },
            |w| w.workspace.clone(),
        );
        ws.configured = !r.owner.is_empty();
        ws.owner.clone_from(&r.owner);
        ws.url = if ws.configured { self.settings.url(&r.name) } else { String::new() };
        ws.ip.clone_from(&r.ip);
        if !r.os.is_empty() {
            ws.os.clone_from(&r.os);
        }
        ws.self_service = !r.declared;
        ws.node.clone_from(&r.node);
        ws.owner_left = left_marker(&self.state_root(), &r.name).exists();
        if !r.declared {
            // What the next start brings is the record's.
            let next = self.next_boot(r, has);
            let running = has.and_then(|w| w.booted.as_ref());
            let open_links: Vec<&str> = has
                .map(|w| w.workspace.apps.iter().filter(|a| a.open_link).map(|a| a.id.as_str()).collect())
                .unwrap_or_default();
            ws.changes = if running.is_some() { apps::changes(running, &next) } else { vec![] };
            ws.apps = reachable(&next, running)
                .into_iter()
                .map(|a| App {
                    url: a.port.map(|_| app_url(&ws.url, &a.id)),
                    open_link: a.port.is_some() && open_links.contains(&a.id.as_str()),
                    id: a.id,
                    name: a.name,
                    connect: a.connect,
                })
                .collect();
            ws.memory_auto = r.max_memory == Ceiling::Auto;
            if let Ceiling::Size(gib) = r.max_memory {
                ws.max_memory_bytes = gib * GIB;
            }
        }
        if !answered {
            ws.state = WsState::Unreachable;
            ws.since = None;
        }
        if r.is_moving() {
            ws.state = WsState::Moving;
            ws.since = None;
        }
        ws
    }

    /// What a workspace made at run time boots next: the record's apps,
    /// with the packages its node gives it.
    fn next_boot(&self, r: &Record, has: Option<&NodeWorkspace>) -> Boot {
        Boot {
            packages: has.map(|w| w.next.packages.clone()).unwrap_or_default(),
            apps: self.settings.apps_of(&r.apps),
        }
    }

    /// A workspace as the access check and the tunnel need it: its owner,
    /// its address and its apps, from the registry, never a node.
    pub fn config(&self, name: &str) -> anyhow::Result<Config> {
        let r = self.record(name)?;
        anyhow::ensure!(!r.owner.is_empty(), "{name} was removed from the configuration");
        Ok(Config {
            name: r.name.clone(),
            owner: r.owner.clone(),
            url: self.settings.url(&r.name),
            ip: r.ip.clone(),
            os: r.os.clone(),
            apps: self.settings.apps_of(&r.apps),
            self_service: !r.declared,
            ..Config::default()
        })
    }

    /// A workspace's record, for an action on it: not while it moves.
    fn acting_on(&self, name: &str) -> anyhow::Result<Record> {
        let r = self.record(name)?;
        if r.is_moving() {
            return Err(moving(name));
        }
        Ok(r)
    }

    /// Starts, stops or restarts a workspace through its node, without
    /// waiting for it.
    pub async fn control(&self, name: &str, verb: Verb) -> anyhow::Result<()> {
        let r = self.acting_on(name)?;
        self.node(&r.node)?.control(name, verb, false).await.map_err(|e| node_error(&r.node, e))
    }

    /// A link that signs a browser in to an app of the workspace, through
    /// its node.
    pub async fn open_link(&self, name: &str, app: &str) -> Result<String, OpenLinkError> {
        let r = self.acting_on(name).map_err(|e| OpenLinkError::Starting(format!("{e:#}")))?;
        let node = self.node(&r.node).map_err(|e| OpenLinkError::Starting(format!("{e:#}")))?;
        match node.open_link(name, app).await {
            Ok(a) => link_of(a),
            Err(e) => Err(OpenLinkError::Starting(format!("node {}: {e}", r.node))),
        }
    }

    /// A workspace's shared console, through its node: binary messages
    /// carry its bytes both ways, text messages its status as JSON.
    pub async fn console(&self, name: &str, viewer: &str) -> anyhow::Result<WebSocketStream<Conn>> {
        let r = self.acting_on(name)?;
        let n = self.settings.node(&r.node)?;
        super::client::console(&n.api, name, viewer).await.map_err(|e| node_error(&r.node, e))
    }

    /// Closes a node, or reopens it.
    pub fn set_closed(&self, node: &str, closed: bool) -> anyhow::Result<()> {
        self.settings.node(node)?;
        super::set_closed(&self.state_root(), node, closed)
    }

    /// What starting a workspace on `asking` needs: refused, saying why,
    /// unless the registry has it there and it isn't moving away.
    pub async fn start_answer(&self, asking: &str, name: &str) -> Result<StartAnswer, String> {
        let r = match self.registry.get(name) {
            Ok(Some(r)) => r,
            Ok(None) => return Err(format!("the registry has no workspace {name}")),
            Err(e) => return Err(format!("{e:#}")),
        };
        if r.pending {
            return Err(format!("{name} is being made or taken away"));
        }
        if r.node != asking {
            return Err(format!("{name} is on node {}, not {asking}", r.node));
        }
        if r.is_moving() && !r.switched() {
            return Err(format!("{name} is moving away from {asking}"));
        }
        if r.owner.is_empty() {
            return Err(format!("{name} was removed from the configuration"));
        }
        self.answer(&r).await.map_err(|e| format!("{e:#}"))
    }

    async fn answer(&self, r: &Record) -> anyhow::Result<StartAnswer> {
        let (dir, files) = if r.declared {
            let dir =
                if r.secrets_dir.is_empty() { format!("{SECRETS_ROOT}/{}", r.name) } else { r.secrets_dir.clone() };
            (dir, r.environment_files.clone())
        } else {
            let ss = self.settings.self_service.as_ref().context("this front makes no workspaces at run time")?;
            (format!("{}/{}", ss.secrets_root, r.name), ss.environment_files.clone())
        };
        let secrets =
            tokio::task::spawn_blocking(move || crate::launch::collect_secrets(Path::new(&dir), &files)).await??;
        let ssh = self.settings.ssh.clone();
        let (host_key, user_ca) = if ssh.is_empty() {
            (None, None)
        } else {
            let _one = self.signing.lock().await;
            let (name, host) = (r.name.clone(), format!("{}.{}", r.name, self.settings.domain));
            let ca = ssh.clone();
            let key = tokio::task::spawn_blocking(move || crate::launch::sign_host_key(&ca, &name, &host)).await??;
            let user_ca = hostutil::read_bounded(&ssh.user_ca, 64 << 10).with_context(|| ssh.user_ca.clone())?;
            (Some(WireHostKey::from(&key)), Some(B64(user_ca)))
        };
        Ok(StartAnswer {
            record: (!r.declared).then(|| Record { moving: None, ..r.clone() }),
            secrets: (&secrets).try_into()?,
            host_key,
            user_ca,
        })
    }
}

/// Refuses an action on a workspace that moves.
pub(crate) fn moving(name: &str) -> anyhow::Error {
    refuse(format!("{name} is moving to another node; try again once it's there."))
}

/// The apps a browser may reach: the next start's, then those only the
/// running VM has.
#[must_use]
pub fn reachable(next: &Boot, running: Option<&Boot>) -> Vec<BootApp> {
    let only_running = running.into_iter().flat_map(|r| &r.apps).filter(|a| next.app(&a.id).is_none());
    next.apps.iter().chain(only_running).cloned().collect()
}

/// A node's open-link answer, as the UI takes it.
pub fn link_of(a: OpenLinkAnswer) -> Result<String, OpenLinkError> {
    match a {
        OpenLinkAnswer::Link { link } => Ok(link),
        OpenLinkAnswer::Starting { detail } => Err(OpenLinkError::Starting(detail)),
        OpenLinkAnswer::Failed { exit_code, detail } => Err(OpenLinkError::Failed { exit_code, detail }),
        OpenLinkAnswer::NotALink => Err(OpenLinkError::NotALink),
        OpenLinkAnswer::Timeout => Err(OpenLinkError::Timeout),
    }
}

/// A node's failure, as people are told it: a refusal, or that it doesn't
/// answer.
#[must_use]
pub fn node_error(node: &str, e: CallError) -> anyhow::Error {
    let node = node.to_owned();
    match e {
        CallError::Refused { status: 400..=499, error } => Refused(error).into(),
        CallError::Refused { error, .. } => NodeRefusal::Failed { node, error }.into(),
        CallError::Unreachable(_) => NodeRefusal::Unreachable { node }.into(),
        CallError::Version(build) => NodeRefusal::OtherBuild { node, build }.into(),
    }
}

/// The front's API.
pub fn router(fleet: Arc<Fleet>) -> Router {
    Router::new()
        .route("/start/{ws}", post(start))
        .layer(middleware::from_fn_with_state(fleet.clone(), a_node))
        .layer(middleware::from_fn(http::versioned))
        .with_state(fleet)
}

/// The node a request comes from.
#[derive(Debug, Clone)]
struct Asking(String);

/// Takes requests from a node's `WireGuard` address, never one behind it,
/// or from the node on this machine through the socket.
async fn a_node(State(fleet): State<Arc<Fleet>>, mut req: Request, next: Next) -> Response {
    let node = match http::peer(&req) {
        Some(Peer::Local) if !fleet.settings.local_node.is_empty() => fleet.settings.local_node.clone(),
        Some(Peer::Ip(IpAddr::V4(ip))) => match fleet.settings.node_at(ip) {
            Some(n) => n.to_owned(),
            None => return error(StatusCode::FORBIDDEN, format!("{ip} isn't a node's address")),
        },
        _ => return error(StatusCode::FORBIDDEN, "who is asking?"),
    };
    req.extensions_mut().insert(Asking(node));
    next.run(req).await
}

async fn start(
    State(fleet): State<Arc<Fleet>>,
    axum::Extension(Asking(node)): axum::Extension<Asking>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    if !workbench_model::is_workspace_name(&name) {
        return error(StatusCode::BAD_REQUEST, "invalid workspace name");
    }
    match fleet.start_answer(&node, &name).await {
        Ok(a) => axum::Json(a).into_response(),
        Err(why) => {
            eprintln!("node {node} asked to start {name}: refused: {why}");
            error(StatusCode::FORBIDDEN, why)
        }
    }
}

/// `workbench front serve`: the API on the front's `WireGuard` address and,
/// on a machine with both roles, its socket, root's.
pub async fn serve(settings: FrontSettings) -> anyhow::Result<()> {
    let (listen, socket) = (settings.listen, settings.socket.clone());
    let app = router(Arc::new(Fleet::new(settings)));
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
            http::serve_unix(Path::new(&socket), "", app.clone()).await
        }
    };
    tokio::select! {
        r = tcp => r.context("the API"),
        r = unix => r.context("the API's socket"),
    }
}

#[cfg(test)]
mod tests;
