//! The UI's handlers against a fake host: who sees and controls what, the
//! access check, the origin rule, and the pages the server renders.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures_util::{SinkExt, StreamExt};
use tower::ServiceExt;
use workbench::config::Config;
use workbench::self_service::Paths;
use workbench::state::OpenLinkError;
use workbench_model::apps::{AppInfo, BootApp};
use workbench_model::memory::Ceiling;
use workbench_model::self_service::{AppsRequest, Limits, MemoryRequest, Outcome, Request as Asked};
use workbench_model::{App, State, Verb, Workspace};

use super::*;

const ORIGIN: &str = "https://ws.test";

#[derive(Default)]
struct Fake {
    controlled: Arc<Mutex<Vec<(Verb, String)>>>,
    /// alice's workspace has Herdr too, an app without an address.
    herdr: bool,
    open_link: Mutex<Option<Result<String, OpenLinkError>>>,
    /// Workspaces made at run time, besides the declared ones.
    made: Vec<Workspace>,
    /// Where the units write how they went, and what they say.
    paths: Option<Paths>,
    outcome: Arc<Mutex<Option<Outcome>>>,
    units: Arc<Mutex<Vec<String>>>,
    /// The keys the UI had signed, and for whom.
    signed: Arc<Mutex<Vec<(String, String)>>>,
    /// Where the workspaces' SSH servers listen.
    ssh: String,
}

fn made(name: &str, owner: &str) -> Workspace {
    Workspace { self_service: true, ip: "10.232.0.20".into(), ..ws(name, owner, State::Running) }
}

fn ws(name: &str, owner: &str, state: State) -> Workspace {
    Workspace {
        name: name.into(),
        owner: owner.into(),
        configured: true,
        url: format!("https://{name}.ws.test"),
        ip: "10.232.0.10".into(),
        os: "ubuntu".into(),
        state,
        apps: vec![App {
            id: "t3code".into(),
            name: "T3 Code".into(),
            url: Some(format!("https://{name}--t3code.ws.test")),
            open_link: true,
            ..App::default()
        }],
        unit: "active".into(),
        vm: if state == State::Stopped { "-".into() } else { "Running".into() },
        ..Workspace::default()
    }
}

impl Backend for Fake {
    fn list(&self) -> BoxFuture<'_, anyhow::Result<Vec<Workspace>>> {
        let mut alice = ws("alice", "alice", State::Running);
        if self.herdr {
            alice.apps.push(App {
                id: "herdr".into(),
                name: "Herdr".into(),
                connect: vec!["`herdr --remote @host@`".into()],
                ..App::default()
            });
        }
        let mut all = vec![alice, ws("bob", "bob", State::Stopped), ws("bobby", "bob", State::Stopping)];
        all.extend(self.made.iter().cloned());
        Box::pin(async move { Ok(all) })
    }

    fn control<'a>(&'a self, verb: Verb, name: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
        self.controlled.lock().unwrap().push((verb, name.to_owned()));
        Box::pin(async { Ok(()) })
    }

    fn config(&self, name: &str) -> anyhow::Result<Config> {
        if let Some(w) = self.made.iter().find(|w| w.name == name) {
            return Ok(Config {
                name: name.into(),
                owner: w.owner.clone(),
                url: w.url.clone(),
                ip: w.ip.clone(),
                apps: vec![t3code()],
                self_service: true,
                ..Config::default()
            });
        }
        let owner = match name {
            "alice" => "alice",
            "bob" | "bobby" => "bob",
            _ => anyhow::bail!("no workspace {name:?}"),
        };
        let mut apps = vec![t3code()];
        if self.herdr && name == "alice" {
            apps.push(BootApp {
                id: "herdr".into(),
                name: "Herdr".into(),
                port: None,
                bundle: "/nix/store/x-herdr".into(),
                ..BootApp::default()
            });
        }
        Ok(Config {
            name: name.into(),
            owner: owner.into(),
            url: format!("https://{name}.ws.test"),
            ip: "10.232.0.10".into(),
            apps,
            ..Config::default()
        })
    }

    fn reachable(&self, c: &Config) -> Vec<BootApp> {
        c.apps.clone()
    }

    fn open_link(&self, _: Config, _: String) -> BoxFuture<'_, Result<String, OpenLinkError>> {
        let answer =
            self.open_link.lock().unwrap().clone().unwrap_or(Ok("https://alice--t3code.ws.test/pair#token=t".into()));
        Box::pin(async move { answer })
    }

    fn whoami<'a>(&'a self, cookie: &'a str) -> BoxFuture<'a, anyhow::Result<Identity>> {
        let who = who_is(cookie);
        Box::pin(async move { Ok(who) })
    }

    fn host_ca(&self) -> anyhow::Result<String> {
        Ok("ssh-ed25519 AAAAhostca workbench host CA".into())
    }

    fn sign_user_key<'a>(&'a self, user: &'a str, _: &'a str, key: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
        self.signed.lock().unwrap().push((user.to_owned(), key.to_owned()));
        let cert = format!("ssh-ed25519-cert-v01@openssh.com AAAAcert-of-{user}");
        Box::pin(async move { Ok(cert) })
    }

    fn ssh_address(&self, _: &Config) -> String {
        self.ssh.clone()
    }

    fn run_unit<'a>(&'a self, unit: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
        self.units.lock().unwrap().push(unit.to_owned());
        let outcome = self.outcome.lock().unwrap().clone();
        let ran = ran_unit(self.paths.as_ref(), unit, outcome.as_ref());
        Box::pin(async move { ran })
    }
}

/// Who a fake cookie names: `<user>` or `<user>:<group>,<group>`.
fn who_is(cookie: &str) -> Identity {
    let value = cookie.split(';').map(str::trim).find_map(|c| c.strip_prefix("_workbench_auth=")).unwrap_or_default();
    let (user, groups) = value.split_once(':').unwrap_or((value, ""));
    Identity { user: user.to_owned(), groups: authz::groups(groups) }
}

/// A unit as the fakes run it: it writes `outcome` where its kind's go,
/// and fails when that says so.
fn ran_unit(paths: Option<&Paths>, unit: &str, outcome: Option<&Outcome>) -> anyhow::Result<()> {
    let name = unit.split_once('@').map(|(_, r)| r.trim_end_matches(".service")).unwrap_or_default();
    if let (Some(paths), Some(o)) = (paths, outcome) {
        let out = match unit.split_once('@').map_or("", |(kind, _)| kind) {
            "workbench-apps" => paths.apps_outcome(name),
            "workbench-memory" => paths.memory_outcome(name),
            "workbench-move" => paths.move_outcome(name),
            _ => paths.outcome(name),
        };
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        std::fs::write(out, serde_json::to_vec(o).unwrap()).unwrap();
    }
    anyhow::ensure!(outcome.is_none_or(|o| o.ok), "the unit failed");
    Ok(())
}

fn t3code() -> BootApp {
    BootApp {
        id: "t3code".into(),
        name: "T3 Code".into(),
        port: Some(3773),
        bundle: "/nix/store/x-t3code".into(),
        ..BootApp::default()
    }
}

fn state(fake: Fake) -> AppState {
    state_with(fake, |_| {})
}

/// The state with the host's limits as `limits` leaves them.
fn state_with(fake: Fake, limits: impl FnOnce(&mut Limits)) -> AppState {
    let mut l = Limits {
        os: vec!["ubuntu".into()],
        boot_mem_mib: 2048,
        max_memory: Ceiling::Size(16),
        host_memory_gib: 0,
        max_disk_gib: 128,
        quota: 1,
        apps: vec![
            AppInfo { id: "t3code".into(), name: "T3 Code".into(), terminal: false, default: true },
            AppInfo { id: "herdr".into(), name: "Herdr".into(), terminal: true, default: false },
        ],
    };
    limits(&mut l);
    let self_service = fake.paths.clone().map(|paths| SelfService { paths, limits: l });
    let opts = Options {
        origin: ORIGIN.into(),
        admins: vec!["carol".into()],
        admin_groups: vec!["workbench-admins".into()],
        creator_groups: vec!["workbench".into()],
        self_service,
        cookie_name: "_workbench_auth".into(),
        devices: devices_file(),
        ..Options::default()
    };
    let leptos =
        LeptosOptions::builder().output_name("workbench-web").site_root("target/site").site_pkg_dir("pkg").build();
    AppState::new(opts, Box::new(fake), leptos).unwrap()
}

/// A devices file of the test's own, in a directory the process leaves.
fn devices_file() -> std::path::PathBuf {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let dir = DIR.get_or_init(|| tempfile::tempdir().unwrap());
    dir.path().join(format!("devices-{}.json", N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)))
}

/// Leptos's reactive runtime is global to the process: tests each on a
/// tokio runtime of their own may wait on one another's when they render
/// at once. The server has one runtime, where pages render concurrently
/// (see `pages_render_concurrently`); here they take turns.
static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn ui(state: &AppState) -> axum::Router {
    ui_router(state.clone(), std::path::Path::new("target/site"))
}

async fn send(router: axum::Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let res = router.oneshot(req).await.unwrap();
    let (parts, body) = res.into_parts();
    let body = axum::body::to_bytes(body, 1 << 22).await.unwrap();
    (parts.status, parts.headers, String::from_utf8_lossy(&body).into_owned())
}

fn get(path: &str, user: &str) -> Request<Body> {
    Request::get(path).header("x-workbench-user", user).body(Body::empty()).unwrap()
}

/// `user` in `groups`, as nginx names them.
fn with_groups(mut req: Request<Body>, groups: &str) -> Request<Body> {
    req.headers_mut().insert("x-workbench-groups", groups.parse().unwrap());
    req
}

fn post(path: &str, user: &str, origin: &str, form: &str) -> Request<Body> {
    Request::post(path)
        .header("x-workbench-user", user)
        .header(header::ORIGIN, origin)
        .header(header::ACCEPT, "text/html")
        .header(header::REFERER, format!("{ORIGIN}/"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form.to_owned()))
        .unwrap()
}

#[tokio::test]
async fn nobody_without_single_sign_on() {
    let _turn = TURN.lock().await;
    let s = state(Fake::default());
    let req = Request::get("/").body(Body::empty()).unwrap();
    assert_eq!(send(ui(&s), req).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(send(ui(&s), get("/", "Not A User")).await.0, StatusCode::UNAUTHORIZED);
}

/// The page the server renders already lists the viewer's workspaces,
/// and only theirs, with a policy that lets in its own scripts only.
#[tokio::test]
async fn the_list_is_rendered_for_each_viewer() {
    let _turn = TURN.lock().await;
    let s = state(Fake::default());
    let (code, headers, body) = send(ui(&s), get("/", "alice")).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert!(body.contains(r#"data-ws="alice""#) && !body.contains(r#"data-ws="bob""#), "{body}");
    assert!(!body.contains(r#"href="/host""#), "alice is no admin");
    assert!(!body.contains("10.232.0.10"), "only admins see addresses");
    let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert!(csp.contains("script-src 'self' 'nonce-") && csp.contains("style-src-elem 'self';"), "{csp}");
    assert!(csp.contains("form-action 'self' https://*.ws.test"), "{csp}");
    let nonce = csp.split("'nonce-").nth(1).unwrap().split('\'').next().unwrap();
    assert!(body.contains(&format!("nonce=\"{nonce}\"")), "the hydration script carries the nonce");

    let (code, _, body) = send(ui(&s), get("/?w=bob", "carol")).await;
    assert_eq!(code, StatusCode::OK);
    for name in ["alice", "bob", "bobby"] {
        assert!(body.contains(&format!(r#"data-ws="{name}""#)), "carol sees {name}");
    }
    assert!(body.contains(r#"href="/host""#), "carol is an admin");
    assert!(!body.contains(r#"class="lay np"#), "a list keeps room for its side panel");

    let (code, _, body) = send(ui(&s), get("/", "dave")).await;
    assert_eq!(code, StatusCode::OK);
    assert!(body.contains("You don't have a workspace yet"), "{body}");
    assert!(body.contains(r#"class="lay np"#), "no side panel without a workspace: {body}");
}

#[tokio::test]
async fn pages_of_other_peoples_workspaces_are_forbidden() {
    let _turn = TURN.lock().await;
    let s = state(Fake::default());
    for path in ["/w/bob/stop", "/w/bob/console", "/w/nobody/restart", "/host"] {
        let (code, _, body) = send(ui(&s), get(path, "alice")).await;
        assert_eq!(code, StatusCode::FORBIDDEN, "{path}: {body}");
        assert!(body.contains("isn't yours"), "{path}");
    }
    let (code, _, body) = send(ui(&s), get("/w/alice/stop", "alice")).await;
    assert_eq!(code, StatusCode::OK);
    assert!(body.contains("It stays stopped"), "{body}");
    let (code, _, _) = send(ui(&s), get("/host", "carol")).await;
    assert_eq!(code, StatusCode::OK);
}

/// Only the console allows inline style elements, which xterm.js makes.
#[tokio::test]
async fn only_the_console_allows_inline_styles() {
    let _turn = TURN.lock().await;
    let s = state(Fake::default());
    let (code, headers, _) = send(ui(&s), get("/w/alice/console", "alice")).await;
    assert_eq!(code, StatusCode::OK);
    let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert!(csp.contains("style-src-elem 'self' 'unsafe-inline'"), "{csp}");
    assert!(!csp.contains("script-src 'self' 'unsafe-inline'"), "{csp}");
}

#[tokio::test]
async fn actions_need_the_uis_origin_and_the_workspace() {
    let _turn = TURN.lock().await;
    let fake = Fake::default();
    let calls = fake.controlled.clone();
    let s = state(fake);
    let form = "ws=alice&action=stop";
    let (code, _, _) = send(ui(&s), post("/api/control", "alice", "https://alice.ws.test", form)).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "a workspace's page posted to the UI");
    let (code, _, _) = send(ui(&s), post("/api/control", "alice", ORIGIN, "ws=bob&action=stop")).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "alice stopped bob's workspace");
    let (code, headers, body) = send(ui(&s), post("/api/control", "alice", ORIGIN, form)).await;
    assert_eq!(code, StatusCode::SEE_OTHER, "{body}");
    assert_eq!(headers[header::LOCATION], "/?w=alice");
    let (code, _, _) = send(ui(&s), post("/api/control", "carol", ORIGIN, "ws=bob&action=start")).await;
    assert_eq!(code, StatusCode::SEE_OTHER, "an admin starts anyone's");
    // A workspace already on its way somewhere is left alone.
    let (code, headers, _) = send(ui(&s), post("/api/control", "bob", ORIGIN, "ws=bobby&action=restart")).await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/w/bobby/restart?error=busy");
    assert_eq!(*calls.lock().unwrap(), [(Verb::Stop, "alice".to_owned()), (Verb::Start, "bob".to_owned())]);
}

#[tokio::test]
async fn open_sends_the_browser_to_the_apps_link() {
    let _turn = TURN.lock().await;
    let s = state(Fake::default());
    let open = "ws=alice&app=t3code";
    let (code, headers, _) = send(ui(&s), post("/api/open", "alice", ORIGIN, open)).await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "https://alice--t3code.ws.test/pair#token=t");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    let (code, _, _) = send(ui(&s), post("/api/open", "bob", ORIGIN, open)).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    // A stopped workspace has no link: the app's open page says why.
    let (code, headers, _) = send(ui(&s), post("/api/open", "bob", ORIGIN, "ws=bob&app=t3code")).await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/w/bob/open/t3code");
    let (code, _, body) = send(ui(&s), get("/w/bob/open/t3code", "bob")).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(body.contains("is stopped"), "{body}");
    let (code, _, _) = send(ui(&s), get("/w/alice/open/nope", "alice")).await;
    assert_eq!(code, StatusCode::NOT_FOUND, "an app it hasn't");

    let failing = Fake::default();
    *failing.open_link.lock().unwrap() = Some(Err(OpenLinkError::Failed { exit_code: 2, detail: "boom".into() }));
    let s = state(failing);
    let (_, headers, _) = send(ui(&s), post("/api/open", "alice", ORIGIN, open)).await;
    let to = headers[header::LOCATION].to_str().unwrap().to_owned();
    assert!(to.starts_with("/w/alice/open/t3code?error=failed&detail=exit%20status%202"), "{to}");
    let (code, _, body) = send(ui(&s), get(&to, "alice")).await;
    assert_eq!(code, StatusCode::BAD_GATEWAY);
    assert!(body.contains("couldn't make a sign-in link") && body.contains("boom"), "{body}");

    // Only a NixOS workspace is told about its slower first boot.
    let (_, _, body) = send(ui(&s), get("/w/alice/open/t3code?error=starting", "alice")).await;
    assert!(body.contains("isn't answering") && !body.contains("NixOS"), "{body}");
}

/// An app that comes with the next restart doesn't open until then, and
/// the panel says what waits for it.
#[tokio::test]
async fn what_waits_for_a_restart() {
    let _turn = TURN.lock().await;
    let mut dev = made("alice-dev", "alice");
    dev.changes = vec![workbench_model::Change::AppAdded { app: "t3code".into() }];
    let (_dir, fake) = self_serve(vec![dev], None);
    let s = state(fake);
    let (_, h, _) =
        send(ui(&s), with_groups(post("/api/open", "alice", ORIGIN, "ws=alice-dev&app=t3code"), "workbench")).await;
    assert_eq!(h[header::LOCATION], "/w/alice-dev/open/t3code");
    let (code, _, body) = send(ui(&s), with_groups(get("/w/alice-dev/open/t3code", "alice"), "workbench")).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(body.contains("comes with the next restart"), "{body}");
    let (_, _, body) = send(ui(&s), with_groups(get("/?w=alice-dev", "alice"), "workbench")).await;
    assert!(
        body.contains("T3 Code comes with the next restart.") && body.contains("T3 Code after a restart"),
        "{body}"
    );
}

/// The link for another device comes back as data, with its QR code.
#[tokio::test]
async fn a_link_for_another_device() {
    let _turn = TURN.lock().await;
    let s = state(Fake::default());
    let mut req = post("/api/device-link", "alice", ORIGIN, "ws=alice&app=t3code");
    req.headers_mut().insert(header::ACCEPT, "application/json".parse().unwrap());
    let (code, headers, body) = send(ui(&s), req).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    let d: workbench_model::apps::DeviceLink = serde_json::from_str(&body).unwrap();
    assert_eq!(d.link, "https://alice--t3code.ws.test/pair#token=t");
    assert!(d.size > 20 && d.qr.starts_with('M'), "{d:?}");
    let (code, _, _) = send(ui(&s), post("/api/device-link", "bob", ORIGIN, "ws=alice&app=t3code")).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_api_lists_what_the_viewer_sees() {
    let _turn = TURN.lock().await;
    let s = state(Fake::default());
    let (code, _, body) = send(ui(&s), get("/api/workspaces", "bob")).await;
    assert_eq!(code, StatusCode::OK);
    let all: Vec<Workspace> = serde_json::from_str(&body).unwrap();
    let names: Vec<_> = all.iter().map(|w| w.name.as_str()).collect();
    assert_eq!(names, ["bob", "bobby"]);
    assert!(all.iter().all(|w| w.ip.is_empty()));
}

#[tokio::test]
async fn the_access_check() {
    let _turn = TURN.lock().await;
    let s = state(Fake::default());
    let check = |ws: &str, cookie: &str| {
        Request::get("/authz")
            .header("x-workbench-workspace", ws)
            .header("x-workbench-app", "t3code")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
    };
    let (code, _, _) = send(authz_router(s.clone()), check("alice", "t3=1")).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED, "nobody signed in");
    let (code, h, _) = send(authz_router(s.clone()), check("alice", "_workbench_auth=bob:staff,workbench; t3=1")).await;
    assert_eq!((code, h["x-workbench-user"].to_str().unwrap()), (StatusCode::FORBIDDEN, "bob"));
    assert_eq!(h["x-workbench-groups"], "staff,workbench", "for the refusal's page");
    let (code, h, _) = send(authz_router(s.clone()), check("alice", "t3=1; _workbench_auth=alice")).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(h["x-workbench-cookie"], "t3=1", "the app never gets the SSO cookie");
    assert!(!h.contains_key("x-workbench-groups"), "a pass carries no groups");
    let (code, _, _) = send(authz_router(s.clone()), check("alice", "_workbench_auth=carol")).await;
    assert_eq!(code, StatusCode::OK, "admins reach every workspace");
    let mut other = check("alice", "_workbench_auth=alice");
    other.headers_mut().insert("x-workbench-app", "herdr".parse().unwrap());
    assert_eq!(send(authz_router(s.clone()), other).await.0, StatusCode::FORBIDDEN, "an app it hasn't");
    let (code, _, _) = send(authz_router(s), check("nobody", "_workbench_auth=carol")).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "a workspace that doesn't exist");

    let (_dir, fake) = self_serve(vec![made("alice-dev", "alice")], None);
    let s = state(fake);
    let (code, h, _) = send(authz_router(s.clone()), check("alice-dev", "_workbench_auth=alice:workbench")).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(h["x-workbench-upstream"], "10.232.0.20:3773", "nginx proxies to what the config says");
    let (code, _, _) = send(authz_router(s.clone()), check("alice-dev", "_workbench_auth=alice")).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "a workspace made at run time needs its owner in the group");
    let (code, _, _) = send(authz_router(s), check("alice", "_workbench_auth=alice")).await;
    assert_eq!(code, StatusCode::OK, "a declared one doesn't");
}

/// A host that lets people make workspaces, its files in a directory of
/// the test's.
fn self_serve(made: Vec<Workspace>, outcome: Option<Outcome>) -> (tempfile::TempDir, Fake) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake {
        made,
        paths: Some(Paths(dir.path().to_owned())),
        outcome: Arc::new(Mutex::new(outcome)),
        ..Fake::default()
    };
    (dir, fake)
}

const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMbJhk5H0h7O alice@laptop";

#[tokio::test]
async fn people_in_the_group_make_their_own() {
    let _turn = TURN.lock().await;
    let (dir, fake) = self_serve(vec![], Some(Outcome { ok: true, ..Outcome::default() }));
    let units = fake.units.clone();
    let s = state(fake);
    let paths = Paths(dir.path().to_owned());

    let (code, _, _) = send(ui(&s), get("/new", "bob")).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "bob is in no group");
    let (code, _, body) = send(ui(&s), with_groups(get("/new", "alice"), "staff, workbench")).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert!(body.contains("New workspace") && body.contains("alice or alice-something"), "{body}");
    let (_, _, body) = send(ui(&s), with_groups(get("/", "alice"), "workbench")).await;
    assert!(body.contains(r#"href="/new""#), "the list offers it");

    let form =
        format!("name=alice-dev&os=ubuntu&memory_gib=8&disk_gib=64&apps%5B0%5D=t3code&ssh_keys={}", super::encode(KEY));
    let (code, _, _) = send(ui(&s), post("/api/create", "bob", ORIGIN, &form)).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, h, _) = send(ui(&s), with_groups(post("/api/create", "alice", ORIGIN, &form), "workbench")).await;
    assert_eq!((code, h[header::LOCATION].to_str().unwrap()), (StatusCode::SEE_OTHER, "/?w=alice-dev"));
    assert_eq!(*units.lock().unwrap(), ["workbench-create@alice-dev.service"]);
    let asked: Asked = serde_json::from_slice(&std::fs::read(paths.request("alice-dev")).unwrap()).unwrap();
    assert_eq!((asked.owner.as_str(), asked.ssh_authorized_keys.as_slice()), ("alice", [KEY.to_owned()].as_slice()));
    assert_eq!(asked.apps, ["t3code"]);
    assert!(body_has_apps(&s).await, "the form offers the host's apps");

    for (bad, says) in [
        ("name=bob-dev&os=ubuntu&memory_gib=8&disk_gib=64", "start%20with%20alice-"),
        ("name=alice-x&os=nixos&memory_gib=8&disk_gib=64", "isn%27t%20a%20system"),
        ("name=alice-x&os=ubuntu&memory_gib=99&disk_gib=64", "Memory%20must%20be"),
        ("name=alice&os=ubuntu&memory_gib=8&disk_gib=64", "is%20taken"),
        ("name=alice-x&os=ubuntu&memory_gib=8&disk_gib=64&apps%5B0%5D=nope", "isn%27t%20an%20app"),
    ] {
        let (code, h, _) = send(ui(&s), with_groups(post("/api/create", "alice", ORIGIN, bad), "workbench")).await;
        let to = h[header::LOCATION].to_str().unwrap();
        assert!(code == StatusCode::SEE_OTHER && to.starts_with("/new?error=") && to.contains(says), "{bad}: {to}");
    }
    assert_eq!(units.lock().unwrap().len(), 1, "nothing refused reaches the host");
}

async fn body_has_apps(s: &AppState) -> bool {
    let (_, _, body) = send(ui(s), with_groups(get("/new", "alice"), "workbench")).await;
    // What a new workspace has is checked; an app it opts into isn't.
    body.contains(r#"name="apps[0]" value="t3code" checked"#)
        && body.contains(r#"name="apps[1]" value="herdr">"#)
        && body.contains("Reached from your terminal")
}

/// A workspace made at run time changes its apps through the host's unit;
/// a declared one's are the Nix configuration's.
#[tokio::test]
async fn apps_of_a_workspace_made_at_run_time() {
    let _turn = TURN.lock().await;
    let (dir, fake) = self_serve(vec![made("alice-dev", "alice")], Some(Outcome { ok: true, ..Outcome::default() }));
    let (units, calls, outcome) = (fake.units.clone(), fake.controlled.clone(), fake.outcome.clone());
    let s = state(fake);
    let paths = Paths(dir.path().to_owned());
    let as_alice = |form: &str| with_groups(post("/api/apps", "alice", ORIGIN, form), "workbench");
    let (code, _, body) = send(ui(&s), with_groups(get("/w/alice-dev/apps", "alice"), "workbench")).await;
    assert_eq!(code, StatusCode::OK);
    assert!(body.contains("Apps in") && body.contains("Save and restart"), "{body}");
    let (code, _, _) = send(ui(&s), with_groups(get("/w/alice/apps", "alice"), "workbench")).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "a declared workspace's apps are Nix's");
    assert_eq!(send(ui(&s), as_alice("ws=alice&apps%5B0%5D=t3code")).await.0, StatusCode::FORBIDDEN);

    let (_, h, _) = send(ui(&s), as_alice("ws=alice-dev")).await;
    assert_eq!(h[header::LOCATION], "/?w=alice-dev");
    let asked: AppsRequest = serde_json::from_slice(&std::fs::read(paths.apps_request("alice-dev")).unwrap()).unwrap();
    assert!(asked.apps.is_empty(), "none at all");
    assert!(calls.lock().unwrap().is_empty(), "saved for the next start");
    let (_, h, _) = send(ui(&s), as_alice("ws=alice-dev&apps%5B0%5D=t3code&restart=true")).await;
    assert_eq!(h[header::LOCATION], "/?w=alice-dev");
    assert_eq!(*calls.lock().unwrap(), [(Verb::Restart, "alice-dev".to_owned())], "and now");
    assert_eq!(*units.lock().unwrap(), ["workbench-apps@alice-dev.service", "workbench-apps@alice-dev.service"]);
    let (_, h, _) = send(ui(&s), as_alice("ws=alice-dev&apps%5B0%5D=nope")).await;
    assert!(h[header::LOCATION].to_str().unwrap().starts_with("/w/alice-dev/apps?error="), "{h:?}");
    assert_eq!(units.lock().unwrap().len(), 2, "nothing refused reaches the host");

    // One at a time: another would join the run, which reads one request.
    s.making.lock().unwrap().insert("alice-dev/apps".into());
    let (_, h, _) = send(ui(&s), as_alice("ws=alice-dev")).await;
    assert!(h[header::LOCATION].to_str().unwrap().contains("being%20changed"), "{h:?}");
    s.making.lock().unwrap().clear();
    assert_eq!(units.lock().unwrap().len(), 2);

    // An outcome from before the request says nothing of it.
    let stale = paths.apps_outcome("alice-dev");
    std::fs::write(&stale, br#"{"ok":false,"error":"an old refusal"}"#).unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    std::fs::File::options().write(true).open(&stale).unwrap().set_modified(old).unwrap();
    *outcome.lock().unwrap() = None;
    let (_, h, _) = send(ui(&s), as_alice("ws=alice-dev")).await;
    let to = h[header::LOCATION].to_str().unwrap();
    assert!(to.contains("said%20nothing") && !to.contains("old%20refusal"), "{to}");
}

/// An admin changes how far a workspace made at run time grows, through
/// the host's unit; its owner doesn't, and a declared one's is Nix's.
#[tokio::test]
async fn admins_change_a_workspaces_memory() {
    let _turn = TURN.lock().await;
    let sized = Workspace { boot_memory_bytes: 2 << 30, max_memory_bytes: 8 << 30, ..made("alice-dev", "alice") };
    let (dir, fake) = self_serve(vec![sized], Some(Outcome { ok: true, ..Outcome::default() }));
    let (units, calls) = (fake.units.clone(), fake.controlled.clone());
    let s = state(fake);
    let paths = Paths(dir.path().to_owned());
    let (page, dialog) = (r#"href="/w/alice-dev/memory""#, r#"<dialog id="memory-alice-dev""#);

    let (_, _, panel) = send(ui(&s), with_groups(get("/?w=alice-dev", "alice"), "workbench")).await;
    assert!(panel.contains("8 GiB") && !panel.contains(page), "the owner sees it, not the way to change it");
    assert!(!panel.contains(dialog), "{panel}");
    let (code, _, _) = send(ui(&s), with_groups(get("/w/alice-dev/memory", "alice"), "workbench")).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let form = "ws=alice-dev&memory_gib=16";
    let (code, _, _) = send(ui(&s), with_groups(post("/api/memory", "alice", ORIGIN, form), "workbench")).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "for admins");

    let (_, _, panel) = send(ui(&s), get("/?w=alice-dev", "carol")).await;
    assert!(panel.contains(page) && panel.contains("Edit…"), "{panel}");
    assert!(panel.contains(dialog) && panel.contains(r#"value="8""#), "in place, as the apps are: {panel}");
    let (code, _, body) = send(ui(&s), get("/w/alice-dev/memory", "carol")).await;
    assert_eq!(code, StatusCode::OK);
    assert!(body.contains("Memory of") && body.contains(r#"value="8""#) && body.contains("Save and restart"), "{body}");
    let as_carol = |form: &str| post("/api/memory", "carol", ORIGIN, form);
    let (_, h, _) = send(ui(&s), as_carol(form)).await;
    assert_eq!(h[header::LOCATION], "/?w=alice-dev");
    let asked: MemoryRequest =
        serde_json::from_slice(&std::fs::read(paths.memory_request("alice-dev")).unwrap()).unwrap();
    assert_eq!(asked.max_memory, Ceiling::Size(16));
    assert!(calls.lock().unwrap().is_empty(), "saved for the next start");
    let (_, h, _) = send(ui(&s), as_carol("ws=alice-dev&memory_gib=12&restart=true")).await;
    assert_eq!(h[header::LOCATION], "/?w=alice-dev");
    assert_eq!(*calls.lock().unwrap(), [(Verb::Restart, "alice-dev".to_owned())], "and now");
    assert_eq!(*units.lock().unwrap(), ["workbench-memory@alice-dev.service", "workbench-memory@alice-dev.service"]);

    for (form, why) in [
        ("ws=alice-dev&memory_gib=17", "Memory%20must"),
        ("ws=alice-dev&memory_gib=x", "whole%20number"),
        ("ws=alice-dev&memory_gib=auto", "no%20auto"),
    ] {
        let (code, h, _) = send(ui(&s), as_carol(form)).await;
        assert_eq!(code, StatusCode::SEE_OTHER);
        let to = h[header::LOCATION].to_str().unwrap();
        assert!(to.starts_with("/w/alice-dev/memory?error=") && to.contains(why), "{to}");
    }
    let (code, _, _) = send(ui(&s), as_carol("ws=alice&memory_gib=8")).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "a declared workspace's memory is Nix's");
    assert_eq!(units.lock().unwrap().len(), 2, "nothing refused reaches the host");
}

/// Where the host's most is its memory, a workspace may follow it: new
/// ones do unless their owner picks a size, and an admin sets it.
#[tokio::test]
async fn auto_where_the_host_offers_it() {
    let _turn = TURN.lock().await;
    let following = Workspace {
        boot_memory_bytes: 2 << 30,
        max_memory_bytes: 64 << 30,
        memory_auto: true,
        ..made("alice-dev", "alice")
    };
    let (dir, fake) = self_serve(vec![following], Some(Outcome { ok: true, ..Outcome::default() }));
    let s = state_with(fake, |l| {
        l.max_memory = Ceiling::Auto;
        l.host_memory_gib = 64;
        l.quota = 2;
    });
    let paths = Paths(dir.path().to_owned());

    let (_, _, body) = send(ui(&s), with_groups(get("/new", "alice"), "workbench")).await;
    assert!(body.contains(r#"pattern="auto|[0-9]+""#) && body.contains("2 to 64, or auto"), "{body}");
    assert!(body.contains(r#"name="memory_gib" type="text""#) && body.contains(r#"value="auto""#), "auto by default");
    let form = "name=alice-two&os=ubuntu&memory_gib=auto&disk_gib=64";
    let (_, h, _) = send(ui(&s), with_groups(post("/api/create", "alice", ORIGIN, form), "workbench")).await;
    assert_eq!(h[header::LOCATION], "/?w=alice-two");
    let asked: Asked = serde_json::from_slice(&std::fs::read(paths.request("alice-two")).unwrap()).unwrap();
    assert_eq!(asked.max_memory, Ceiling::Auto);

    let (_, _, panel) = send(ui(&s), get("/?w=alice-dev", "carol")).await;
    assert!(panel.contains("of 64 GiB, its node's"), "{panel}");
    let (_, _, body) = send(ui(&s), get("/w/alice-dev/memory", "carol")).await;
    assert!(body.contains(r#"value="auto""#), "{body}");
    let (_, h, _) = send(ui(&s), post("/api/memory", "carol", ORIGIN, "ws=alice-dev&memory_gib=24")).await;
    assert_eq!(h[header::LOCATION], "/?w=alice-dev");
    let asked: MemoryRequest =
        serde_json::from_slice(&std::fs::read(paths.memory_request("alice-dev")).unwrap()).unwrap();
    assert_eq!(asked.max_memory, Ceiling::Size(24));
    let (_, h, _) = send(ui(&s), post("/api/memory", "carol", ORIGIN, "ws=alice-dev&memory_gib=auto")).await;
    assert_eq!(h[header::LOCATION], "/?w=alice-dev");
    let asked: MemoryRequest =
        serde_json::from_slice(&std::fs::read(paths.memory_request("alice-dev")).unwrap()).unwrap();
    assert_eq!(asked.max_memory, Ceiling::Auto);
}

#[tokio::test]
async fn the_quota_and_the_hosts_refusals() {
    let _turn = TURN.lock().await;
    let (dir, fake) = self_serve(vec![made("alice-dev", "alice")], Some(Outcome { ok: true, ..Outcome::default() }));
    let outcome = fake.outcome.clone();
    let s = state(fake);
    let form = "name=alice-two&os=ubuntu&memory_gib=8&disk_gib=64";
    let (_, h, _) = send(ui(&s), with_groups(post("/api/create", "alice", ORIGIN, form), "workbench")).await;
    assert!(h[header::LOCATION].to_str().unwrap().contains("quota%20allows%20%281%29"), "{h:?}");

    // An admin raises it; only an admin may.
    let (code, _, _) =
        send(ui(&s), with_groups(post("/api/quota", "alice", ORIGIN, "user=alice&quota=3"), "workbench")).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, h, _) =
        send(ui(&s), with_groups(post("/api/quota", "dave", ORIGIN, "user=alice&quota=3"), "workbench-admins")).await;
    assert_eq!((code, h[header::LOCATION].to_str().unwrap()), (StatusCode::SEE_OTHER, "/host"), "an admin by group");
    let quotas: std::collections::BTreeMap<String, u32> =
        serde_json::from_slice(&std::fs::read(Paths(dir.path().to_owned()).quotas()).unwrap()).unwrap();
    assert_eq!(quotas["alice"], 3);
    let (_, _, body) = send(ui(&s), get("/host", "carol")).await;
    assert!(body.contains("Quotas") && body.contains(r#"value="3""#), "{body}");
    let (_, h, _) = send(ui(&s), with_groups(post("/api/create", "alice", ORIGIN, form), "workbench")).await;
    assert_eq!(h[header::LOCATION], "/?w=alice-two");

    // What the host refuses, such as a missing credential, is said too.
    let refusal = "Your workspace has no credential for its app yet. Ask an admin.";
    *outcome.lock().unwrap() = Some(Outcome { ok: false, error: refusal.into(), ..Outcome::default() });
    let (_, h, _) = send(
        ui(&s),
        with_groups(post("/api/create", "alice", ORIGIN, form.replace("two", "three").as_str()), "workbench"),
    )
    .await;
    let to = h[header::LOCATION].to_str().unwrap().to_owned();
    assert!(to.contains("no%20credential"), "{to}");
    let (_, _, body) = send(ui(&s), with_groups(get(&to, "alice"), "workbench")).await;
    assert!(body.contains(refusal), "{body}");

    // A name being made for one request isn't written over by another.
    s.making.lock().unwrap().insert("alice-four".into());
    let four = form.replace("two", "four");
    let (_, h, _) = send(ui(&s), with_groups(post("/api/create", "alice", ORIGIN, &four), "workbench")).await;
    assert!(h[header::LOCATION].to_str().unwrap().contains("being%20made"), "{h:?}");
}

#[tokio::test]
async fn only_workspaces_made_at_run_time_are_destroyed_here() {
    let _turn = TURN.lock().await;
    let (_dir, fake) = self_serve(vec![made("alice-dev", "alice")], None);
    let units = fake.units.clone();
    let s = state(fake);
    let as_alice = |form: &str| with_groups(post("/api/destroy", "alice", ORIGIN, form), "workbench");
    let (_, _, body) = send(ui(&s), with_groups(get("/w/alice-dev/destroy", "alice"), "workbench")).await;
    assert!(body.contains("Type") && body.contains("to confirm"), "{body}");
    let (code, _, _) = send(ui(&s), with_groups(get("/w/alice/destroy", "alice"), "workbench")).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "a declared workspace has no Destroy page");
    let (_, h, _) = send(ui(&s), as_alice("ws=alice-dev&confirm=alice")).await;
    assert_eq!(h[header::LOCATION], "/w/alice-dev/destroy?error=confirm");
    let (code, _, _) = send(ui(&s), as_alice("ws=alice&confirm=alice")).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, _, _) =
        send(ui(&s), with_groups(post("/api/destroy", "bob", ORIGIN, "ws=alice-dev&confirm=alice-dev"), "workbench"))
            .await;
    assert_eq!(code, StatusCode::FORBIDDEN, "not bob's");
    let (_, h, _) = send(ui(&s), as_alice("ws=alice-dev&confirm=alice-dev")).await;
    assert_eq!(h[header::LOCATION], "/");
    assert_eq!(*units.lock().unwrap(), ["workbench-destroy@alice-dev.service"]);
}

#[tokio::test]
async fn the_denied_page_is_styled_from_the_ui() {
    let _turn = TURN.lock().await;
    let s = state(Fake::default());
    let (code, headers, body) = send(ui(&s), get("/denied?ws=alice&app=t3code", "bob")).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert!(body.contains("alice.ws.test") && body.contains("bob") && body.contains("https://ws.test/assets/app.css"));
    let (code, _, body) = send(ui(&s), get("/denied?ws=alice&app=herdr", "alice")).await;
    assert_eq!(code, StatusCode::NOT_FOUND, "her own workspace hasn't that app");
    assert!(body.contains("has no app") && body.contains("herdr"), "{body}");
    assert!(headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap().starts_with("default-src 'none'"));
    let (_, _, body) = send(ui(&s), get("/denied?ws=%3Cscript%3E", "bob")).await;
    assert!(!body.contains("<script>"));
}

/// An app without an address has nothing to reach through nginx, and no
/// Open: it is reached from a terminal.
#[tokio::test]
async fn apps_without_an_address() {
    let _turn = TURN.lock().await;
    let s = state(Fake { herdr: true, ..Fake::default() });
    let check = Request::get("/authz")
        .header("x-workbench-workspace", "alice")
        .header("x-workbench-app", "herdr")
        .header(header::COOKIE, "_workbench_auth=alice")
        .body(Body::empty())
        .unwrap();
    let (code, h, _) = send(authz_router(s.clone()), check).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "as for an app it hasn't");
    assert!(!h.contains_key("x-workbench-upstream"));
    let (code, _, body) = send(ui(&s), get("/denied?ws=alice&app=herdr", "alice")).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert!(body.contains("has no address for") && body.contains("from your terminal"), "{body}");
    let (_, h, _) = send(ui(&s), post("/api/open", "alice", ORIGIN, "ws=alice&app=herdr")).await;
    assert_eq!(h[header::LOCATION], "/w/alice/open/herdr");
    let (code, _, body) = send(ui(&s), get("/w/alice/open/herdr", "alice")).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(body.contains("no address to open in a browser"), "{body}");
    let (_, _, body) = send(ui(&s), get("/?w=alice", "alice")).await;
    assert!(body.contains(r#"href="/w/alice/connect/herdr""#) && body.contains("Connect to Herdr…"), "{body}");
    assert!(!body.contains("Open Herdr"), "{body}");
}

/// The Connect page of an app without an address sets a computer up with
/// a fresh code and says how to reach the app; the person's devices are
/// listed there and on their page, and they revoke them, theirs only.
#[tokio::test]
async fn connect_and_devices() {
    let _turn = TURN.lock().await;
    let s = state(Fake { herdr: true, ..Fake::default() });
    let (code, h, body) = send(ui(&s), with_groups(get("/w/alice/connect/herdr", "alice"), "staff")).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(h[header::CACHE_CONTROL], "no-store", "the code is the viewer's");
    assert_eq!(h[header::REFERRER_POLICY], "same-origin", "the UI's forms keep their origin");
    let install = "curl -fsSL https://ws.test/connect/install.sh | sh -s -- wbp-";
    assert!(body.contains(install) && body.contains("<code>herdr --remote alice.ws.test</code>"), "{body}");
    assert!(body.contains("No computer is set up yet"), "{body}");
    let (set_up, reach) = (body.find("Set this computer up").unwrap(), body.find("Reach Herdr").unwrap());
    assert!(set_up < reach, "setting up first: {body}");
    let code_of = |body: &str| body.split("sh -s -- ").nth(1).unwrap()[..36].to_owned();
    let (_, _, again) = send(ui(&s), get("/w/alice/connect/herdr", "alice")).await;
    assert_ne!(code_of(&body), code_of(&again), "a new code every time");
    let (_, device) = s.devices.pair(&code_of(&body), "laptop").unwrap();
    assert_eq!(device.groups, ["staff"], "the groups single sign-on gave her");
    assert_eq!(s.devices.pair(&code_of(&body), "again"), Err(connect::PairError::Code));
    let (_, _, paired) = send(ui(&s), get("/w/alice/connect/herdr", "alice")).await;
    let (reach, more) = (paired.find("Reach Herdr").unwrap(), paired.find("Another computer?").unwrap());
    assert!(reach < more && paired.contains("<b>laptop</b>"), "reaching first, once set up: {paired}");
    let (code, _, _) = send(ui(&s), get("/w/alice/connect/herdr", "bob")).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "not bob's");
    let (code, _, _) = send(ui(&s), get("/w/alice/connect/t3code", "alice")).await;
    assert_eq!(code, StatusCode::NOT_FOUND, "T3 Code opens in the browser");

    let (code, _, body) = send(ui(&s), get("/devices", "alice")).await;
    assert_eq!(code, StatusCode::OK);
    assert!(body.contains("<b>laptop</b>") && body.contains("never used"), "{body}");
    let (_, _, body) = send(ui(&s), get("/devices", "bob")).await;
    assert!(body.contains("No computer is set up yet"), "bob has none: {body}");
    let revoke = format!("id={}&back=%2Fw%2Falice%2Fconnect%2Fherdr", device.id);
    let (code, h, _) = send(ui(&s), post("/api/revoke-device", "bob", ORIGIN, &revoke)).await;
    assert_eq!((code, h[header::LOCATION].to_str().unwrap()), (StatusCode::SEE_OTHER, "/w/alice/connect/herdr"));
    assert_eq!(s.devices.of("alice").len(), 1, "not bob's to revoke");
    let (code, _, _) = send(ui(&s), post("/api/revoke-device", "alice", "https://alice.ws.test", &revoke)).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "from the UI's origin only");
    assert_eq!(s.devices.of("alice").len(), 1);
    let (code, h, _) = send(ui(&s), post("/api/revoke-device", "alice", ORIGIN, &revoke)).await;
    assert_eq!((code, h[header::LOCATION].to_str().unwrap()), (StatusCode::SEE_OTHER, "/w/alice/connect/herdr"));
    assert!(s.devices.of("alice").is_empty(), "revoked");
    let (code, h, _) = send(ui(&s), post("/api/revoke-device", "alice", ORIGIN, &revoke)).await;
    assert_eq!(
        (code, h[header::LOCATION].to_str().unwrap()),
        (StatusCode::SEE_OTHER, "/w/alice/connect/herdr"),
        "twice"
    );

    // Back only to a path of the UI's: browsers read `/\host` as `//host`.
    let (_, device) = s.devices.pair(&s.devices.make_code("alice", &[]).unwrap(), "phone").unwrap();
    for back in ["%2F%5Cevil.example", "%2F%09%2Fevil.example", "%2F%0A%2Fevil.example"] {
        let revoke = format!("id={}&back={back}", device.id);
        let (code, h, _) = send(ui(&s), post("/api/revoke-device", "alice", ORIGIN, &revoke)).await;
        assert_eq!((code, h[header::LOCATION].to_str().unwrap()), (StatusCode::SEE_OTHER, "/devices"), "{back}");
    }

    // Any visit to the panel says her groups again, for her devices.
    s.devices.pair(&s.devices.make_code("alice", &["staff".into()]).unwrap(), "desk").unwrap();
    let (code, _, _) = send(ui(&s), with_groups(get("/", "alice"), "admins")).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(s.devices.of("alice")[0].groups, ["admins"], "as single sign-on says them now");
}

/// A Connect page makes a code only for the workspace's owner, whose
/// certificates its SSH server takes, for an app there is to reach, and
/// asked by the UI or by a person following a link: never in the
/// background of a workspace's page.
#[tokio::test]
async fn connect_codes_for_the_owner_only() {
    let _turn = TURN.lock().await;
    let s = state(Fake { herdr: true, ..Fake::default() });
    let kept = s.devices.make_code("alice", &[]).unwrap();

    let (code, _, _) = send(ui(&s), get("/w/alice/connect/herdr", "carol")).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "SSH to alice's workspace signs in as alice, not as an admin");
    let (_, _, panel) = send(ui(&s), get("/?w=alice", "carol")).await;
    assert!(!panel.contains(r#"href="/w/alice/connect/herdr""#) && panel.contains("from alice"), "{panel}");

    for path in ["/w/alice/connect/t3code", "/w/alice/connect/nope", "/api/connect?ws=alice&app=t3code"] {
        let (code, _, body) = send(ui(&s), get(path, "alice")).await;
        assert_eq!(code, StatusCode::NOT_FOUND, "{path}: {body}");
    }

    let asked = |path: &str, site: &str, mode: &str, dest: &str| {
        let mut req = get(path, "alice");
        for (k, v) in [("sec-fetch-site", site), ("sec-fetch-mode", mode), ("sec-fetch-dest", dest)] {
            req.headers_mut().insert(k, v.parse().unwrap());
        }
        req
    };
    let api = "/api/connect?ws=alice&app=herdr";
    for (req, why) in [
        (asked(api, "same-site", "cors", "empty"), "a workspace's script"),
        (asked("/w/alice/connect/herdr", "same-site", "no-cors", "image"), "a workspace's image"),
        (asked("/w/alice/connect/herdr", "same-site", "navigate", "iframe"), "a workspace's frame"),
    ] {
        assert_eq!(send(ui(&s), req).await.0, StatusCode::FORBIDDEN, "{why}");
    }
    for (req, why) in [
        (asked("/w/alice/connect/herdr", "same-site", "navigate", "document"), "a link from a workspace"),
        (asked(api, "same-origin", "cors", "empty"), "the UI's own page"),
    ] {
        let (code, _, body) = send(ui(&s), req).await;
        assert_eq!(code, StatusCode::OK, "{why}: {body}");
    }
    assert!(s.devices.pair(&kept, "laptop").is_ok(), "none of the refused made a code that pushed it out");
}

fn json_post(path: &str, bearer: Option<&str>, body: &serde_json::Value) -> Request<Body> {
    let mut req = Request::post(path).header(header::CONTENT_TYPE, "application/json");
    if let Some(b) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {b}"));
    }
    req.body(Body::from(body.to_string())).unwrap()
}

/// A computer pairs with a code its person made, once, and gets SSH
/// certificates in their name while its credential works.
#[tokio::test]
async fn pairing_and_certificates() {
    let _turn = TURN.lock().await;
    let fake = Fake::default();
    let signed = fake.signed.clone();
    let s = state(fake);
    let code = s.devices.make_code("alice", &["workbench".into()]).unwrap();
    let pair = |code: &str| json_post("/connect/pair", None, &serde_json::json!({"code": code, "label": "laptop"}));
    let (status, h, body) = send(connect_router(s.clone()), pair(&code)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(h[header::CACHE_CONTROL], "no-store");
    let paired: workbench_model::connect::Paired = serde_json::from_str(&body).unwrap();
    assert_eq!((paired.user.as_str(), paired.domain.as_str()), ("alice", "ws.test"));
    assert!(paired.host_ca.starts_with("ssh-ed25519 ") && paired.credential.starts_with("wbc-"), "{paired:?}");
    let (status, _, body) = send(connect_router(s.clone()), pair(&code)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a code works once");
    assert!(body.contains("doesn't work"), "{body}");
    let (status, _, _) = send(connect_router(s.clone()), pair("wbp-0123")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMbJhk5H0h7O laptop";
    let cert =
        |bearer: Option<&str>, key: &str| json_post("/connect/cert", bearer, &serde_json::json!({"publicKey": key}));
    let (status, _, _) = send(connect_router(s.clone()), cert(None, key)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no credential");
    let (status, _, _) = send(connect_router(s.clone()), cert(Some("wbc-nope"), key)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "not a credential");
    let (status, _, body) = send(connect_router(s.clone()), cert(Some(&paired.credential), key)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let c: workbench_model::connect::Cert = serde_json::from_str(&body).unwrap();
    assert_eq!(c.certificate, "ssh-ed25519-cert-v01@openssh.com AAAAcert-of-alice");
    assert_eq!(*signed.lock().unwrap(), [("alice".to_owned(), key.to_owned())], "in her name");
    let (status, _, _) = send(connect_router(s.clone()), cert(Some(&paired.credential), "ssh-ed25519 short")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "not a key");
    let two = format!("{key}\n{key}");
    let (status, _, _) = send(connect_router(s.clone()), cert(Some(&paired.credential), &two)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "one key");

    let id = s.devices.of("alice")[0].id.clone();
    s.devices.revoke("alice", &id).unwrap();
    let (status, _, body) = send(connect_router(s), cert(Some(&paired.credential), key)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a revoked credential");
    assert!(body.contains("revoked"), "{body}");
}

/// The tunnel to a workspace's SSH server lets a device in by its
/// person, by the owner rule of the access check, while the VM runs.
#[tokio::test(flavor = "multi_thread")]
async fn the_ssh_tunnel() {
    use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};
    let _turn = TURN.lock().await;
    // The guest's SSH server, which says back what it gets.
    let sshd = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ssh = sshd.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((mut conn, _)) = sshd.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = conn.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    let s = state(Fake { ssh, ..Fake::default() });
    let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(axum::serve(server, connect_router(s.clone())).into_future());
    let paired = |user: &str| s.devices.pair(&s.devices.make_code(user, &[]).unwrap(), "laptop").unwrap().0;
    let (alice, bob, carol) = (paired("alice"), paired("bob"), paired("carol"));
    let dial = |ws: &str, credential: Option<&str>| {
        let mut req = format!("ws://{addr}/connect/ssh/{ws}").into_client_request().unwrap();
        if let Some(c) = credential {
            req.headers_mut().insert(header::AUTHORIZATION, format!("Bearer {c}").parse().unwrap());
        }
        tokio_tungstenite::connect_async(req)
    };
    let refused = |r: Result<_, tungstenite::Error>| match r {
        Err(tungstenite::Error::Http(res)) => res.status().as_u16(),
        Err(e) => panic!("{e}"),
        Ok(_) => 101,
    };
    let (mut ws, _) = dial("alice", Some(&alice)).await.unwrap();
    ws.send(tungstenite::Message::Binary(b"SSH-2.0-test\r\n".to_vec().into())).await.unwrap();
    let back = ws.next().await.unwrap().unwrap();
    assert_eq!(back.into_data().as_ref(), b"SSH-2.0-test\r\n", "through to the SSH server and back");
    ws.close(None).await.unwrap();
    assert_eq!(refused(dial("alice", None).await), 401, "no credential");
    assert_eq!(refused(dial("alice", Some(&bob)).await), 403, "another person's credential");
    assert_eq!(refused(dial("nobody", Some(&alice)).await), 403, "a workspace that doesn't exist, alike");
    assert_eq!(refused(dial("bob", Some(&bob)).await), 409, "a VM that doesn't run");
    assert_eq!(refused(dial("alice", Some(&carol)).await), 101, "an admin, whom the SSH server then asks for a name");
    let id = s.devices.of("alice")[0].id.clone();
    assert!(connect::still_reaches(&s, &id, "alice"), "an open tunnel stays");
    assert!(!connect::still_reaches(&s, &id, "bob"), "not to another's");
    s.devices.revoke("alice", &id).unwrap();
    assert!(!connect::still_reaches(&s, &id, "alice"), "nor once its device is revoked");
    assert_eq!(refused(dial("alice", Some(&alice)).await), 401, "a revoked credential");
}

#[test]
fn encodes_query_values() {
    assert_eq!(encode("exit status 2\nboom & co"), "exit%20status%202%0Aboom%20%26%20co");
}

/// Pages rendered at once on one runtime, as the server does.
#[tokio::test(flavor = "multi_thread")]
async fn pages_render_concurrently() {
    let _turn = TURN.lock().await;
    let s = state(Fake::default());
    let router = ui(&s);
    let mut all = tokio::task::JoinSet::new();
    for i in 0..50 {
        let (router, path) = (router.clone(), ["/", "/?w=bob", "/host", "/w/bob/stop"][i % 4]);
        all.spawn(async move { send(router, get(path, "carol")).await.0 });
    }
    let codes = tokio::time::timeout(std::time::Duration::from_secs(20), all.join_all()).await.expect("pages hung");
    assert!(codes.iter().all(|c| *c == StatusCode::OK), "{codes:?}");
}

#[test]
fn assets_carry_the_build_version() {
    let o = LeptosOptions::builder().output_name("workbench-web").site_root("x").site_pkg_dir("pkg/abc").build();
    assert_eq!(crate::app::asset(&o, "app.css"), "/assets/app.css?v=abc");
    let (a, b) = (
        super::asset_version(std::path::Path::new("/nix/store/a-site")),
        super::asset_version(std::path::Path::new("/nix/store/b-site")),
    );
    assert!(a.len() == 12 && a != b, "{a} {b}");
}

mod front;
