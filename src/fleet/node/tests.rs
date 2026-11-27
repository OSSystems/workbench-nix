//! The node's API against a fake node: it answers the front only, at its
//! version, and streams disks both ways; then the HTTP client against it
//! over a Unix socket.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Mutex;

use axum::extract::ConnectInfo;
use axum::http::Request as HttpRequest;
use tower::ServiceExt as _;
use workbench_model::fleet::{NODE_PORT, Record, VERSION_HEADER};

use super::*;
use crate::fleet::Endpoint;
use crate::fleet::client::{Node, NodeClient};
use crate::fleet::http::Local;

const FRONT: &str = "10.231.0.1";

#[derive(Default)]
struct Fake {
    dir: PathBuf,
    calls: Mutex<Vec<String>>,
}

impl Fake {
    fn log(&self, s: String) {
        self.calls.lock().unwrap().push(s);
    }
}

impl NodeOps for Fake {
    fn host(&self) -> BoxFuture<'_, anyhow::Result<HostInfo>> {
        Box::pin(async { Ok(HostInfo { version: VERSION.into(), mem_total: 1 << 36, ..HostInfo::default() }) })
    }

    fn workspaces(&self) -> BoxFuture<'_, anyhow::Result<Vec<NodeWorkspace>>> {
        Box::pin(async { Ok(vec![]) })
    }

    fn make(&self, req: MakeRequest) -> BoxFuture<'_, anyhow::Result<()>> {
        self.log(format!("make {} {}", req.record.name, req.for_move));
        Box::pin(async { Ok(()) })
    }

    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
        self.log(format!("delete {name}"));
        Box::pin(async { Ok(()) })
    }

    fn control<'a>(&'a self, name: &'a str, verb: Verb, wait: bool) -> BoxFuture<'a, anyhow::Result<()>> {
        self.log(format!("{verb} {name} {wait}"));
        Box::pin(async move {
            if name == "bob" {
                bail!(Status(StatusCode::NOT_FOUND, "no bob".into()));
            }
            Ok(())
        })
    }

    fn open_link<'a>(&'a self, name: &'a str, app: &'a str) -> BoxFuture<'a, anyhow::Result<OpenLinkAnswer>> {
        Box::pin(async move { Ok(OpenLinkAnswer::Link { link: format!("https://{name}--{app}.ws.test/") }) })
    }

    fn disk<'a>(&'a self, name: &'a str) -> BoxFuture<'a, anyhow::Result<PathBuf>> {
        Box::pin(async move { Ok(self.dir.join(format!("{name}.raw"))) })
    }

    fn disk_target(&self, name: &str) -> anyhow::Result<(PathBuf, Account)> {
        let path = self.dir.join(format!("{name}-copy.raw"));
        ensure!(!path.exists(), Status(StatusCode::CONFLICT, "a disk is there".into()));
        let me = Account { uid: nix::unistd::getuid().as_raw(), gid: nix::unistd::getgid().as_raw(), groups: vec![] };
        Ok((path, me))
    }

    fn serial(&self, _name: &str) -> anyhow::Result<PathBuf> {
        Ok(self.dir.join("serial.sock"))
    }
}

fn app(fake: Arc<Fake>) -> Router {
    router(fake, FRONT.parse().unwrap(), Arc::new(Consoles::default()))
}

async fn send(app: &Router, method: &str, path: &str, from: Option<&str>, body: Body) -> (StatusCode, Bytes) {
    let mut req = HttpRequest::builder().method(method).uri(path).header(VERSION_HEADER, VERSION);
    match from {
        Some("local") => req = req.extension(Local),
        Some(ip) => req = req.extension(ConnectInfo(format!("{ip}:50000").parse::<SocketAddr>().unwrap())),
        None => {}
    }
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    assert_eq!(resp.headers()[VERSION_HEADER], VERSION);
    let status = resp.status();
    (status, axum::body::to_bytes(resp.into_body(), 1 << 30).await.unwrap())
}

#[tokio::test]
async fn the_node_answers_the_front_only() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(Arc::new(Fake { dir: dir.path().into(), ..Fake::default() }));
    for from in [Some(FRONT), Some("local")] {
        assert_eq!(send(&app, "GET", "/host", from, Body::empty()).await.0, StatusCode::OK, "{from:?}");
    }
    // Another node, a guest behind it, a guest of this node, nobody.
    for from in [Some("10.231.0.11"), Some("10.232.1.128"), Some("10.232.0.130"), None] {
        let (status, body) = send(&app, "GET", "/workspaces", from, Body::empty()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{from:?}: {}", String::from_utf8_lossy(&body));
    }
    let req = HttpRequest::get("/host")
        .header(VERSION_HEADER, "0.0.1")
        .extension(ConnectInfo(format!("{FRONT}:1").parse::<SocketAddr>().unwrap()))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT, "another version is refused");
    assert_eq!(resp.headers()[VERSION_HEADER], VERSION);
}

#[tokio::test]
async fn requests_reach_the_node() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake { dir: dir.path().into(), ..Fake::default() });
    let app = app(fake.clone());
    let record = Record { name: "alice".into(), node: "a".into(), ip: "10.232.0.130".into(), ..Record::default() };
    let make = serde_json::to_vec(&MakeRequest { record: record.clone(), for_move: true }).unwrap();
    assert_eq!(send(&app, "PUT", "/workspaces/alice", Some(FRONT), Body::from(make.clone())).await.0, StatusCode::OK);
    assert_eq!(
        send(&app, "PUT", "/workspaces/bob", Some(FRONT), Body::from(make)).await.0,
        StatusCode::BAD_REQUEST,
        "a record of another name"
    );
    let bad =
        serde_json::to_vec(&MakeRequest { record: Record { ip: "x".into(), ..record }, for_move: false }).unwrap();
    assert_eq!(send(&app, "PUT", "/workspaces/alice", Some(FRONT), Body::from(bad)).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(send(&app, "POST", "/workspaces/alice/stop?wait=1", Some(FRONT), Body::empty()).await.0, StatusCode::OK);
    assert_eq!(send(&app, "POST", "/workspaces/alice/start", Some(FRONT), Body::empty()).await.0, StatusCode::OK);
    assert_eq!(send(&app, "POST", "/workspaces/bob/start", Some(FRONT), Body::empty()).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        send(&app, "POST", "/workspaces/alice/shell", Some(FRONT), Body::empty()).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(send(&app, "DELETE", "/workspaces/Alice", Some(FRONT), Body::empty()).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(send(&app, "DELETE", "/workspaces/alice", Some(FRONT), Body::empty()).await.0, StatusCode::OK);
    let (status, body) =
        send(&app, "POST", "/workspaces/alice/open-link", Some(FRONT), Body::from(r#"{"app":"t3code"}"#)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<OpenLinkAnswer>(&body).unwrap(),
        OpenLinkAnswer::Link { link: "https://alice--t3code.ws.test/".into() }
    );
    assert_eq!(
        *fake.calls.lock().unwrap(),
        ["make alice true", "stop alice true", "start alice false", "start bob false", "delete alice"]
    );
}

/// A disk goes out as a stream and comes in as one, through the API.
#[tokio::test]
async fn disks_through_the_api() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(Arc::new(Fake { dir: dir.path().into(), ..Fake::default() }));
    let src = dir.path().join("alice.raw");
    let f = std::fs::File::create(&src).unwrap();
    f.set_len(32 << 20).unwrap();
    std::os::unix::fs::FileExt::write_all_at(&f, b"data", 20 << 20).unwrap();
    let (status, stream) = send(&app, "GET", "/workspaces/alice/disk", Some(FRONT), Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(stream.len() < 64 << 10, "the holes travelled: {}", stream.len());
    let (status, body) = send(&app, "PUT", "/workspaces/alice/disk", Some(FRONT), Body::from(stream.clone())).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let answer: DiskAnswer = serde_json::from_slice(&body).unwrap();
    let mut tail = disk::Tail::default();
    tail.push(&stream);
    assert_eq!(answer.sha256, disk::hex(&tail.finish().unwrap()));
    let copy = dir.path().join("alice-copy.raw");
    assert_eq!(std::fs::read(&copy).unwrap(), std::fs::read(&src).unwrap());
    assert_eq!(std::fs::metadata(&copy).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(!dir.path().join("alice-copy.raw.new").exists());
    let (status, _) = send(&app, "PUT", "/workspaces/alice/disk", Some(FRONT), Body::from(stream)).await;
    assert_eq!(status, StatusCode::CONFLICT, "a disk that is there stays");
    std::fs::remove_file(&copy).unwrap();
    let (status, _) = send(&app, "PUT", "/workspaces/alice/disk", Some(FRONT), Body::from("WBDISK1\n")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a broken stream");
    assert!(!copy.exists() && !dir.path().join("alice-copy.raw.new").exists(), "a broken stream leaves nothing");
}

/// The front's client, against the node's API on a Unix socket.
#[tokio::test]
async fn the_client_and_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake { dir: dir.path().into(), ..Fake::default() });
    let socket = dir.path().join("api.sock");
    let server = tokio::spawn({
        let (app, socket) = (app(fake.clone()), socket.clone());
        async move { http::serve_unix(&socket, "", app).await }
    });
    hostutil::wait_for_socket(&socket, std::time::Duration::from_secs(5)).await.unwrap();
    let node = NodeClient::new(Endpoint::Unix(socket));
    assert_eq!(node.host().await.unwrap().mem_total, 1 << 36);
    assert!(node.workspaces().await.unwrap().is_empty());
    node.control("alice", Verb::Restart, false).await.unwrap();
    let e = node.control("bob", Verb::Start, false).await.unwrap_err();
    assert_eq!((e.status(), e.to_string()), (Some(404), "no bob".to_owned()));
    let src = dir.path().join("alice.raw");
    std::fs::write(&src, vec![9u8; 3 << 20]).unwrap();
    let mut out = node.disk_out("alice").await.unwrap();
    let (tx, rx) = mpsc::channel(8);
    let pump = async move {
        while let Some(piece) = out.recv().await {
            tx.send(piece.unwrap()).await.unwrap();
        }
    };
    let ((), hash) = tokio::join!(pump, node.disk_in("alice", rx));
    assert_eq!(hash.unwrap().len(), 64);
    assert_eq!(std::fs::read(dir.path().join("alice-copy.raw")).unwrap(), std::fs::read(&src).unwrap());
    let gone = NodeClient::new(Endpoint::Unix(dir.path().join("nothing.sock")));
    assert!(matches!(gone.host().await, Err(crate::fleet::client::CallError::Unreachable(_))));
    server.abort();
    assert_eq!(NODE_PORT, 7910);
}

/// A node deletes a workspace made at run time without waiting for good
/// on the lock it holds: its own tools stand in for the host's. A lock
/// taken twice blocks its thread, so the delete runs on one of its own.
#[test]
fn a_node_deletes_what_it_made() {
    let dir = tempfile::tempdir().unwrap();
    let mut t = crate::self_service::tests::template();
    t.state_dir = dir.path().join("state").display().to_string();
    t.secrets_root = dir.path().join("secrets").display().to_string();
    let template = dir.path().join("template.json");
    std::fs::write(&template, serde_json::to_vec(&t).unwrap()).unwrap();
    let record = Record {
        name: "zz-delete".into(),
        owner: "alice".into(),
        node: "a".into(),
        ip: "10.232.0.254".into(),
        os: "nixos".into(),
        max_memory: workbench_model::memory::Ceiling::Size(8),
        disk_gib: 16,
        ..Record::default()
    };
    let c = crate::self_service::render(&t, &record, 60_999).unwrap();
    crate::self_service::write_config(&t.paths(), &c).unwrap();
    std::fs::create_dir_all(&c.state_dir).unwrap();
    let settings: NodeSettings = serde_json::from_value(serde_json::json!({
        "name": "a",
        "socket": "/run/workbench-node/api.sock",
        "frontAddress": FRONT,
        "front": "10.231.0.1:7911",
        "stateDir": t.state_dir,
        "template": template,
        "configDirs": [dir.path().join("etc"), t.paths().configs()],
    }))
    .unwrap();
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let log = asked.clone();
        crate::self_service::tools::stub(move |tool, args| {
            log.borrow_mut().push(format!("{tool} {}", args.join(" ")));
            // No unit runs.
            if args.first() == Some(&"is-active") {
                bail!("inactive");
            }
            Ok(())
        });
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let deleted = rt.block_on(HostNode { settings }.delete("zz-delete")).map_err(|e| format!("{e:#}"));
        let _ = done.send((deleted, asked.borrow().clone()));
    });
    let (deleted, asked) =
        finished.recv_timeout(std::time::Duration::from_secs(10)).expect("the delete waited on its own lock");
    deleted.unwrap();
    assert!(!t.paths().configs().join("zz-delete.json").exists() && !Path::new(&c.state_dir).exists());
    assert!(asked.contains(&"systemctl stop workbench@zz-delete.service".to_owned()), "{asked:?}");
}

/// A received disk is in place, and its directory synced, before the node
/// answers.
#[test]
fn a_received_disk_is_placed_for_good() {
    let dir = tempfile::tempdir().unwrap();
    let (tmp, path) = (dir.path().join("disk.raw.new"), dir.path().join("disk.raw"));
    std::fs::write(&tmp, b"disk").unwrap();
    let me = Account { uid: nix::unistd::getuid().as_raw(), gid: nix::unistd::getgid().as_raw(), groups: vec![] };
    let synced = Mutex::new(Vec::new());
    install(&tmp, &path, &me, |d| {
        assert!(path.exists() && !tmp.exists(), "synced before the rename");
        synced.lock().unwrap().push(d.to_owned());
        Ok(())
    })
    .unwrap();
    assert_eq!(*synced.lock().unwrap(), [dir.path().to_owned()], "the directory wasn't synced");
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
}

/// Over TCP, the node knows who asks by the connection's address.
#[tokio::test]
async fn over_tcp_the_peer_is_the_connection_s() {
    for (front, ok) in [("127.0.0.1", true), ("127.0.0.2", false)] {
        let dir = tempfile::tempdir().unwrap();
        let app = router(
            Arc::new(Fake { dir: dir.path().into(), ..Fake::default() }),
            front.parse().unwrap(),
            Arc::default(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        let server = tokio::spawn(http::serve_listener(listener, app));
        let got = NodeClient::new(Endpoint::Tcp(at)).host().await;
        assert_eq!(got.is_ok(), ok, "front {front}: {got:?}");
        if !ok {
            assert_eq!(got.unwrap_err().status(), Some(403));
        }
        server.abort();
    }
}

/// Another build is refused both ways: the node refuses a request of
/// another build, and the front's client an answer of another build.
#[tokio::test]
async fn another_build_is_refused_both_ways() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(Arc::new(Fake { dir: dir.path().into(), ..Fake::default() }));
    let req = HttpRequest::get("/host")
        .header(VERSION_HEADER, "another-build")
        .extension(ConnectInfo(format!("{FRONT}:1").parse::<SocketAddr>().unwrap()))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let other = Router::new()
        .route("/host", get(|| async { ([(VERSION_HEADER, "another-build")], axum::Json(HostInfo::default())) }))
        .route(
            "/start/{ws}",
            post(|| async { ([(VERSION_HEADER, "another-build")], axum::Json(serde_json::json!({}))) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = listener.local_addr().unwrap();
    let server = tokio::spawn(http::serve_listener(listener, other));
    let got = NodeClient::new(Endpoint::Tcp(at)).host().await;
    assert_eq!(got.unwrap_err(), crate::fleet::client::CallError::Version("another-build".into()));
    let front = crate::fleet::client::FrontClient { endpoint: Endpoint::Tcp(at) };
    assert!(matches!(front.start("alice").await, Err(crate::fleet::client::CallError::Version(_))));
    server.abort();
}
