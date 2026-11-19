//! The UI's handlers against a fake host: who sees and controls what, the
//! access check, the origin rule, and the pages the server renders.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;
use workbench::config::Config;
use workbench::self_service::Paths;
use workbench::state::SignInError;
use workbench_model::self_service::{Limits, Outcome, Request as Asked};
use workbench_model::{State, Verb, Workspace};

use super::*;

const ORIGIN: &str = "https://ws.test";

#[derive(Default)]
struct Fake {
    controlled: Arc<Mutex<Vec<(Verb, String)>>>,
    sign_in: Mutex<Option<Result<String, SignInError>>>,
    /// Workspaces made at run time, besides the declared ones.
    made: Vec<Workspace>,
    /// Where the units write how they went, and what they say.
    paths: Option<Paths>,
    outcome: Arc<Mutex<Option<Outcome>>>,
    units: Arc<Mutex<Vec<String>>>,
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
        sign_in: true,
        unit: "active".into(),
        vm: "Running".into(),
        ..Workspace::default()
    }
}

impl Backend for Fake {
    fn list(&self) -> BoxFuture<'_, anyhow::Result<Vec<Workspace>>> {
        let mut all = vec![
            ws("alice", "alice", State::Running),
            ws("bob", "bob", State::Stopped),
            ws("bobby", "bob", State::Stopping),
        ];
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
                app_port: 3773,
                self_service: true,
                ..Config::default()
            });
        }
        let owner = match name {
            "alice" => "alice",
            "bob" | "bobby" => "bob",
            _ => anyhow::bail!("no workspace {name:?}"),
        };
        Ok(Config {
            name: name.into(),
            owner: owner.into(),
            url: format!("https://{name}.ws.test"),
            ..Config::default()
        })
    }

    fn sign_in(&self, _: Config) -> BoxFuture<'_, Result<String, SignInError>> {
        let answer = self.sign_in.lock().unwrap().clone().unwrap_or(Ok("https://alice.ws.test/pair#token=t".into()));
        Box::pin(async move { answer })
    }

    /// The cookie is `<user>` or `<user>:<group>,<group>`.
    fn whoami<'a>(&'a self, cookie: &'a str) -> BoxFuture<'a, anyhow::Result<Identity>> {
        let value =
            cookie.split(';').map(str::trim).find_map(|c| c.strip_prefix("_workbench_auth=")).unwrap_or_default();
        let (user, groups) = value.split_once(':').unwrap_or((value, ""));
        let who = Identity { user: user.to_owned(), groups: authz::groups(groups) };
        Box::pin(async move { Ok(who) })
    }

    fn run_unit<'a>(&'a self, unit: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
        self.units.lock().unwrap().push(unit.to_owned());
        let name = unit.split_once('@').map(|(_, r)| r.trim_end_matches(".service")).unwrap_or_default();
        let outcome = self.outcome.lock().unwrap().clone();
        if let (Some(paths), Some(o)) = (&self.paths, &outcome) {
            let out = paths.outcome(name);
            std::fs::create_dir_all(out.parent().unwrap()).unwrap();
            std::fs::write(out, serde_json::to_vec(o).unwrap()).unwrap();
        }
        Box::pin(async move {
            anyhow::ensure!(outcome.is_none_or(|o| o.ok), "the unit failed");
            Ok(())
        })
    }
}

fn state(fake: Fake) -> AppState {
    let self_service = fake.paths.clone().map(|paths| SelfService {
        paths,
        limits: Limits {
            os: vec!["ubuntu".into()],
            boot_mem_mib: 2048,
            max_memory_gib: 16,
            max_disk_gib: 128,
            quota: 1,
        },
    });
    let opts = Options {
        origin: ORIGIN.into(),
        admins: vec!["carol".into()],
        admin_groups: vec!["workbench-admins".into()],
        creator_groups: vec!["workbench".into()],
        self_service,
        cookie_name: "_workbench_auth".into(),
        ..Options::default()
    };
    let leptos =
        LeptosOptions::builder().output_name("workbench-web").site_root("target/site").site_pkg_dir("pkg").build();
    AppState::new(opts, Box::new(fake), leptos)
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
async fn sign_in_sends_the_browser_to_the_apps_link() {
    let _turn = TURN.lock().await;
    let s = state(Fake::default());
    let (code, headers, _) = send(ui(&s), post("/api/sign-in", "alice", ORIGIN, "ws=alice")).await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "https://alice.ws.test/pair#token=t");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    let (code, _, _) = send(ui(&s), post("/api/sign-in", "bob", ORIGIN, "ws=alice")).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    // A stopped workspace has no link: its sign-in page says why.
    let (code, headers, _) = send(ui(&s), post("/api/sign-in", "bob", ORIGIN, "ws=bob")).await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/w/bob/sign-in");
    let (code, _, body) = send(ui(&s), get("/w/bob/sign-in", "bob")).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(body.contains("is stopped"), "{body}");

    let failing = Fake::default();
    *failing.sign_in.lock().unwrap() = Some(Err(SignInError::Failed { exit_code: 2, detail: "boom".into() }));
    let s = state(failing);
    let (_, headers, _) = send(ui(&s), post("/api/sign-in", "alice", ORIGIN, "ws=alice")).await;
    let to = headers[header::LOCATION].to_str().unwrap().to_owned();
    assert!(to.starts_with("/w/alice/sign-in?error=failed&detail=exit%20status%202"), "{to}");
    let (code, _, body) = send(ui(&s), get(&to, "alice")).await;
    assert_eq!(code, StatusCode::BAD_GATEWAY);
    assert!(body.contains("couldn't make a sign-in link") && body.contains("boom"), "{body}");

    // Only a NixOS workspace is told about its slower first boot.
    let (_, _, body) = send(ui(&s), get("/w/alice/sign-in?error=starting", "alice")).await;
    assert!(body.contains("is still starting") && !body.contains("NixOS"), "{body}");
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
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
    };
    let (code, _, _) = send(authz_router(s.clone()), check("alice", "t3=1")).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED, "nobody signed in");
    let (code, h, _) = send(authz_router(s.clone()), check("alice", "_workbench_auth=bob; t3=1")).await;
    assert_eq!((code, h["x-workbench-user"].to_str().unwrap()), (StatusCode::FORBIDDEN, "bob"));
    let (code, h, _) = send(authz_router(s.clone()), check("alice", "t3=1; _workbench_auth=alice")).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(h["x-workbench-cookie"], "t3=1", "the app never gets the SSO cookie");
    let (code, _, _) = send(authz_router(s.clone()), check("alice", "_workbench_auth=carol")).await;
    assert_eq!(code, StatusCode::OK, "admins reach every workspace");
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
    let (dir, fake) = self_serve(vec![], Some(Outcome { ok: true, error: String::new() }));
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

    let form = format!("name=alice-dev&os=ubuntu&memory_gib=8&disk_gib=64&ssh_keys={}", super::encode(KEY));
    let (code, _, _) = send(ui(&s), post("/api/create", "bob", ORIGIN, &form)).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, h, _) = send(ui(&s), with_groups(post("/api/create", "alice", ORIGIN, &form), "workbench")).await;
    assert_eq!((code, h[header::LOCATION].to_str().unwrap()), (StatusCode::SEE_OTHER, "/?w=alice-dev"));
    assert_eq!(*units.lock().unwrap(), ["workbench-create@alice-dev.service"]);
    let asked: Asked = serde_json::from_slice(&std::fs::read(paths.request("alice-dev")).unwrap()).unwrap();
    assert_eq!((asked.owner.as_str(), asked.ssh_authorized_keys.as_slice()), ("alice", [KEY.to_owned()].as_slice()));

    for (bad, says) in [
        ("name=bob-dev&os=ubuntu&memory_gib=8&disk_gib=64", "start%20with%20alice-"),
        ("name=alice-x&os=nixos&memory_gib=8&disk_gib=64", "isn%27t%20a%20system"),
        ("name=alice-x&os=ubuntu&memory_gib=99&disk_gib=64", "Memory%20must%20be"),
        ("name=alice&os=ubuntu&memory_gib=8&disk_gib=64", "is%20taken"),
    ] {
        let (code, h, _) = send(ui(&s), with_groups(post("/api/create", "alice", ORIGIN, bad), "workbench")).await;
        let to = h[header::LOCATION].to_str().unwrap();
        assert!(code == StatusCode::SEE_OTHER && to.starts_with("/new?error=") && to.contains(says), "{bad}: {to}");
    }
    assert_eq!(units.lock().unwrap().len(), 1, "nothing refused reaches the host");
}

#[tokio::test]
async fn the_quota_and_the_hosts_refusals() {
    let _turn = TURN.lock().await;
    let (dir, fake) = self_serve(vec![made("alice-dev", "alice")], Some(Outcome { ok: true, error: String::new() }));
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
    *outcome.lock().unwrap() = Some(Outcome { ok: false, error: refusal.into() });
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
    let (code, headers, body) = send(ui(&s), get("/denied?ws=alice", "bob")).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert!(body.contains("alice.ws.test") && body.contains("bob") && body.contains("https://ws.test/assets/app.css"));
    assert!(headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap().starts_with("default-src 'none'"));
    let (_, _, body) = send(ui(&s), get("/denied?ws=%3Cscript%3E", "bob")).await;
    assert!(!body.contains("<script>"));
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
