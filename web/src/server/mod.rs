//! The web UI's server: it renders the pages and answers their server
//! functions, streams live updates, serves the shared consoles and nginx's
//! access check for every workspace. It runs as its own user: systemd
//! state and the VMs' sockets are readable to it, and polkit lets it start,
//! stop and restart workspace units and nothing else.

pub mod authz;
pub mod console;
mod routes;
mod view;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use leptos::config::LeptosOptions;
use leptos::prelude::*;
use workbench::config::{self, Config};
use workbench::state::{self, SignInError};
use workbench_model::{HostView, Snapshot, State, Verb, Viewer, Workspace, is_user_name};

pub use routes::{authz_router, ui_router};

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
    /// The app's sign-in link.
    fn sign_in(&self, c: Config) -> BoxFuture<'_, Result<String, SignInError>>;
    /// Who a browser's cookies are signed in as; empty for nobody.
    fn whoami<'a>(&'a self, cookie: &'a str) -> BoxFuture<'a, anyhow::Result<String>>;
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
    /// Where the access check asks who a cookie belongs to.
    pub oauth2_proxy: String,
    /// The single sign-on cookie, which no workspace's app ever gets.
    pub cookie_name: String,
    pub reserve_percent: f64,
    pub reserve_min_gib: f64,
    /// The host's memory pools and their status files, for the host page.
    pub pools: Vec<(String, PathBuf)>,
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

    fn sign_in(&self, c: Config) -> BoxFuture<'_, Result<String, SignInError>> {
        Box::pin(async move { state::sign_in_link(&c).await })
    }

    fn whoami<'a>(&'a self, cookie: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(authz::ask_oauth2_proxy(&self.opts.oauth2_proxy, cookie))
    }
}

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
    pub leptos: LeptosOptions,
    pub consoles: console::Consoles,
    cache: tokio::sync::Mutex<Cache>,
    pub(crate) authz: Mutex<HashMap<String, (String, Instant)>>,
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
    #[must_use]
    pub fn new(opts: Options, backend: Box<dyn Backend>, leptos: LeptosOptions) -> Self {
        let consoles = console::Consoles::new(opts.dirs.clone());
        AppState(Arc::new(Inner {
            opts,
            backend,
            leptos,
            consoles,
            cache: tokio::sync::Mutex::default(),
            authz: Mutex::default(),
        }))
    }

    /// Who `name` is here.
    #[must_use]
    pub fn viewer(&self, name: &str) -> Option<Viewer> {
        is_user_name(name).then(|| Viewer { name: name.to_owned(), admin: self.opts.admins.iter().any(|a| a == name) })
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

    /// Forgets the collected state, after an action changed it.
    async fn forget(&self) {
        self.cache.lock().await.at = None;
    }

    /// What `viewer` sees.
    pub async fn snapshot_for(&self, viewer: &Viewer) -> anyhow::Result<Snapshot> {
        let all = self.all().await?;
        Ok(Snapshot::of(viewer.clone(), all.into_iter().map(view::row)))
    }

    /// The host page.
    pub async fn host_view(&self) -> anyhow::Result<HostView> {
        let all = self.all().await?;
        view::host(&self.opts, &all)
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
        {
            let mut cache = self.cache.lock().await;
            if verb == Verb::Restart {
                cache.restarts.insert(name.to_owned(), Instant::now());
            } else {
                cache.restarts.remove(name);
            }
        }
        self.forget().await;
        if !stay {
            see_other(&format!("/?w={name}"));
        }
        Ok(())
    }

    /// The `sign_in` server function: a `303` to the app's link, which is
    /// never shown or logged, or to the sign-in page with why not.
    pub async fn sign_in_action(&self, viewer: &Viewer, name: &str) -> Result<(), ServerFnError> {
        let ws = self.find(viewer, name).await?;
        let page = format!("/w/{name}/sign-in");
        if ws.sign_in_blocker().is_some() {
            see_other(&page);
            return Ok(());
        }
        let c =
            self.backend.config(name).map_err(|e| refuse(&format!("{e:#}"), axum::http::StatusCode::BAD_GATEWAY))?;
        match self.backend.sign_in(c).await {
            Ok(link) => {
                if let Some(res) = use_context::<leptos_axum::ResponseOptions>() {
                    res.insert_header(
                        axum::http::header::CACHE_CONTROL,
                        axum::http::HeaderValue::from_static("no-store"),
                    );
                    res.insert_header(
                        axum::http::header::REFERRER_POLICY,
                        axum::http::HeaderValue::from_static("no-referrer"),
                    );
                }
                see_other(&link);
            }
            Err(e) => {
                let (kind, detail) = match &e {
                    SignInError::Starting(_) => ("starting", String::new()),
                    SignInError::Timeout => ("timeout", String::new()),
                    SignInError::NotALink => ("not-a-link", String::new()),
                    SignInError::Failed { exit_code: 0, detail } => ("failed", detail.clone()),
                    SignInError::Failed { exit_code, detail } => {
                        ("failed", format!("exit status {exit_code}\n{detail}"))
                    }
                };
                let detail: String = detail.chars().take(2000).collect();
                see_other(&format!("{page}?error={kind}&detail={}", encode(&detail)));
            }
        }
        Ok(())
    }
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
