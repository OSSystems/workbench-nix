//! The UI on a front, against fake nodes: the list from the registry and
//! every node, the access check and the tunnel from the registry, picking a
//! node in New workspace, closing one, moves, and the consoles relayed.

use std::collections::BTreeMap;
use std::path::Path;

use axum::body::Bytes;
use tokio::sync::mpsc;
use workbench::config::SshCa;
use workbench::fleet::client::{BoxFuture as NodeFuture, CallError, DiskOut, Node};
use workbench::fleet::{Endpoint, FrontSelfService, FrontSettings, NodeEntry, VERSION};
use workbench_model::GIB;
use workbench_model::apps::Boot;
use workbench_model::fleet::{
    HostInfo, MakeRequest, Move, MoveRequest, NodeState, NodeWorkspace, OpenLinkAnswer, Phase, Record,
};
use workbench_model::self_service::NewView;

use super::*;

/// A node: what it has, or nothing while it doesn't answer; of this
/// build or another; at once, or after a while.
pub(super) struct FakeNode {
    has: Mutex<Option<Vec<NodeWorkspace>>>,
    other_build: std::sync::atomic::AtomicBool,
    /// How long it takes to say what it has.
    slow: Mutex<std::time::Duration>,
    /// What its control answers, when not done.
    refuses: Mutex<Option<CallError>>,
    controlled: Mutex<Vec<(Verb, String)>>,
    /// Its memory, in GiB.
    mem_gib: Mutex<i64>,
}

impl FakeNode {
    pub(super) fn new(has: Vec<NodeWorkspace>) -> Arc<Self> {
        Arc::new(FakeNode {
            has: Mutex::new(Some(has)),
            other_build: std::sync::atomic::AtomicBool::new(false),
            slow: Mutex::default(),
            refuses: Mutex::default(),
            controlled: Mutex::default(),
            mem_gib: Mutex::new(64),
        })
    }

    fn down(&self) {
        *self.has.lock().unwrap() = None;
    }

    fn up(&self, has: Vec<NodeWorkspace>) {
        *self.has.lock().unwrap() = Some(has);
    }

    fn of_another_build(&self) {
        self.other_build.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn answer<T: Send + 'static>(&self, f: impl FnOnce(&[NodeWorkspace]) -> T) -> Result<T, CallError> {
        if self.other_build.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(CallError::Version("another".into()));
        }
        match &*self.has.lock().unwrap() {
            Some(has) => Ok(f(has)),
            None => Err(CallError::Unreachable("connection refused".into())),
        }
    }

    /// An answer, once it takes the while it takes.
    fn after<'a, T: Send + 'a>(&self, answer: T) -> NodeFuture<'a, T> {
        let slow = *self.slow.lock().unwrap();
        Box::pin(async move {
            tokio::time::sleep(slow).await;
            answer
        })
    }
}

impl Node for FakeNode {
    fn host(&self) -> NodeFuture<'_, Result<HostInfo, CallError>> {
        let h = self.answer(|has| HostInfo {
            version: VERSION.into(),
            mem_total: *self.mem_gib.lock().unwrap() * GIB,
            mem_available: 40 * GIB,
            reserve: 6 * GIB,
            disk_total: 1 << 40,
            disk_free: 500 << 30,
            max_memory: has.iter().map(|w| w.workspace.max_memory_bytes).sum(),
            ..HostInfo::default()
        });
        self.after(h)
    }

    fn workspaces(&self) -> NodeFuture<'_, Result<Vec<NodeWorkspace>, CallError>> {
        let has = self.answer(<[NodeWorkspace]>::to_vec);
        self.after(has)
    }

    fn make<'a>(&'a self, _: &'a MakeRequest) -> NodeFuture<'a, Result<(), CallError>> {
        Box::pin(async { Ok(()) })
    }

    fn delete<'a>(&'a self, _: &'a str) -> NodeFuture<'a, Result<(), CallError>> {
        Box::pin(async { Ok(()) })
    }

    fn control<'a>(&'a self, name: &'a str, verb: Verb, _: bool) -> NodeFuture<'a, Result<(), CallError>> {
        let done = self.answer(|_| ()).and_then(|()| self.refuses.lock().unwrap().clone().map_or(Ok(()), Err));
        if done.is_ok() {
            self.controlled.lock().unwrap().push((verb, name.to_owned()));
        }
        Box::pin(async move { done })
    }

    fn open_link<'a>(&'a self, name: &'a str, app: &'a str) -> NodeFuture<'a, Result<OpenLinkAnswer, CallError>> {
        let link = format!("https://{name}--{app}.ws.test/pair#token=t");
        let a = self.answer(|_| OpenLinkAnswer::Link { link });
        Box::pin(async move { a })
    }

    fn disk_out<'a>(&'a self, _: &'a str) -> NodeFuture<'a, Result<DiskOut, CallError>> {
        Box::pin(async { Err(CallError::Refused { status: 409, error: "not in these tests".into() }) })
    }

    fn disk_in<'a>(&'a self, _: &'a str, _: mpsc::Receiver<Bytes>) -> NodeFuture<'a, Result<String, CallError>> {
        Box::pin(async { Err(CallError::Refused { status: 409, error: "not in these tests".into() }) })
    }
}

/// A workspace as its node has it.
fn on_node(name: &str, state: State) -> NodeWorkspace {
    let running = state != State::Stopped;
    let boot = Boot { packages: "/nix/store/x-packages".into(), apps: vec![t3code()] };
    NodeWorkspace {
        workspace: Workspace {
            name: name.into(),
            configured: true,
            os: "ubuntu".into(),
            state,
            unit: if running { "active" } else { "inactive" }.into(),
            vm: if running { "Running" } else { "-" }.into(),
            agent: if running { "up" } else { "" }.into(),
            memory_bytes: if running { 3 * GIB } else { 0 },
            boot_memory_bytes: 2 * GIB,
            max_memory_bytes: 16 * GIB,
            disk_bytes: 64 << 30,
            disk_used_bytes: 20 << 30,
            apps: vec![App {
                id: "t3code".into(),
                name: "T3 Code".into(),
                url: Some(format!("https://{name}--t3code.ws.test")),
                open_link: true,
                ..App::default()
            }],
            ..Workspace::default()
        },
        booted: running.then(|| boot.clone()),
        next: boot,
        stopped: !running,
    }
}

/// The UI as a front runs it, with what the tests fake around it: who a
/// cookie is, the units and how they went.
struct OnFront {
    inner: FleetBackend,
    units: Arc<Mutex<Vec<String>>>,
    outcome: Arc<Mutex<Option<Outcome>>>,
    /// Its units fail, whatever their outcome says.
    units_fail: Arc<std::sync::atomic::AtomicBool>,
}

impl Backend for OnFront {
    fn list(&self) -> BoxFuture<'_, anyhow::Result<Vec<Workspace>>> {
        self.inner.list()
    }

    fn control<'a>(&'a self, verb: Verb, name: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
        self.inner.control(verb, name)
    }

    fn config(&self, name: &str) -> anyhow::Result<Config> {
        self.inner.config(name)
    }

    fn reachable(&self, c: &Config) -> Vec<BootApp> {
        self.inner.reachable(c)
    }

    fn open_link(&self, c: Config, app: String) -> BoxFuture<'_, Result<String, OpenLinkError>> {
        self.inner.open_link(c, app)
    }

    fn whoami<'a>(&'a self, cookie: &'a str) -> BoxFuture<'a, anyhow::Result<Identity>> {
        let who = who_is(cookie);
        Box::pin(async move { Ok(who) })
    }

    fn run_unit<'a>(&'a self, unit: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
        self.units.lock().unwrap().push(unit.to_owned());
        let outcome = self.outcome.lock().unwrap().clone();
        let mut ran = ran_unit(Some(&self.inner.fleet.paths()), unit, outcome.as_ref());
        if self.units_fail.load(std::sync::atomic::Ordering::Relaxed) {
            ran = Err(anyhow::anyhow!("{unit}: exit status 1"));
        }
        Box::pin(async move { ran })
    }

    fn host_ca(&self) -> anyhow::Result<String> {
        Ok("ssh-ed25519 AAAAhostca workbench host CA".into())
    }

    fn sign_user_key<'a>(&'a self, _: &'a str, _: &'a str, _: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async { anyhow::bail!("not in these tests") })
    }

    fn ssh_address(&self, c: &Config) -> String {
        self.inner.ssh_address(c)
    }

    fn fleet(&self) -> &Fleet {
        self.inner.fleet()
    }
}

/// A front with nodes `a`, `b` and `c`, its files in `dir`; each node's
/// API, for the consoles, is a socket there.
pub(super) fn settings(dir: &Path) -> FrontSettings {
    let node = |n: u8| NodeEntry {
        address: format!("10.231.0.{}", 10 + n).parse().unwrap(),
        subnet: format!("10.232.{n}"),
        api: Endpoint::Unix(dir.join(format!("node-{n}.sock"))),
    };
    let limits = Limits {
        os: vec!["ubuntu".into()],
        boot_mem_mib: 2048,
        max_memory: Ceiling::Size(16),
        host_memory_gib: 0,
        max_disk_gib: 128,
        quota: 1,
        apps: vec![AppInfo { id: "t3code".into(), name: "T3 Code".into(), terminal: false, default: true }],
    };
    FrontSettings {
        domain: "ws.test".into(),
        scheme: "https".into(),
        state_dir: dir.join("state").display().to_string(),
        listen: None,
        socket: String::new(),
        local_node: String::new(),
        nodes: [("a".to_owned(), node(0)), ("b".to_owned(), node(1)), ("c".to_owned(), node(2))].into(),
        declared: dir.join("declared").display().to_string(),
        apps: vec![t3code()],
        default_apps: vec!["t3code".into()],
        ssh: SshCa::default(),
        self_service: Some(FrontSelfService {
            limits,
            apps: vec!["t3code".into()],
            secrets_root: dir.join("secrets").display().to_string(),
            environment_files: vec![],
            environment_command: "/bin/true".into(),
            owner_command: String::new(),
        }),
    }
}

fn record(name: &str, owner: &str, node: &str, ip: &str) -> Record {
    Record {
        name: name.into(),
        owner: owner.into(),
        node: node.into(),
        ip: ip.into(),
        os: "ubuntu".into(),
        max_memory: Ceiling::Size(16),
        disk_gib: 64,
        apps: vec!["t3code".into()],
        ..Record::default()
    }
}

/// A front and its nodes: alice's declared `alice` and her `alice-dev`,
/// made at run time, on `a`; bob's `bob-m`, moving from `b` to `a`; dave's
/// `dave` on `c`, which doesn't answer.
struct Front {
    dir: tempfile::TempDir,
    state: AppState,
    a: Arc<FakeNode>,
    b: Arc<FakeNode>,
    c: Arc<FakeNode>,
    units: Arc<Mutex<Vec<String>>>,
    outcome: Arc<Mutex<Option<Outcome>>>,
    units_fail: Arc<std::sync::atomic::AtomicBool>,
}

impl Front {
    fn fleet(&self) -> &Fleet {
        self.state.backend.fleet()
    }

    fn state_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("state")
    }
}

fn front() -> Front {
    let dir = tempfile::tempdir().unwrap();
    let settings = settings(dir.path());
    let declared = Path::new(&settings.declared);
    std::fs::create_dir_all(declared).unwrap();
    let alice = record("alice", "alice", "a", "10.232.0.10");
    std::fs::write(declared.join("alice.json"), serde_json::to_vec(&alice).unwrap()).unwrap();
    let a = FakeNode::new(vec![on_node("alice", State::Running), on_node("alice-dev", State::Running)]);
    let b = FakeNode::new(vec![on_node("bob-m", State::Stopped)]);
    let c = FakeNode::new(vec![]);
    c.down();
    let nodes: BTreeMap<String, Arc<dyn Node>> = [
        ("a".to_owned(), a.clone() as Arc<dyn Node>),
        ("b".to_owned(), b.clone() as Arc<dyn Node>),
        ("c".to_owned(), c.clone() as Arc<dyn Node>),
    ]
    .into();
    let fleet = Fleet::with_nodes(settings.clone(), nodes);
    fleet.registry.put(&record("alice-dev", "alice", "a", "10.232.0.128")).unwrap();
    let moving = Move {
        from: "b".into(),
        from_ip: "10.232.1.128".into(),
        to: "a".into(),
        ip: "10.232.0.129".into(),
        phase: Phase::Copy,
        ran: false,
    };
    fleet.registry.put(&Record { moving: Some(moving), ..record("bob-m", "bob", "b", "10.232.1.128") }).unwrap();
    fleet.registry.put(&record("dave", "dave", "c", "10.232.2.128")).unwrap();
    let ss = settings.self_service.as_ref().unwrap();
    let opts = Options {
        origin: ORIGIN.into(),
        admins: vec!["carol".into()],
        creator_groups: vec!["workbench".into()],
        self_service: Some(SelfService { limits: ss.limits.clone(), paths: fleet.paths() }),
        cookie_name: "_workbench_auth".into(),
        devices: devices_file(),
        ..Options::default()
    };
    let (units, outcome) = (Arc::default(), Arc::new(Mutex::new(Some(Outcome { ok: true, ..Outcome::default() }))));
    let units_fail = Arc::default();
    let backend = OnFront {
        inner: FleetBackend::new(opts.clone(), fleet),
        units: Arc::clone(&units),
        outcome: Arc::clone(&outcome),
        units_fail: Arc::clone(&units_fail),
    };
    let leptos =
        LeptosOptions::builder().output_name("workbench-web").site_root("target/site").site_pkg_dir("pkg").build();
    let state = AppState::new(opts, Box::new(backend), leptos).unwrap();
    Front { dir, state, a, b, c, units, outcome, units_fail }
}

/// A request from someone in the self-service group.
fn member(mut req: Request<Body>) -> Request<Body> {
    req.headers_mut().insert("x-workbench-groups", "workbench".parse().unwrap());
    req
}

async fn listed(s: &AppState, user: &str) -> Vec<Workspace> {
    let (code, _, body) = send(ui(s), member(get("/api/workspaces", user))).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    serde_json::from_str(&body).unwrap()
}

fn find<'a>(all: &'a [Workspace], name: &str) -> &'a Workspace {
    all.iter().find(|w| w.name == name).unwrap_or_else(|| panic!("no {name} in {all:?}"))
}

/// The list is the registry's, each workspace's state its node's: one
/// that moves is Moving, one whose node doesn't answer Unreachable, with
/// what its node last said of it. Admins see each one's node, owners
/// never.
#[tokio::test]
async fn the_list_merges_every_node() {
    let _turn = TURN.lock().await;
    let f = front();
    // c answered once, then stopped.
    f.c.up(vec![on_node("dave", State::Running)]);
    let all = listed(&f.state, "carol").await;
    assert_eq!(find(&all, "dave").state, State::Running);
    f.c.down();
    f.state.forget().await;
    let all = listed(&f.state, "carol").await;
    let states: Vec<_> = all.iter().map(|w| (w.name.as_str(), w.state, w.node.as_str())).collect();
    assert_eq!(
        states,
        [
            ("alice", State::Running, "a"),
            ("alice-dev", State::Running, "a"),
            ("bob-m", State::Moving, "b"),
            ("dave", State::Unreachable, "c"),
        ]
    );
    let dave = find(&all, "dave");
    assert_eq!((dave.memory_bytes, dave.disk_bytes), (3 * GIB, 64 << 30), "what its node last said");
    assert_eq!(dave.vm, "-", "nor that its VM runs");

    let mine = listed(&f.state, "alice").await;
    assert_eq!(mine.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(), ["alice", "alice-dev"]);
    assert!(mine.iter().all(|w| w.node.is_empty() && w.ip.is_empty()), "owners see nothing of nodes");
    let (_, _, body) = send(ui(&f.state), member(get("/api/workspaces", "bob"))).await;
    assert!(body.contains(r#""state":"moving""#) && !body.contains(r#""node""#), "{body}");

    // The panel says so, and its actions wait.
    let (code, _, body) = send(ui(&f.state), get("/?w=dave", "carol")).await;
    assert_eq!(code, StatusCode::OK);
    assert!(body.contains("Its node doesn't answer.") && body.contains("Disk on its node"), "{body}");
    assert!(body.contains(r#"class="kv gone""#), "greyed: {body}");
    assert!(!body.contains(r#"href="/w/dave/console""#), "no console while its node doesn't answer");
    let (_, _, body) = send(ui(&f.state), member(get("/?w=bob-m", "bob"))).await;
    assert!(body.contains("Moving to another node.") && !body.contains("Disk on its node"), "{body}");
    let (code, h, body) =
        send(ui(&f.state), member(post("/api/control", "dave", ORIGIN, "ws=dave&action=start"))).await;
    assert_eq!(
        (code, h[header::LOCATION].to_str().unwrap()),
        (StatusCode::SEE_OTHER, "/w/dave/start?error=busy"),
        "{body}"
    );
    assert!(f.c.controlled.lock().unwrap().is_empty(), "an action waits for its node");

    // What works goes through its node.
    let (code, _, _) =
        send(ui(&f.state), member(post("/api/control", "alice", ORIGIN, "ws=alice-dev&action=stop"))).await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    assert_eq!(*f.a.controlled.lock().unwrap(), [(Verb::Stop, "alice-dev".to_owned())]);
    let (code, h, _) = send(ui(&f.state), member(post("/api/open", "alice", ORIGIN, "ws=alice&app=t3code"))).await;
    assert_eq!(
        (code, h[header::LOCATION].to_str().unwrap()),
        (StatusCode::SEE_OTHER, "https://alice--t3code.ws.test/pair#token=t")
    );
}

/// The access check and the tunnel read the registry: an app answers while
/// its node's API doesn't, and the tunnel refuses while it can't tell the
/// VM runs.
#[tokio::test(flavor = "multi_thread")]
async fn the_access_check_and_the_tunnel_read_the_registry() {
    use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};
    let _turn = TURN.lock().await;
    let f = front();
    let check = |ws: &str, who: &str| {
        Request::get("/authz")
            .header("x-workbench-workspace", ws)
            .header("x-workbench-app", "t3code")
            .header(header::COOKIE, format!("_workbench_auth={who}"))
            .body(Body::empty())
            .unwrap()
    };
    let (code, h, _) = send(authz_router(f.state.clone()), check("dave", "dave:workbench")).await;
    assert_eq!(code, StatusCode::OK, "its node doesn't answer, its app does");
    assert_eq!(h["x-workbench-upstream"], "10.232.2.128:3773", "at the registry's address");
    let (code, _, _) = send(authz_router(f.state.clone()), check("dave", "alice:workbench")).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "the registry says whose it is");
    let (code, _, _) = send(authz_router(f.state.clone()), check("dave", "dave")).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "a workspace made at run time needs its owner in the group");
    assert_eq!(f.state.backend.ssh_address(&f.state.backend.config("dave").unwrap()), "10.232.2.128:22");

    // An app taken from a record still opens while the VM runs it, and
    // not once it restarted without it, though no one listed since.
    let r = f.fleet().registry.get("alice-dev").unwrap().unwrap();
    f.fleet().registry.put(&Record { apps: vec![], ..r }).unwrap();
    let (code, h, _) = send(authz_router(f.state.clone()), check("alice-dev", "alice:workbench")).await;
    assert_eq!((code, h["x-workbench-upstream"].to_str().unwrap()), (StatusCode::OK, "10.232.0.128:3773"));
    let restarted = NodeWorkspace {
        booted: Some(Boot { packages: "/nix/store/x-packages".into(), apps: vec![] }),
        next: Boot { packages: "/nix/store/x-packages".into(), apps: vec![] },
        ..on_node("alice-dev", State::Running)
    };
    f.a.up(vec![on_node("alice", State::Running), restarted]);
    std::thread::sleep(SNAPSHOT_FOR);
    let (code, _, _) = send(authz_router(f.state.clone()), check("alice-dev", "alice:workbench")).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "gone with the restart");
    f.a.down();
    let (code, _, _) = send(authz_router(f.state.clone()), check("alice", "alice")).await;
    assert_eq!(code, StatusCode::OK, "a declared one, while its node doesn't answer");

    let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(axum::serve(server, connect_router(f.state.clone())).into_future());
    let s = &f.state;
    let dave = s.devices.pair(&s.devices.make_code("dave", &["workbench".into()]).unwrap(), "laptop").unwrap().0;
    let mut req = format!("ws://{addr}/connect/ssh/dave").into_client_request().unwrap();
    req.headers_mut().insert(header::AUTHORIZATION, format!("Bearer {dave}").parse().unwrap());
    match tokio_tungstenite::connect_async(req).await {
        Err(tungstenite::Error::Http(res)) => {
            assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
            let body = String::from_utf8_lossy(res.body().as_deref().unwrap_or_default()).into_owned();
            assert!(body.contains("can't tell it runs"), "{body}");
        }
        other => panic!("the tunnel opened, or failed otherwise: {:?}", other.map(|_| ())),
    }
}

/// New workspace lists the nodes for an admin, the one placement takes
/// first; a closed, silent or other build's one can't be picked, and the
/// pick is the request's, even one placement wouldn't take. Anyone else's
/// is the placement's.
#[tokio::test]
async fn admins_pick_the_node() {
    let _turn = TURN.lock().await;
    let f = front();
    f.fleet().set_closed("b", true).unwrap();
    let (code, _, body) = send(ui(&f.state), get("/api/new", "carol")).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let (_, new): (Snapshot, NewView) = serde_json::from_str(&body).unwrap();
    let nodes: Vec<_> = new.nodes.iter().map(|n| (n.name.as_str(), n.state)).collect();
    assert_eq!(nodes, [("a", NodeState::Up), ("b", NodeState::Closed), ("c", NodeState::Unreachable)]);
    assert_eq!(new.node, "a", "preselected: the one placement takes");
    let (_, _, body) = send(ui(&f.state), member(get("/api/new", "alice"))).await;
    let (_, new): (Snapshot, NewView) = serde_json::from_str(&body).unwrap();
    assert!(new.nodes.is_empty() && new.node.is_empty(), "only admins see nodes");

    let (code, _, body) = send(ui(&f.state), get("/new", "carol")).await;
    assert_eq!(code, StatusCode::OK);
    assert!(body.contains(r#"<select name="node">"#), "{body}");
    assert!(body.contains(r#"<option value="a" selected>a · 34 GiB free"#), "{body}");
    assert!(
        body.contains(r#"<option value="b" disabled>b · closed · 34 GiB free · its workspaces up to 16 GiB"#),
        "{body}"
    );
    assert!(body.contains(r#"<option value="c" disabled>c · unreachable<"#), "{body}");

    let form =
        |node: &str| format!("name={}&os=ubuntu&memory_gib=8&disk_gib=64&apps%5B0%5D=t3code&node={node}", "carol");
    let request = f.fleet().paths().request("carol");
    for (node, why) in [("b", "closed"), ("c", "doesn"), ("z", "isn")] {
        let (code, h, _) = send(ui(&f.state), post("/api/create", "carol", ORIGIN, &form(node))).await;
        let to = h[header::LOCATION].to_str().unwrap();
        assert_eq!(code, StatusCode::SEE_OTHER);
        assert!(
            to.starts_with("/new?error=") && to.contains(why) && to.contains(&format!("node={node}")),
            "{node}: {to}"
        );
        assert!(!request.exists(), "{node}: no request");
    }
    f.c.of_another_build();
    f.state.forget().await;
    let (_, _, body) = send(ui(&f.state), get("/new", "carol")).await;
    assert!(body.contains(r#"<option value="c" disabled>c · other build<"#), "{body}");
    let (_, h, _) = send(ui(&f.state), post("/api/create", "carol", ORIGIN, &form("c"))).await;
    assert!(h[header::LOCATION].to_str().unwrap().contains("another%20build"), "{:?}", h[header::LOCATION]);
    assert!(!request.exists(), "no request for another build's node");

    // b open again: placement takes a, the first of two alike; the admin
    // picks b.
    f.fleet().set_closed("b", false).unwrap();
    f.state.forget().await;
    let (_, _, body) = send(ui(&f.state), get("/api/new", "carol")).await;
    let (_, new): (Snapshot, NewView) = serde_json::from_str(&body).unwrap();
    assert_eq!(new.node, "a");
    let (code, h, _) = send(ui(&f.state), post("/api/create", "carol", ORIGIN, &form("b"))).await;
    assert_eq!((code, h[header::LOCATION].to_str().unwrap()), (StatusCode::SEE_OTHER, "/?w=carol"));
    let asked: Asked = serde_json::from_slice(&std::fs::read(&request).unwrap()).unwrap();
    assert_eq!(asked.node, "b", "the admin's pick, not placement's");
    assert_eq!(f.units.lock().unwrap().last().unwrap(), "workbench-create@carol.service");

    let theirs = "name=alice-new&os=ubuntu&memory_gib=8&disk_gib=64&node=a";
    let mut quotas = BTreeMap::new();
    quotas.insert("alice", 5);
    let path = f.fleet().paths().quotas();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec(&quotas).unwrap()).unwrap();
    let (code, h, _) = send(ui(&f.state), member(post("/api/create", "alice", ORIGIN, theirs))).await;
    assert_eq!((code, h[header::LOCATION].to_str().unwrap()), (StatusCode::SEE_OTHER, "/?w=alice-new"));
    let asked: Asked = serde_json::from_slice(&std::fs::read(f.fleet().paths().request("alice-new")).unwrap()).unwrap();
    assert_eq!(asked.node, "", "a non-admin's pick is ignored");
}

/// Admins close a node, which the front keeps in `<stateDir>/closed`, and
/// reopen it, on the host page, whether it answers or not.
#[tokio::test]
async fn admins_close_and_reopen_nodes() {
    let _turn = TURN.lock().await;
    let f = front();
    let marker = f.state_dir().join("closed").join("b");
    let (code, _, _) = send(ui(&f.state), member(post("/api/node", "alice", ORIGIN, "node=b&closed=true"))).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert!(!marker.exists(), "only admins");
    let (code, h, _) = send(ui(&f.state), post("/api/node", "carol", ORIGIN, "node=b&closed=true")).await;
    assert_eq!((code, h[header::LOCATION].to_str().unwrap()), (StatusCode::SEE_OTHER, "/host"));
    assert!(marker.exists());
    let (code, _, body) = send(ui(&f.state), get("/host", "carol")).await;
    assert_eq!(code, StatusCode::OK);
    assert!(body.contains("Reopen b") && body.contains("Close a") && body.contains("Close c"), "{body}");
    // Closed, then down for maintenance: it says both, and reopens.
    f.b.down();
    f.state.forget().await;
    let (_, _, body) = send(ui(&f.state), get("/host", "carol")).await;
    let section = |n: &str| {
        let from = body.find(&format!(r#"aria-label="Node {n}""#)).unwrap();
        let to = body[from + 1..].find("aria-label=\"Node ").map_or(body.len(), |i| from + 1 + i);
        body[from..to].to_owned()
    };
    let b = section("b");
    assert!(b.contains(r#"class="ns unreachable""#) && b.contains(r#"class="ns closed""#), "{b}");
    assert!(b.contains("Reopen b"), "a closed node reopens while it is down: {b}");
    assert!(!section("c").contains(r#"class="ns closed""#) && section("c").contains("Close c"));
    let (code, _, _) = send(ui(&f.state), post("/api/node", "carol", ORIGIN, "node=b&closed=false")).await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    assert!(!marker.exists());
    let (code, _, _) = send(ui(&f.state), post("/api/node", "carol", ORIGIN, "node=..%2Fb&closed=true")).await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "a node of the fleet's only");
}

/// Admins move a workspace made at run time: the request is written and
/// `workbench-move@` started, and its outcome shown as a create's is; a
/// declared one moves by changing Nix.
#[tokio::test]
async fn admins_move_workspaces() {
    let _turn = TURN.lock().await;
    let f = front();
    let request = f.fleet().paths().move_request("alice-dev");
    let moving = |user: &str, form: &str| send(ui(&f.state), member(post("/api/move", user, ORIGIN, form)));
    let (code, _, _) = moving("alice", "ws=alice-dev&node=b").await;
    assert_eq!(code, StatusCode::FORBIDDEN, "admins only");
    for (form, why) in [
        ("ws=alice&node=b", "declared in Nix"),
        ("ws=alice-dev&node=a", "on a already"),
        ("ws=alice-dev&node=c", "doesn"),
        ("ws=bob-m&node=c", "moving to another node"),
    ] {
        let (code, h, _) = moving("carol", form).await;
        let to = h[header::LOCATION].to_str().unwrap();
        assert_eq!(code, StatusCode::SEE_OTHER, "{form}");
        assert!(to.contains("/move?error=") && to.contains(&encode(why)), "{form}: {to}");
        assert!(!request.exists(), "{form}: no request");
    }
    assert!(f.units.lock().unwrap().is_empty());

    let (code, h, _) = moving("carol", "ws=alice-dev&node=b").await;
    assert_eq!((code, h[header::LOCATION].to_str().unwrap()), (StatusCode::SEE_OTHER, "/host"));
    let asked: MoveRequest = serde_json::from_slice(&std::fs::read(&request).unwrap()).unwrap();
    assert_eq!(asked.node, "b");
    assert_eq!(*f.units.lock().unwrap(), ["workbench-move@alice-dev.service"]);

    *f.outcome.lock().unwrap() = Some(Outcome { ok: false, error: "No room on b".into(), ..Outcome::default() });
    let (code, h, _) = moving("carol", "ws=alice-dev&node=b").await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    assert_eq!(h[header::LOCATION], "/w/alice-dev/move?error=No%20room%20on%20b", "its outcome, as a create's");

    // A unit that fails failed, whatever outcome it wrote, as a change's.
    *f.outcome.lock().unwrap() = Some(Outcome { ok: true, ..Outcome::default() });
    f.units_fail.store(true, std::sync::atomic::Ordering::Relaxed);
    let (_, h, _) = moving("carol", "ws=alice-dev&node=b").await;
    let to = h[header::LOCATION].to_str().unwrap();
    assert!(to.starts_with("/w/alice-dev/move?error=") && to.contains("exit%20status%201"), "{to}");
    f.units_fail.store(false, std::sync::atomic::Ordering::Relaxed);

    // Another build's node takes no moves.
    f.c.of_another_build();
    f.state.forget().await;
    let (_, h, _) = moving("carol", "ws=alice-dev&node=c").await;
    assert!(h[header::LOCATION].to_str().unwrap().contains("another%20build"), "{:?}", h[header::LOCATION]);

    // Its page, and the host page's dialog.
    let (code, _, body) = send(ui(&f.state), get("/w/alice-dev/move?error=No%20room", "carol")).await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body.contains(r#"value="b""#) && !body.contains(r#"value="a""#) && !body.contains(r#"value="c""#),
        "{body}"
    );
    assert!(body.contains("down while its disk is copied") && body.contains("No room"), "{body}");
    let (code, _, _) = send(ui(&f.state), member(get("/w/alice-dev/move", "alice"))).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "admins only");
    let (_, _, body) = send(ui(&f.state), get("/host", "carol")).await;
    assert!(body.contains(r#"href="/w/alice-dev/move""#) && body.contains("declared in Nix"), "{body}");
    assert!(!body.contains(r#"href="/w/alice/move""#), "a declared one has no Move");
}

/// The console of a workspace on a node is that node's, relayed: its bytes
/// both ways, and its status.
#[tokio::test(flavor = "multi_thread")]
async fn the_console_relays_a_nodes() {
    use axum::extract::ws::{Message as Msg, WebSocketUpgrade};
    use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};
    let _turn = TURN.lock().await;
    let f = front();
    // Node a's console: it says who watches, then says back what it gets.
    let console = axum::Router::new().route(
        "/workspaces/{ws}/console",
        axum::routing::get(
            |axum::extract::Path(ws): axum::extract::Path<String>,
             axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
             up: WebSocketUpgrade| async move {
                let viewer = q.get("viewer").cloned().unwrap_or_default();
                let mut res = up.on_upgrade(move |mut sock| async move {
                    let status = format!(r#"{{"connected":true,"viewers":["{viewer}"],"ws":"{ws}"}}"#);
                    let _ = sock.send(Msg::Text(status.into())).await;
                    while let Some(Ok(Msg::Binary(b))) = sock.next().await {
                        let _ = sock.send(Msg::Binary(b)).await;
                    }
                });
                res.headers_mut().insert(workbench_model::fleet::VERSION_HEADER, VERSION.parse().unwrap());
                res
            },
        ),
    );
    let sock = f.dir.path().join("node-0.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    tokio::spawn(axum::serve(listener, console).into_future());
    let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(axum::serve(server, ui(&f.state)).into_future());
    let dial = |ws: &str, user: &str| {
        let mut req = format!("ws://{addr}/w/{ws}/console/socket").into_client_request().unwrap();
        req.headers_mut().insert("x-workbench-user", user.parse().unwrap());
        req.headers_mut().insert("x-workbench-groups", "workbench".parse().unwrap());
        req.headers_mut().insert(header::ORIGIN, ORIGIN.parse().unwrap());
        tokio_tungstenite::connect_async(req)
    };
    let (mut ws, _) = dial("alice", "alice").await.unwrap();
    let status = ws.next().await.unwrap().unwrap();
    assert_eq!(status.into_text().unwrap().as_str(), r#"{"connected":true,"viewers":["alice"],"ws":"alice"}"#);
    ws.send(tungstenite::Message::Binary(b"ls\r".to_vec().into())).await.unwrap();
    let back = ws.next().await.unwrap().unwrap();
    assert_eq!(back.into_data().as_ref(), b"ls\r", "through to the node's console and back");
    ws.close(None).await.unwrap();
    match dial("alice", "bob").await {
        Err(tungstenite::Error::Http(res)) => assert_eq!(res.status(), StatusCode::FORBIDDEN),
        other => panic!("{:?}", other.map(|_| ())),
    }
    match dial("dave", "dave").await {
        Err(tungstenite::Error::Http(res)) => {
            assert_eq!(res.status(), StatusCode::BAD_GATEWAY, "its node doesn't answer");
            let body = String::from_utf8_lossy(res.body().as_deref().unwrap_or_default()).into_owned();
            assert!(body.contains("Its node doesn't answer") && !body.contains("node, c"), "no node to owners: {body}");
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
}

/// Owners never read a node's name, not even in what a node's refusal
/// says; admins do.
#[tokio::test]
async fn owners_never_read_a_nodes_name() {
    let _turn = TURN.lock().await;
    let f = front();
    *f.a.refuses.lock().unwrap() = Some(CallError::Refused { status: 500, error: "systemctl: job failed".into() });
    let stop = |user: &str| send(ui(&f.state), member(post("/api/control", user, ORIGIN, "ws=alice-dev&action=stop")));
    let (_, h, _) = stop("alice").await;
    let to = h[header::LOCATION].to_str().unwrap().to_owned();
    assert!(to.starts_with("/w/alice-dev/stop?error=failed&detail=its%20node%3A%20systemctl"), "{to}");
    assert!(!to.contains("node%20a"), "{to}");
    let (_, h, _) = stop("carol").await;
    assert!(h[header::LOCATION].to_str().unwrap().contains("detail=node%20a%3A%20systemctl"), "admins read it");

    // Its node went quiet since the list said it runs.
    *f.a.refuses.lock().unwrap() = None;
    listed(&f.state, "carol").await;
    f.a.down();
    let (_, h, _) = stop("alice").await;
    let to = h[header::LOCATION].to_str().unwrap();
    assert!(to.contains("detail=Its%20node%20doesn%27t%20answer") && !to.contains("%20a%2C"), "{to}");

    // A unit's refusal names its node in its outcome: an owner reads it
    // as the refusal words it for them.
    let other = workbench_model::fleet::NodeRefusal::OtherBuild { node: "a".into(), build: "x".into() };
    let said = Outcome { ok: false, error: format!("The front couldn't change them: {other}"), node: Some(other) };
    *f.outcome.lock().unwrap() = Some(said);
    let apps =
        |user: &str| send(ui(&f.state), member(post("/api/apps", user, ORIGIN, "ws=alice-dev&apps%5B0%5D=t3code")));
    let (_, h, _) = apps("alice").await;
    let to = h[header::LOCATION].to_str().unwrap().to_owned();
    assert!(to.contains("Its%20node%20is%20of%20another%20build%3A%20an%20admin%20deploys%20it."), "{to}");
    assert!(!to.contains("%20a%2C") && !to.contains("%28x%29"), "{to}");
    let (_, h, _) = apps("carol").await;
    assert!(
        h[header::LOCATION].to_str().unwrap().contains("Its%20node%2C%20a%2C%20is%20of%20another%20build%20%28x%29")
    );
}

/// While a workspace moves, its apps say so: the access check refuses, and
/// the page nginx shows then says it moves.
#[tokio::test]
async fn a_moving_workspaces_apps_say_so() {
    let _turn = TURN.lock().await;
    let f = front();
    let check = Request::get("/authz")
        .header("x-workbench-workspace", "bob-m")
        .header("x-workbench-app", "t3code")
        .header(header::COOKIE, "_workbench_auth=bob:workbench")
        .body(Body::empty())
        .unwrap();
    let (code, h, _) = send(authz_router(f.state.clone()), check).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "its VM is down");
    assert!(!h.contains_key("x-workbench-upstream"));
    let (code, _, body) = send(ui(&f.state), member(get("/denied?ws=bob-m&app=t3code", "bob"))).await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.contains("is moving to another node") && body.contains("/?w=bob-m"), "{body}");
    let (code, _, body) = send(ui(&f.state), member(get("/denied?ws=bob-m&app=t3code", "alice"))).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "someone else's says only that: {body}");
}

/// The access check refreshes the state only for an app the record no
/// longer has that the last list saw run: an app no one has slows no
/// page, even with a slow node; and one it can't tell still runs is
/// refused.
#[tokio::test(flavor = "multi_thread")]
async fn the_access_check_refreshes_for_a_removed_app_only() {
    let _turn = TURN.lock().await;
    let f = front();
    let check = |app: &str| {
        Request::get("/authz")
            .header("x-workbench-workspace", "alice-dev")
            .header("x-workbench-app", app)
            .header(header::COOKIE, "_workbench_auth=alice:workbench")
            .body(Body::empty())
            .unwrap()
    };
    let r = f.fleet().registry.get("alice-dev").unwrap().unwrap();
    f.fleet().registry.put(&Record { apps: vec![], ..r }).unwrap();
    listed(&f.state, "carol").await;
    *f.b.slow.lock().unwrap() = std::time::Duration::from_secs(6);
    tokio::time::sleep(SNAPSHOT_FOR + std::time::Duration::from_millis(100)).await;
    let asked = std::time::Instant::now();
    let (code, _, _) = send(authz_router(f.state.clone()), check("nosuch")).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let all = tokio::time::timeout(std::time::Duration::from_millis(500), f.state.all()).await;
    assert!(
        all.is_ok() && asked.elapsed() < std::time::Duration::from_secs(1),
        "an app it never had waits for nothing"
    );
    let (code, _, _) = send(authz_router(f.state.clone()), check("t3code")).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "no state under a second old says the VM still runs it");
}

/// An admin changes a workspace's memory within its own node's, and not
/// while its node doesn't answer.
#[tokio::test]
async fn the_memory_of_a_workspace_is_its_node_s() {
    let _turn = TURN.lock().await;
    let f = front();
    *f.a.mem_gib.lock().unwrap() = 8;
    let (code, _, body) = send(ui(&f.state), get("/w/alice-dev/memory", "carol")).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert!(body.contains(r#"max="8""#) && body.contains("2 to 8"), "a's 8 GiB, not the front's 16: {body}");
    let (_, _, panel) = send(ui(&f.state), get("/?w=alice-dev", "carol")).await;
    assert!(panel.contains(r#"href="/w/alice-dev/memory""#), "{panel}");
    f.a.down();
    f.state.forget().await;
    let (_, _, panel) = send(ui(&f.state), get("/?w=alice-dev", "carol")).await;
    assert!(!panel.contains(r#"href="/w/alice-dev/memory""#), "its node doesn't answer: {panel}");
    let (code, _, _) = send(ui(&f.state), get("/w/alice-dev/memory", "carol")).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "nor on its own page");
}

/// A page asked for long after anyone looked shows the state now, not
/// the one last collected.
#[tokio::test]
async fn a_page_after_a_while_is_current() {
    let _turn = TURN.lock().await;
    let f = front();
    let state_of = |all: &[Workspace]| all.iter().find(|w| w.name == "alice-dev").map(|w| w.state);
    assert_eq!(state_of(&f.state.all().await.unwrap()), Some(State::Running));
    f.a.up(vec![on_node("alice", State::Running), on_node("alice-dev", State::Stopped)]);
    tokio::time::sleep(SHOWN_FOR).await;
    assert_eq!(state_of(&f.state.all().await.unwrap()), Some(State::Stopped));
}

/// A node slow to answer holds no page for long. While anyone looks,
/// everyone gets the last state at once while it is collected again. An
/// answer to an action, and a page after no one looked for a while, wait
/// for a new one at most [`COLLECT_WAIT`], then make do with the last.
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_node_holds_no_page_for_long() {
    let _turn = TURN.lock().await;
    let f = front();
    listed(&f.state, "carol").await;
    *f.b.slow.lock().unwrap() = std::time::Duration::from_secs(6);
    tokio::time::sleep(SNAPSHOT_FOR + std::time::Duration::from_millis(100)).await;
    for _ in 0..3 {
        let all = tokio::time::timeout(std::time::Duration::from_millis(500), f.state.all()).await;
        assert!(all.expect("the last state, at once").unwrap().iter().any(|w| w.name == "bob-m"));
    }
    let waits = |what: &'static str| {
        let state = f.state.clone();
        async move {
            let asked = std::time::Instant::now();
            assert!(state.all().await.unwrap().iter().any(|w| w.name == "bob-m"), "{what}: the last state");
            let waited = asked.elapsed();
            assert!(
                waited >= COLLECT_WAIT && waited < COLLECT_WAIT + std::time::Duration::from_secs(1),
                "{what}: {waited:?}"
            );
        }
    };
    f.state.forget().await;
    waits("an action's answer").await;
    // The collection it began ends; then no one looks.
    tokio::time::sleep(std::time::Duration::from_secs(4) + SHOWN_FOR).await;
    waits("a page after a while").await;
}
