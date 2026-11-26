//! The front's units against fake nodes: creating where there is room,
//! the pending records, destroying, changing a record, the owners'
//! reconciliation, forgetting and importing.

use std::path::Path;
use std::sync::Arc;

use workbench_model::fleet::Record;
use workbench_model::memory::Ceiling;
use workbench_model::self_service::{Outcome, Request};
use workbench_model::{State, Verb};

use crate::fleet::fake::{FakeNode, fleet};
use crate::fleet::front::Fleet;
use crate::fleet::registry::tests::record;
use crate::fleet::tests::front_settings;
use crate::self_service::{Refused, left_marker};

struct Setup {
    dir: tempfile::TempDir,
    a: Arc<FakeNode>,
    b: Arc<FakeNode>,
    f: Fleet,
}

/// A script that prints `out` and exits with `code`. A child writes it:
/// a descriptor this process held open to write it, inherited by a
/// command another test forks meanwhile, would make running the script
/// fail with ETXTBSY.
fn script(dir: &Path, name: &str, out: &str, code: i32) -> String {
    let path = dir.join(name);
    let written = std::process::Command::new("sh")
        .args(["-c", r#"printf '%s' "$1" > "$2" && chmod 755 "$2""#, "sh"])
        .arg(format!("#!/bin/sh\necho '{out}'\nexit {code}\n"))
        .arg(&path)
        .status()
        .unwrap();
    assert!(written.success(), "writing {}", path.display());
    path.display().to_string()
}

/// Node a has 10 GiB free, b 30.
fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let a = FakeNode::new("a", dir.path().join("a"), 14);
    let b = FakeNode::new("b", dir.path().join("b"), 34);
    let mut settings = front_settings(dir.path());
    let ss = settings.self_service.as_mut().unwrap();
    ss.environment_command = script(dir.path(), "env-of", "API_KEY=sk-1", 0);
    ss.limits.quota = 1;
    let f = fleet(settings, &[&a, &b]);
    Setup { dir, a, b, f }
}

fn request(f: &Fleet, name: &str, owner: &str, node: &str) {
    let r = Request {
        name: name.into(),
        owner: owner.into(),
        os: "ubuntu".into(),
        ssh_authorized_keys: vec!["ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMbJhk5H0h7O me@laptop".into()],
        max_memory: Ceiling::Size(8),
        disk_gib: 64,
        apps: vec!["t3code".into()],
        node: node.into(),
    };
    let path = f.paths().request(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec(&r).unwrap()).unwrap();
}

fn outcome(path: &Path) -> Outcome {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn refused(e: &anyhow::Error) -> String {
    let node = e.downcast_ref::<workbench_model::fleet::NodeRefusal>().filter(|n| !n.failed()).map(ToString::to_string);
    e.downcast_ref::<Refused>().map(|r| r.0.clone()).or(node).unwrap_or_else(|| panic!("not a refusal: {e:#}"))
}

#[tokio::test]
async fn a_workspace_is_made_where_there_is_room() {
    let s = setup();
    request(&s.f, "alice", "alice", "");
    s.f.create("alice").await.unwrap();
    let r = s.f.registry.get("alice").unwrap().unwrap();
    assert_eq!((r.node.as_str(), r.ip.as_str(), r.pending), ("b", "10.232.1.128", false), "b has the most free");
    assert_eq!(s.b.calls(), ["host", "make alice", "start alice"], "asked for its room, then to make it");
    assert!(s.a.calls().iter().all(|c| c == "host"), "{:?}", s.a.calls());
    let env = std::fs::read_to_string(s.dir.path().join("secrets/alice/environment")).unwrap();
    assert_eq!(env.trim(), "API_KEY=sk-1", "its secret environment is the front's");
    assert!(outcome(&s.f.paths().outcome("alice")).ok);
    assert!(!s.f.paths().request("alice").exists());
    // The quota counts what the registry has; the name is taken everywhere.
    request(&s.f, "alice-two", "alice", "");
    let e = s.f.create("alice-two").await.unwrap_err();
    assert!(refused(&e).contains("quota"));
    request(&s.f, "alice", "alice", "");
    assert!(refused(&s.f.create("alice").await.unwrap_err()).contains("taken"));
    // An admin's pick, which has room.
    request(&s.f, "bob", "bob", "a");
    s.f.create("bob").await.unwrap();
    assert_eq!(s.f.registry.get("bob").unwrap().unwrap().ip, "10.232.0.128");
    // None with room.
    s.a.state.lock().unwrap().host.mem_available = 5 << 30;
    s.b.state.lock().unwrap().host.mem_available = 5 << 30;
    request(&s.f, "carol", "carol", "");
    assert_eq!(refused(&s.f.create("carol").await.unwrap_err()), crate::fleet::placement::NO_ROOM);
    assert!(s.f.registry.get("carol").unwrap().is_none());
}

/// A create its node fails leaves nothing: on the node, its record, its
/// secrets.
#[tokio::test]
async fn a_failed_create_leaves_nothing() {
    let s = setup();
    s.b.fail("make");
    request(&s.f, "alice", "alice", "");
    let e = s.f.create("alice").await.unwrap_err();
    assert!(!outcome(&s.f.paths().outcome("alice")).ok, "{e:#}");
    assert!(s.f.registry.get("alice").unwrap().is_none());
    assert!(!s.dir.path().join("secrets/alice").exists());
    assert_eq!(s.b.calls(), ["host", "make alice", "delete alice"]);
}

/// A create or a destroy that stopped half way is cleaned up by the next,
/// on its node and the front; one whose node is down waits for it.
#[tokio::test]
async fn pending_records_are_cleaned_up() {
    let s = setup();
    let half = Record { pending: true, ..record("dave", "a", "10.232.0.128") };
    s.f.registry.put(&half).unwrap();
    s.a.has(&half, false, b"half");
    std::fs::create_dir_all(s.dir.path().join("secrets/dave")).unwrap();
    s.a.state.lock().unwrap().unreachable = true;
    s.f.finish_pending().await;
    assert!(s.f.registry.get("dave").unwrap().is_some(), "its name and address stay taken while a is down");
    s.a.state.lock().unwrap().unreachable = false;
    s.f.finish_pending().await;
    assert!(s.f.registry.get("dave").unwrap().is_none());
    assert!(s.a.ws("dave").is_none() && !s.dir.path().join("secrets/dave").exists());
    assert!(s.f.list().await.unwrap().is_empty(), "a pending record isn't listed");
}

#[tokio::test]
async fn destroying() {
    let s = setup();
    let r = record("alice", "a", "10.232.0.128");
    s.f.registry.put(&r).unwrap();
    s.a.has(&r, true, b"disk");
    std::fs::create_dir_all(s.dir.path().join("secrets/alice")).unwrap();
    s.a.state.lock().unwrap().unreachable = true;
    let e = s.f.destroy("alice").await.unwrap_err();
    assert!(refused(&e).contains("doesn't answer"));
    assert!(!s.f.registry.get("alice").unwrap().unwrap().pending, "nothing is marked while a is down");
    s.a.state.lock().unwrap().unreachable = false;
    s.f.destroy("alice").await.unwrap();
    assert!(s.f.registry.get("alice").unwrap().is_none() && s.a.ws("alice").is_none());
    assert!(!s.dir.path().join("secrets/alice").exists());
    let declared = std::path::PathBuf::from(&s.f.settings.declared);
    std::fs::create_dir_all(&declared).unwrap();
    std::fs::write(declared.join("bob.json"), serde_json::to_vec(&record("bob", "b", "10.232.1.10")).unwrap()).unwrap();
    assert!(refused(&s.f.destroy("bob").await.unwrap_err()).contains("Nix"));
}

/// Apps and memory change the record; the next start brings them.
#[tokio::test]
async fn changing_a_record() {
    let s = setup();
    s.f.registry.put(&record("alice", "a", "10.232.0.128")).unwrap();
    let p = s.f.paths();
    std::fs::create_dir_all(p.apps_request("alice").parent().unwrap()).unwrap();
    std::fs::write(p.apps_request("alice"), r#"{"apps":[]}"#).unwrap();
    s.f.apps("alice").await.unwrap();
    assert!(s.f.registry.get("alice").unwrap().unwrap().apps.is_empty());
    std::fs::write(p.apps_request("alice"), r#"{"apps":["herdr"]}"#).unwrap();
    assert!(refused(&s.f.apps("alice").await.unwrap_err()).contains("offers"));
    std::fs::write(p.memory_request("alice"), r#"{"maxMemoryGiB":16}"#).unwrap();
    s.f.memory("alice").await.unwrap();
    assert_eq!(s.f.registry.get("alice").unwrap().unwrap().max_memory, Ceiling::Size(16));
    std::fs::write(p.memory_request("alice"), r#"{"maxMemoryGiB":"auto"}"#).unwrap();
    assert!(refused(&s.f.memory("alice").await.unwrap_err()).contains("no auto"));
    assert!(!s.a.calls().iter().any(|c| c != "host"), "the node is asked to do nothing: {:?}", s.a.calls());
    assert!(outcome(&p.memory_outcome("alice")).error.contains("no auto"));
}

/// An owner who left has their workspaces stopped through their nodes,
/// and those it stopped start again when they come back.
#[tokio::test]
async fn owners_who_leave_and_come_back() {
    let mut s = setup();
    let (running, stopped) = (record("alice", "a", "10.232.0.128"), record("alice-two", "b", "10.232.1.128"));
    for (r, node, up) in [(&running, &s.a, true), (&stopped, &s.b, false)] {
        s.f.registry.put(r).unwrap();
        node.has(r, up, b"disk");
    }
    let ss = s.f.settings.self_service.as_mut().unwrap();
    ss.owner_command = script(s.dir.path(), "owner", "", 1);
    s.f.reconcile().await.unwrap();
    assert_eq!(s.a.ws("alice").unwrap().state, State::Stopped);
    assert!(!s.b.calls().iter().any(|c| c.starts_with("stop")), "one stopped already is left");
    s.f.list().await.unwrap().iter().for_each(|w| assert!(w.owner_left, "{}", w.name));
    let ss = s.f.settings.self_service.as_mut().unwrap();
    ss.owner_command = script(s.dir.path(), "owner", "", 0);
    s.f.reconcile().await.unwrap();
    assert_eq!(s.a.ws("alice").unwrap().state, State::Running, "the one it stopped starts again");
    assert_eq!(s.b.ws("alice-two").unwrap().state, State::Stopped, "the one they stopped stays stopped");
    assert!(!s.f.list().await.unwrap().iter().any(|w| w.owner_left));
    let ss = s.f.settings.self_service.as_mut().unwrap();
    ss.owner_command = script(s.dir.path(), "owner", "", 2);
    s.f.reconcile().await.unwrap();
    assert_eq!(s.a.ws("alice").unwrap().state, State::Running, "a directory that is down decides nothing");
}

#[tokio::test]
async fn forgetting_a_lost_node_s_workspace() {
    let s = setup();
    let r = record("alice", "a", "10.232.0.128");
    s.f.registry.put(&r).unwrap();
    s.a.has(&r, false, b"disk");
    std::fs::create_dir_all(s.dir.path().join("secrets/alice")).unwrap();
    assert!(refused(&s.f.forget("alice").await.unwrap_err()).contains("still has"));
    s.a.state.lock().unwrap().unreachable = true;
    s.f.forget("alice").await.unwrap();
    assert!(s.f.registry.get("alice").unwrap().is_none() && !s.dir.path().join("secrets/alice").exists());
    request(&s.f, "alice", "alice", "b");
    s.f.create("alice").await.unwrap();
    assert_eq!(s.f.registry.get("alice").unwrap().unwrap().node, "b", "its name is free again");
    s.f.control("alice", Verb::Restart).await.unwrap();
    assert_eq!(s.b.calls().last().unwrap(), "restart alice");
}

/// A host on its own's configs made at run time become records on the node
/// it becomes.
#[tokio::test]
async fn importing_a_host_s_configs() {
    let s = setup();
    let configs = s.dir.path().join("configs");
    std::fs::create_dir_all(&configs).unwrap();
    let t = crate::self_service::tests::template();
    let c = crate::self_service::config_of(
        &t,
        &crate::self_service::request_of(&record("alice", "a", "10.232.0.130")),
        "10.232.0.130",
        60_600,
    )
    .unwrap();
    std::fs::write(configs.join("alice.json"), serde_json::to_vec(&c).unwrap()).unwrap();
    let declared = crate::config::Config { name: "bob".into(), self_service: false, ..c.clone() };
    std::fs::write(configs.join("bob.json"), serde_json::to_vec(&declared).unwrap()).unwrap();
    assert_eq!(s.f.import("a", &configs).unwrap(), ["alice"]);
    let r = s.f.registry.get("alice").unwrap().unwrap();
    assert_eq!(r, record("alice", "a", "10.232.0.130"), "the record is the config's");
    assert!(s.f.import("a", &configs).unwrap().is_empty(), "once");
    assert!(s.f.import("c", &configs).is_err());
}

/// Where the most memory is `auto`, a workspace may grow to its own node's
/// memory and no more: placed where that fits, changed within it.
#[tokio::test]
async fn the_most_memory_is_the_node_s() {
    let mut s = setup();
    s.f.settings.self_service.as_mut().unwrap().limits.max_memory = Ceiling::Auto;
    s.a.state.lock().unwrap().host.mem_total = 32 << 30;
    s.b.state.lock().unwrap().host.mem_total = 128 << 30;
    // a has more free now, but not the memory to grow to 100 GiB.
    s.a.state.lock().unwrap().host.mem_available = 30 << 30;
    s.b.state.lock().unwrap().host.mem_available = 20 << 30;
    let path = s.f.paths().request("alice");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut r: Request = {
        request(&s.f, "alice", "alice", "");
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
    };
    r.max_memory = Ceiling::Size(100);
    std::fs::write(&path, serde_json::to_vec(&r).unwrap()).unwrap();
    s.f.create("alice").await.unwrap();
    assert_eq!(s.f.registry.get("alice").unwrap().unwrap().node, "b");
    s.f.registry.put(&record("bob", "a", "10.232.0.128")).unwrap();
    let p = s.f.paths();
    std::fs::write(p.memory_request("bob"), r#"{"maxMemoryGiB":100}"#).unwrap();
    let e = s.f.memory("bob").await.unwrap_err();
    assert!(refused(&e).contains("at most 32 GiB on node a"), "{e:#}");
    std::fs::write(p.memory_request("bob"), r#"{"maxMemoryGiB":24}"#).unwrap();
    s.f.memory("bob").await.unwrap();
    assert_eq!(s.f.registry.get("bob").unwrap().unwrap().max_memory, Ceiling::Size(24));
}

/// A config made at run time as a host on its own made it, before it was
/// a fleet: on `ip`, for alice.
fn config_from_before(name: &str, ip: &str) -> crate::config::Config {
    let t = crate::self_service::tests::template();
    let req = crate::self_service::request_of(&record(name, "a", ip));
    crate::self_service::config_of(&t, &req, ip, 60_600).unwrap()
}

/// On a machine with both roles, `front setup` makes records of its node's
/// workspaces made at run time that the registry lacks, as `front import`
/// would, once; the others stay as they are.
#[tokio::test]
async fn setup_imports_the_local_node_s_workspaces() {
    let mut s = setup();
    s.f.settings.local_node = "a".into();
    let configs = s.f.state_root().join("configs");
    std::fs::create_dir_all(&configs).unwrap();
    let old = config_from_before("alice", "10.232.0.130");
    std::fs::write(configs.join("alice.json"), serde_json::to_vec(&old).unwrap()).unwrap();
    let known = record("alice-two", "a", "10.232.0.131");
    s.f.registry.put(&known).unwrap();
    let mine = config_from_before("alice-two", "10.232.0.131");
    std::fs::write(configs.join("alice-two.json"), serde_json::to_vec(&mine).unwrap()).unwrap();
    std::fs::write(configs.join("broken.json"), b"{").unwrap();
    s.f.setup().await.unwrap();
    assert_eq!(s.f.registry.get("alice").unwrap().unwrap(), record("alice", "a", "10.232.0.130"), "its config's");
    assert_eq!(s.f.registry.get("alice-two").unwrap().unwrap(), known, "a record it has stays");
    s.f.setup().await.unwrap();
    assert_eq!(s.f.registry.all().0.len(), 2, "once");
    // A front with no node here imports nothing.
    let other = setup();
    let configs = other.f.state_root().join("configs");
    std::fs::create_dir_all(&configs).unwrap();
    std::fs::write(configs.join("alice.json"), serde_json::to_vec(&old).unwrap()).unwrap();
    other.f.setup().await.unwrap();
    assert!(other.f.registry.get("alice").unwrap().is_none());
}

/// A declared workspace's record follows an address Nix changes on the
/// same node, and goes once Nix removed it and its node destroyed it.
#[tokio::test]
async fn setup_follows_what_nix_declares() {
    let s = setup();
    let declared = std::path::PathBuf::from(&s.f.settings.declared);
    std::fs::create_dir_all(&declared).unwrap();
    let declare =
        |r: &Record| std::fs::write(declared.join(format!("{}.json", r.name)), serde_json::to_vec(r).unwrap());
    let bob = Record { declared: true, ..record("bob", "b", "10.232.1.10") };
    declare(&bob).unwrap();
    s.b.has(&bob, true, b"disk");
    s.f.setup().await.unwrap();
    assert_eq!(s.f.registry.placed("bob").unwrap().unwrap().ip, "10.232.1.10");
    declare(&Record { ip: "10.232.1.20".into(), ..bob.clone() }).unwrap();
    s.f.setup().await.unwrap();
    assert_eq!(s.f.registry.get("bob").unwrap().unwrap().ip, "10.232.1.20", "its new address, on the same node");

    std::fs::remove_file(declared.join("bob.json")).unwrap();
    s.f.setup().await.unwrap();
    assert!(s.f.registry.get("bob").unwrap().is_some(), "its node still has its disk");
    s.b.state.lock().unwrap().workspaces.remove("bob");
    s.b.state.lock().unwrap().unreachable = true;
    s.f.setup().await.unwrap();
    assert!(s.f.registry.get("bob").unwrap().is_some(), "nothing is forgotten while its node doesn't answer");
    s.b.state.lock().unwrap().unreachable = false;
    s.f.setup().await.unwrap();
    assert!(s.f.registry.get("bob").unwrap().is_none(), "its name is free again");
}

/// A mark a host on its own left before it was a fleet is empty: its
/// workspace starts again when its owner comes back, unless its node keeps
/// it stopped.
#[tokio::test]
async fn an_owner_back_after_the_upgrade() {
    let mut s = setup();
    let (left, kept) = (record("alice", "a", "10.232.0.128"), record("alice-two", "a", "10.232.0.129"));
    for r in [&left, &kept] {
        s.f.registry.put(r).unwrap();
        s.a.has(r, false, b"disk");
        let marker = left_marker(&s.f.state_root(), &r.name);
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(marker, "").unwrap();
    }
    // The host stopped alice's VM without a stop marker; alice-two's has one.
    s.a.state.lock().unwrap().workspaces.get_mut("alice").unwrap().stopped = false;
    s.f.settings.self_service.as_mut().unwrap().owner_command = script(s.dir.path(), "owner", "", 0);
    s.f.reconcile().await.unwrap();
    assert_eq!(s.a.ws("alice").unwrap().state, State::Running, "it starts again");
    assert_eq!(s.a.ws("alice-two").unwrap().state, State::Stopped, "its owner had stopped it");
    assert!(!left_marker(&s.f.state_root(), "alice").exists() && !left_marker(&s.f.state_root(), "alice-two").exists());
}

/// Imported workspaces' secrets and starts are selfService's: a front
/// without it imports none. And no command takes the state directory's own
/// names for a workspace.
#[tokio::test]
async fn imports_need_self_service_and_names_are_workspaces() {
    let mut s = setup();
    let configs = s.dir.path().join("configs");
    std::fs::create_dir_all(&configs).unwrap();
    std::fs::write(
        configs.join("alice.json"),
        serde_json::to_vec(&config_from_before("alice", "10.232.0.130")).unwrap(),
    )
    .unwrap();
    s.f.settings.self_service = None;
    assert!(s.f.import("a", &configs).unwrap_err().to_string().contains("doesn't let people make workspaces"));
    assert!(s.f.registry.get("alice").unwrap().is_none());
    assert!(s.f.forget("registry").await.unwrap_err().to_string().contains("state directory's own names"));
}

/// A create looks at the nodes once: its placement and its `auto` are
/// told from the same answers.
#[tokio::test]
async fn a_create_asks_each_node_once() {
    let mut s = setup();
    s.f.settings.self_service.as_mut().unwrap().limits.max_memory = Ceiling::Auto;
    request(&s.f, "alice", "alice", "");
    s.f.create("alice").await.unwrap();
    for n in [&s.a, &s.b] {
        assert_eq!(n.calls().iter().filter(|c| *c == "host").count(), 1, "{}: {:?}", n.name, n.calls());
    }
}
