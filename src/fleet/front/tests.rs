//! The front's API against fake nodes: it answers a node only for its own
//! workspaces, only from its `WireGuard` address, and never for one moving
//! away; and what the list makes of the registry and the nodes.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use tower::ServiceExt as _;
use workbench_model::fleet::{Move, Phase, Record, VERSION_HEADER};
use workbench_model::{Change, State};

use super::*;
use crate::fleet::VERSION;
use crate::fleet::fake::{FakeNode, fleet};
use crate::fleet::http::Local;
use crate::fleet::registry::tests::record;
use crate::fleet::tests::front_settings;

struct Setup {
    dir: tempfile::TempDir,
    a: Arc<FakeNode>,
    b: Arc<FakeNode>,
    f: Arc<Fleet>,
}

/// alice's workspace, made at run time, is on a, with secrets.
fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let a = FakeNode::new("a", dir.path().join("a"), 30);
    let b = FakeNode::new("b", dir.path().join("b"), 30);
    let mut settings = front_settings(dir.path());
    let extra = dir.path().join("llm.env");
    std::fs::write(&extra, "LLM_KEY=sk-2\n").unwrap();
    settings.self_service.as_mut().unwrap().environment_files = vec![extra.display().to_string()];
    let f = fleet(settings, &[&a, &b]);
    f.registry.put(&record("alice", "a", "10.232.0.130")).unwrap();
    let secrets = dir.path().join("secrets/alice");
    std::fs::create_dir_all(secrets.join("git")).unwrap();
    std::fs::write(secrets.join("environment"), "API_KEY=sk-1").unwrap();
    std::fs::write(secrets.join("git/credentials"), "https://u:p@git").unwrap();
    Setup { dir, a, b, f: Arc::new(f) }
}

/// Asks the front to start `name` as `from` would, with `version`.
async fn ask(f: &Arc<Fleet>, from: Option<&str>, name: &str, version: &str) -> (StatusCode, serde_json::Value) {
    let mut req = Request::post(format!("/start/{name}")).header(VERSION_HEADER, version);
    match from {
        Some("local") => req = req.extension(Local),
        Some(ip) => req = req.extension(ConnectInfo(format!("{ip}:40000").parse::<SocketAddr>().unwrap())),
        None => {}
    }
    let resp = router(f.clone()).oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    assert_eq!(resp.headers()[VERSION_HEADER], VERSION, "every answer has the front's version");
    let body = axum::body::to_bytes(resp.into_body(), 1 << 26).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or_default())
}

const A: &str = "10.231.0.10";
const B: &str = "10.231.0.11";

#[tokio::test]
async fn the_front_answers_a_node_for_its_own_workspaces_only() {
    let s = setup();
    let (status, answer) = ask(&s.f, Some(A), "alice", VERSION).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let answer: StartAnswer = serde_json::from_value(answer).unwrap();
    assert_eq!(
        answer.record.as_ref().map(|r| r.ip.as_str()),
        Some("10.232.0.130"),
        "one made at run time has its record"
    );
    // The secrets as the front reads them.
    let mut c = crate::config::tests::test_config();
    c.secrets_dir = s.dir.path().join("secrets/alice").display().to_string();
    c.environment_files = s.f.settings.self_service.as_ref().unwrap().environment_files.clone();
    let today = crate::launch::collect_secrets(std::path::Path::new(&c.secrets_dir), &c.environment_files).unwrap();
    assert_eq!(answer.given().secrets, today);
    assert_eq!(answer.given().secrets.environment, b"API_KEY=sk-1\nLLM_KEY=sk-2\n");
    assert!(answer.host_key.is_none(), "a front without the CAs signs nothing");

    for (from, name, why) in [
        (Some(B), "alice", "on node a, not b"),
        (Some("10.232.0.130"), "alice", "isn't a node's address"),
        (Some("10.232.1.128"), "alice", "isn't a node's address"),
        (None, "alice", "who is asking"),
        (Some("local"), "alice", "who is asking"),
        (Some(A), "carol", "no workspace carol"),
    ] {
        let (status, answer) = ask(&s.f, from, name, VERSION).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{from:?} {name}");
        assert!(answer["error"].as_str().unwrap().contains(why), "{from:?} {name}: {answer}");
    }
    let (status, answer) = ask(&s.f, Some(A), "alice", "0.0.1").await;
    assert_eq!(status, StatusCode::CONFLICT, "another version: {answer}");
    let (status, _) = ask(&s.f, Some(A), "Alice", VERSION).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// The node on the front's own machine asks through the socket.
#[tokio::test]
async fn the_local_node_asks_through_the_socket() {
    let s = setup();
    let mut settings = s.f.settings.clone();
    settings.local_node = "a".into();
    let f = Arc::new(fleet(settings, &[&s.a, &s.b]));
    assert_eq!(ask(&f, Some("local"), "alice", VERSION).await.0, StatusCode::OK);
}

/// A workspace moving away is started nowhere until its switch, then only
/// on its destination; one being made, nowhere.
#[tokio::test]
async fn nothing_starts_a_workspace_moving_away() {
    let s = setup();
    let m = Move {
        from: "a".into(),
        from_ip: "10.232.0.130".into(),
        to: "b".into(),
        ip: "10.232.1.128".into(),
        phase: Phase::Copy,
        ran: true,
    };
    let r = Record { moving: Some(m.clone()), ..record("alice", "a", "10.232.0.130") };
    s.f.registry.put(&r).unwrap();
    for from in [A, B] {
        let (status, answer) = ask(&s.f, Some(from), "alice", VERSION).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{from}: {answer}");
    }
    let switched =
        Record { node: "b".into(), ip: "10.232.1.128".into(), moving: Some(Move { phase: Phase::Start, ..m }), ..r };
    s.f.registry.put(&switched).unwrap();
    assert_eq!(ask(&s.f, Some(A), "alice", VERSION).await.0, StatusCode::FORBIDDEN, "the source, after the switch");
    let (status, answer) = ask(&s.f, Some(B), "alice", VERSION).await;
    assert_eq!(status, StatusCode::OK);
    let r = answer["record"].clone();
    assert!(r["move"].is_null() && r["ip"] == "10.232.1.128", "the node gets the record as it is to be: {r}");
    s.f.registry.put(&Record { pending: true, ..record("bob", "b", "10.232.1.129") }).unwrap();
    assert_eq!(ask(&s.f, Some(B), "bob", VERSION).await.0, StatusCode::FORBIDDEN, "one being made");
}

/// A declared workspace gets its secrets from where Nix says, its host key
/// signed now for its address, and no record: its config is Nix's.
#[tokio::test]
async fn a_declared_workspace_and_its_host_key() {
    let s = setup();
    let keygen = |args: &[&str]| {
        let out = std::process::Command::new("ssh-keygen").args(args).output().expect("ssh-keygen on the PATH");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let ca = s.dir.path().join("ca");
    std::fs::create_dir(&ca).unwrap();
    for name in ["host-ca", "user-ca"] {
        keygen(&["-q", "-t", "ed25519", "-N", "", "-f", &ca.join(name).to_string_lossy()]);
    }
    let mut settings = s.f.settings.clone();
    settings.ssh = crate::config::SshCa {
        keygen: "ssh-keygen".into(),
        host_ca: ca.join("host-ca").display().to_string(),
        user_ca: ca.join("user-ca.pub").display().to_string(),
        host_keys: s.dir.path().join("hosts").display().to_string(),
    };
    let f = Arc::new(fleet(settings, &[&s.a, &s.b]));
    let declared = std::path::PathBuf::from(&f.settings.declared);
    std::fs::create_dir_all(&declared).unwrap();
    let bob_secrets = s.dir.path().join("bob-secrets");
    std::fs::create_dir_all(&bob_secrets).unwrap();
    std::fs::write(bob_secrets.join("environment"), "BOB=1\n").unwrap();
    let nix =
        Record { declared: true, secrets_dir: bob_secrets.display().to_string(), ..record("bob", "b", "10.232.1.10") };
    std::fs::write(declared.join("bob.json"), serde_json::to_vec(&nix).unwrap()).unwrap();
    let (status, answer) = ask(&f, Some(B), "bob", VERSION).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let answer: StartAnswer = serde_json::from_value(answer).unwrap();
    assert!(answer.record.is_none());
    let given = answer.given();
    assert_eq!(given.secrets.environment, b"BOB=1\n");
    assert_eq!(given.user_ca.unwrap(), std::fs::read(ca.join("user-ca.pub")).unwrap());
    let key = given.host_key.unwrap();
    let cert = s.dir.path().join("cert.pub");
    std::fs::write(&cert, &key.certificate).unwrap();
    let shown = keygen(&["-L", "-f", &cert.to_string_lossy()]);
    assert!(shown.contains("host certificate") && shown.contains("bob.ws.test"), "{shown}");
    let (_, again) = ask(&f, Some(B), "bob", VERSION).await;
    let again: StartAnswer = serde_json::from_value(again).unwrap();
    assert_eq!(again.given().host_key.unwrap().public, key.public, "the same host key at every start");
    assert_eq!(ask(&f, Some(A), "bob", VERSION).await.0, StatusCode::FORBIDDEN, "Nix puts bob on b");
}

/// The list: owner, address and node from the registry, never a node's
/// word; the state from the node, `moving` and `unreachable` the front's.
#[tokio::test]
async fn the_list_of_a_fleet() {
    let s = setup();
    let alice = record("alice", "a", "10.232.0.130");
    s.a.has(&alice, true, b"disk");
    let bob = Record { apps: vec![], ..record("bob", "b", "10.232.1.128") };
    s.f.registry.put(&bob).unwrap();
    s.b.has(&bob, true, b"disk");
    // A disk only a node has: the admins'.
    s.b.has(&record("old", "b", "10.232.1.129"), false, b"disk");
    let all = s.f.list().await.unwrap();
    let names: Vec<_> = all.iter().map(|w| (w.name.as_str(), w.node.as_str(), w.configured)).collect();
    assert_eq!(names, [("alice", "a", true), ("bob", "b", true), ("old", "b", false)]);
    let first = &all[0];
    assert_eq!(
        (first.owner.as_str(), first.url.as_str(), first.state),
        ("alice", "https://alice.ws.test", State::Running)
    );
    assert!(first.self_service && first.max_memory_bytes == 8 << 30);
    assert_eq!(first.apps.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), ["t3code"]);
    assert_eq!(first.apps[0].url.as_deref(), Some("https://alice--t3code.ws.test"));
    assert!(
        first.changes.contains(&Change::AppAdded { app: "t3code".into() }),
        "the VM booted without it: {:?}",
        first.changes
    );
    assert!(all[2].owner.is_empty(), "a node's word for an owner is never taken");

    s.a.state.lock().unwrap().unreachable = true;
    let moved = Move {
        from: "b".into(),
        from_ip: bob.ip.clone(),
        to: "a".into(),
        ip: "10.232.0.131".into(),
        ..Move::default()
    };
    s.f.registry.put(&Record { moving: Some(moved), ..bob }).unwrap();
    let all = s.f.list().await.unwrap();
    assert_eq!((all[0].state, all[1].state), (State::Unreachable, State::Moving));
    assert_eq!(all[0].owner, "alice", "an unreachable one is still its owner's");
    let err = s.f.control("alice", workbench_model::Verb::Stop).await.unwrap_err();
    let refusal = err.downcast_ref::<workbench_model::fleet::NodeRefusal>();
    assert_eq!(refusal, Some(&workbench_model::fleet::NodeRefusal::Unreachable { node: "a".into() }), "{err:#}");
    assert_eq!(refusal.unwrap().told(false), "Its node doesn't answer just now. Try again later.", "no node's name");
    assert!(s.f.open_link("bob", "t3code").await.is_err(), "moving");

    let config = s.f.config("alice").unwrap();
    assert_eq!(
        (config.owner.as_str(), config.ip.as_str(), config.apps.len(), config.self_service),
        ("alice", "10.232.0.130", 1, true)
    );
    let views = s.f.nodes_view().await;
    assert_eq!(views.iter().map(|v| v.state).collect::<Vec<_>>(), [NodeState::Unreachable, NodeState::Up]);
}
