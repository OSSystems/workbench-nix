//! The web UI's server: it renders the pages and answers their server
//! functions, streams live updates, serves the shared consoles and nginx's
//! access check for every workspace. It runs as its own user: systemd
//! state and the VMs' sockets are readable to it, and polkit lets it start,
//! stop and restart workspace units and nothing else.

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
use workbench::config::{self, Config};
use workbench::self_service::Paths;
use workbench::state::{self, OpenLinkError};
use workbench_model::apps::{BootApp, DeviceLink};
use workbench_model::self_service::Limits;
use workbench_model::{HostView, OpenBlocker, Snapshot, State, Verb, Viewer, Workspace, is_user_name};

pub use connect::connect_router;
pub use routes::{authz_router, ui_router};
pub use self_service::CreateForm;
pub use workbench::self_service::Paths as SelfServicePaths;

/// A future a [`Backend`] returns.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the UI does on the host; tests replace it.
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
    pub dirs: Vec<PathBuf>,
    pub state_root: PathBuf,
    /// The UI's own origin, `https://<domain>`; every `POST` must come from
    /// it.
    pub origin: String,
    pub admins: Vec<String>,
    /// Groups whose members are admins.
    pub admin_groups: Vec<String>,
    /// Groups whose members make workspaces of their own.
    pub creator_groups: Vec<String>,
    /// Set when the host lets people make workspaces.
    pub self_service: Option<SelfService>,
    /// Where the access check asks who a cookie belongs to.
    pub oauth2_proxy: String,
    /// The single sign-on cookie, which no workspace's app ever gets.
    pub cookie_name: String,
    pub reserve_percent: f64,
    pub reserve_min_gib: f64,
    /// The host's memory pools and their status files, for the host page.
    pub pools: Vec<(String, PathBuf)>,
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

/// The host as the UI sees it, through the workbench crate.
pub struct HostBackend {
    pub opts: Options,
}

impl Backend for HostBackend {
    fn list(&self) -> BoxFuture<'_, anyhow::Result<Vec<Workspace>>> {
        Box::pin(state::collect(&self.opts.dirs, &self.opts.state_root))
    }

    fn control<'a>(&'a self, verb: Verb, name: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(state::control(&self.opts.state_root, verb, name, false))
    }

    fn config(&self, name: &str) -> anyhow::Result<Config> {
        config::load(&self.opts.dirs, name)
    }

    fn reachable(&self, c: &Config) -> Vec<BootApp> {
        state::reachable_apps(c)
    }

    fn open_link(&self, c: Config, app: String) -> BoxFuture<'_, Result<String, OpenLinkError>> {
        Box::pin(async move { state::open_link(&c, &app).await })
    }

    fn whoami<'a>(&'a self, cookie: &'a str) -> BoxFuture<'a, anyhow::Result<Identity>> {
        Box::pin(authz::ask_oauth2_proxy(&self.opts.oauth2_proxy, cookie))
    }

    fn host_ca(&self) -> anyhow::Result<String> {
        let raw = vmkit::hostutil::read_bounded(&self.opts.host_ca, 64 << 10)
            .with_context(|| self.opts.host_ca.display().to_string())?;
        Ok(String::from_utf8_lossy(&raw).trim().to_owned())
    }

    fn sign_user_key<'a>(&'a self, user: &'a str, id: &'a str, key: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(connect::sign_with(&self.opts.ssh_keygen, &self.opts.user_ca, user, id, key))
    }

    fn ssh_address(&self, c: &Config) -> String {
        format!("{}:22", c.ip)
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
}

/// How long the UI waits for making or destroying a workspace: a new disk
/// is a copy of the image. Longer than the units' own 10 minutes, so their
/// outcome is there to read.
const UNIT_TIMEOUT: Duration = Duration::from_secs(660);

/// How long a collected state is reused, whoever asks.
const SNAPSHOT_FOR: Duration = Duration::from_secs(1);
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
    pub consoles: console::Consoles,
    cache: tokio::sync::Mutex<Cache>,
    pub(crate) authz: Mutex<HashMap<String, (Identity, Instant)>>,
    /// The names being made now, so two requests for one never overwrite
    /// each other's.
    pub(crate) making: Mutex<std::collections::HashSet<String>>,
}

struct Cache {
    at: Option<Instant>,
    all: Result<Vec<Workspace>, String>,
    /// When someone asked for a restart, by workspace.
    restarts: HashMap<String, Instant>,
}

impl Default for Cache {
    fn default() -> Self {
        Cache { at: None, all: Ok(vec![]), restarts: HashMap::new() }
    }
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
        let consoles = console::Consoles::new(opts.dirs.clone());
        let devices = connect::Devices::load(&opts.devices)?;
        Ok(AppState(Arc::new(Inner {
            opts,
            backend,
            devices,
            leptos,
            consoles,
            cache: tokio::sync::Mutex::default(),
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

    /// Every workspace, collected at most once a second whoever asks.
    pub async fn all(&self) -> anyhow::Result<Vec<Workspace>> {
        let mut cache = self.cache.lock().await;
        if cache.at.is_none_or(|at| at.elapsed() >= SNAPSHOT_FOR) {
            cache.all = self.backend.list().await.map_err(|e| format!("{e:#}"));
            cache.at = Some(Instant::now());
            let Cache { all, restarts, .. } = &mut *cache;
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
        cache.all.clone().map_err(anyhow::Error::msg)
    }

    /// Shows a workspace as restarting until it is up again, after asking
    /// for its restart.
    async fn restarted(&self, name: &str) {
        self.cache.lock().await.restarts.insert(name.to_owned(), Instant::now());
        self.forget().await;
    }

    /// Forgets the collected state, after an action changed it.
    async fn forget(&self) {
        self.cache.lock().await.at = None;
    }

    /// What `viewer` sees, and the apps a workspace made at run time may
    /// have.
    pub async fn snapshot_for(&self, viewer: &Viewer) -> anyhow::Result<Snapshot> {
        let all = self.all().await?;
        let mut snap = Snapshot::of(viewer.clone(), all.into_iter().map(view::row));
        if let Some(ss) = &self.opts.self_service {
            snap.apps.clone_from(&ss.limits.apps);
        }
        Ok(snap)
    }

    /// The host page.
    pub async fn host_view(&self) -> anyhow::Result<HostView> {
        let all = self.all().await?;
        let mut host = view::host(&self.opts, &all)?;
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
            return Err(ServerFnError::new(format!("{name} is already {}", ws.state)));
        }
        if let Err(e) = self.backend.control(verb, name).await {
            let detail = format!("{e:#}");
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
