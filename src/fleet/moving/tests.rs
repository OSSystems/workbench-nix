//! Moves against fake nodes: every phase, its failure and its resumption.

use std::sync::Arc;

use workbench_model::State;
use workbench_model::fleet::{Move, Phase, Record};

use crate::fleet::fake::{FakeNode, fleet};
use crate::fleet::front::Fleet;
use crate::fleet::registry::tests::record;
use crate::fleet::tests::front_settings;
use crate::self_service::Refused;

/// A disk with some data and a hole.
fn data() -> Vec<u8> {
    let mut d = vec![0u8; 3 << 20];
    d[..4096].fill(b'a');
    d[(2 << 20)..(2 << 20) + 10].fill(b'z');
    d
}

struct Setup {
    _dir: tempfile::TempDir,
    a: Arc<FakeNode>,
    b: Arc<FakeNode>,
    f: Fleet,
    r: Record,
}

/// alice's workspace runs on a.
fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let a = FakeNode::new("a", dir.path().join("a"), 30);
    let b = FakeNode::new("b", dir.path().join("b"), 30);
    let f = fleet(front_settings(dir.path()), &[&a, &b]);
    let r = record("alice", "a", "10.232.0.130");
    f.registry.put(&r).unwrap();
    a.has(&r, true, &data());
    *a.watch.lock().unwrap() = Some(f.registry.clone());
    Setup { _dir: dir, a, b, f, r }
}

fn outcome(f: &Fleet) -> workbench_model::self_service::Outcome {
    serde_json::from_slice(&std::fs::read(f.paths().move_outcome("alice")).unwrap()).unwrap()
}

#[tokio::test]
async fn a_move_goes_through_its_phases() {
    let s = setup();
    s.f.ask_move("alice", "b").unwrap();
    s.f.move_workspace("alice").await.unwrap();
    let r = s.f.registry.get("alice").unwrap().unwrap();
    assert_eq!((r.node.as_str(), r.ip.as_str(), r.moving), ("b", "10.232.1.128", None), "switched, done");
    assert_eq!(std::fs::read(s.b.disk("alice")).unwrap(), data(), "the disk travelled whole");
    assert!(s.a.ws("alice").is_none() && !s.a.disk("alice").exists(), "the source's copy is gone");
    let there = s.b.ws("alice").unwrap();
    assert!(there.state == State::Running && there.agent_up, "it ran, so it runs there");
    assert!(there.for_move && there.record.ip == "10.232.1.128" && there.record.node == "b");
    assert_eq!(s.a.calls(), ["workspaces", "stop alice", "workspaces", "disk_out alice", "delete alice"]);
    assert_eq!(s.b.calls(), ["host", "make alice", "disk_in alice", "start alice", "workspaces"]);
    assert!(outcome(&s.f).ok);
    assert!(!s.f.paths().move_request("alice").exists(), "the request is taken");
}

/// One that didn't run stays stopped on its new node.
#[tokio::test]
async fn a_stopped_workspace_moves_stopped() {
    let s = setup();
    s.a.has(&s.r, false, &data());
    s.f.ask_move("alice", "b").unwrap();
    s.f.move_workspace("alice").await.unwrap();
    assert_eq!(s.f.registry.get("alice").unwrap().unwrap().node, "b");
    assert_eq!(s.b.ws("alice").unwrap().state, State::Stopped);
    assert!(!s.b.calls().iter().any(|c| c.starts_with("start")));
}

/// A failure before the switch deletes the destination's copy and starts
/// the workspace on the source again.
#[tokio::test]
async fn a_failure_before_the_switch_goes_back() {
    for (node, what) in [("b", "make"), ("a", "stop"), ("a", "disk_out"), ("b", "disk_in"), ("a", "corrupt")] {
        let s = setup();
        if node == "a" {
            s.a.fail(what);
        } else {
            s.b.fail(what);
        }
        s.f.ask_move("alice", "b").unwrap();
        let e = s.f.move_workspace("alice").await.unwrap_err();
        let r = s.f.registry.get("alice").unwrap().unwrap();
        assert_eq!(r, s.r, "{what}: the record is as it was: {e:#}");
        assert!(s.b.ws("alice").is_none(), "{what}: the destination's copy stays");
        assert!(s.b.calls().contains(&"delete alice".to_owned()), "{what}");
        if what != "stop" {
            assert_eq!(s.a.ws("alice").unwrap().state, State::Running, "{what}: not started again on the source");
        }
        assert_eq!(std::fs::read(s.a.disk("alice")).unwrap(), data(), "{what}: the source's disk changed");
        assert!(!s.a.calls().iter().any(|c| c.starts_with("start while")), "{what}: started while still moving");
        let o = outcome(&s.f);
        assert!(!o.ok && o.error.contains("stays on a"), "{what}: {}", o.error);
    }
}

/// A move whose workspace doesn't come up on its destination switches
/// back, and the source runs it again.
#[tokio::test]
async fn a_failed_start_switches_back() {
    let s = setup();
    s.b.state.lock().unwrap().agents_answer = false;
    s.f.ask_move("alice", "b").unwrap();
    let e = s.f.move_workspace("alice").await.unwrap_err();
    assert!(format!("{e:#}").contains("went back to a"), "{e:#}");
    assert_eq!(s.f.registry.get("alice").unwrap().unwrap(), s.r);
    assert!(s.b.ws("alice").is_none());
    assert_eq!(s.a.ws("alice").unwrap().state, State::Running);
    assert_eq!(std::fs::read(s.a.disk("alice")).unwrap(), data());
    assert!(!s.a.calls().iter().any(|c| c.starts_with("start while")), "started while still moving");
}

/// A source that goes silent half way, as one that lost power does,
/// fails the move rather than holding it for good.
#[tokio::test]
async fn a_silent_source_fails_the_move() {
    let s = setup();
    s.a.fail("stall");
    s.f.ask_move("alice", "b").unwrap();
    let moved = tokio::time::timeout(std::time::Duration::from_secs(10), s.f.move_workspace("alice")).await;
    let e = moved.expect("the move waited for good").unwrap_err();
    assert!(format!("{e:#}").contains("said nothing"), "{e:#}");
    assert_eq!(s.f.registry.get("alice").unwrap().unwrap(), s.r);
    assert!(s.b.ws("alice").is_none());
}

/// A move that can't go back, its destination down, stays recorded; once
/// the destination answers, picking it up goes back.
#[tokio::test]
async fn a_failure_while_going_back_is_picked_up() {
    let s = setup();
    s.b.fail("disk_in");
    s.b.fail("delete");
    s.f.ask_move("alice", "b").unwrap();
    let e = s.f.move_workspace("alice").await.unwrap_err();
    assert!(format!("{e:#}").contains("going back to a failed too"), "{e:#}");
    let r = s.f.registry.get("alice").unwrap().unwrap();
    assert!(r.is_moving() && !r.switched(), "it stays moving, before the switch: {r:?}");
    assert_eq!(s.a.ws("alice").unwrap().state, State::Stopped, "not started while it moves");
    s.b.state.lock().unwrap().fail.clear();
    let e = s.f.move_workspace("alice").await.unwrap_err();
    assert!(format!("{e:#}").contains("before its switch"), "{e:#}");
    assert_eq!(s.f.registry.get("alice").unwrap().unwrap(), s.r);
    assert!(s.b.ws("alice").is_none());
    assert_eq!(s.a.ws("alice").unwrap().state, State::Running);
}

/// A declared workspace whose move fails goes back to a source that no
/// longer runs it: no longer moving, and moved again later.
#[tokio::test]
async fn a_declared_move_that_fails_goes_back() {
    let s = setup();
    s.f.registry.remove("alice").unwrap();
    let declared = std::path::PathBuf::from(&s.f.settings.declared);
    std::fs::create_dir_all(&declared).unwrap();
    let nix = Record { declared: true, ..record("bob", "b", "10.232.1.10") };
    std::fs::write(declared.join("bob.json"), serde_json::to_vec(&nix).unwrap()).unwrap();
    s.f.registry.put(&Record { node: "a".into(), ip: "10.232.0.10".into(), ..nix.clone() }).unwrap();
    s.a.has(&record("bob", "a", "10.232.0.10"), false, &data());
    {
        let mut st = s.a.state.lock().unwrap();
        let w = st.workspaces.get_mut("bob").unwrap();
        // Nix took its config away from a, and its deploy stopped it.
        (w.stopped, w.startable) = (false, false);
    }
    s.b.fail("disk_in");
    assert!(s.f.move_workspace("bob").await.is_err());
    let r = s.f.registry.get("bob").unwrap().unwrap();
    assert_eq!((r.node.as_str(), r.moving.is_some()), ("a", false), "not stuck moving: {r:?}");
    s.b.state.lock().unwrap().fail.clear();
    s.f.move_workspace("bob").await.unwrap();
    let r = s.f.registry.get("bob").unwrap().unwrap();
    assert_eq!((r.node.as_str(), r.ip.as_str(), r.moving), ("b", "10.232.1.10", None));
}

fn moving(phase: Phase, switched: bool) -> Record {
    let m = Move {
        from: "a".into(),
        from_ip: "10.232.0.130".into(),
        to: "b".into(),
        ip: "10.232.1.128".into(),
        phase,
        ran: true,
    };
    let r = record("alice", "a", "10.232.0.130");
    if switched {
        Record { node: "b".into(), ip: "10.232.1.128".into(), moving: Some(m), ..r }
    } else {
        Record { moving: Some(m), ..r }
    }
}

/// A front that stopped before the switch goes back when it picks the move
/// up, whichever phase it was in.
#[tokio::test]
async fn resumed_before_the_switch_it_goes_back() {
    for phase in [Phase::Reserve, Phase::Stop, Phase::Copy] {
        let s = setup();
        s.a.has(&s.r, false, &data());
        s.b.has(&record("alice", "b", "10.232.1.128"), false, b"half a disk");
        s.f.registry.put(&moving(phase, false)).unwrap();
        let e = s.f.move_workspace("alice").await.unwrap_err();
        assert!(format!("{e:#}").contains("before its switch"), "{phase:?}: {e:#}");
        assert_eq!(s.f.registry.get("alice").unwrap().unwrap(), s.r, "{phase:?}");
        assert!(s.b.ws("alice").is_none() && !s.b.disk("alice").exists(), "{phase:?}");
        assert_eq!(s.a.ws("alice").unwrap().state, State::Running, "{phase:?}: it ran");
    }
}

/// After the switch it goes on: from the start, or only the cleaning up.
#[tokio::test]
async fn resumed_after_the_switch_it_goes_on() {
    let s = setup();
    s.a.has(&s.r, false, &data());
    s.b.has(&record("alice", "b", "10.232.1.128"), false, &data());
    s.f.registry.put(&moving(Phase::Start, true)).unwrap();
    s.f.move_workspace("alice").await.unwrap();
    let r = s.f.registry.get("alice").unwrap().unwrap();
    assert_eq!((r.node.as_str(), r.moving), ("b", None));
    assert_eq!(s.b.ws("alice").unwrap().state, State::Running);
    assert!(s.a.ws("alice").is_none());

    let s = setup();
    s.a.has(&s.r, false, &data());
    s.b.has(&record("alice", "b", "10.232.1.128"), false, &data());
    s.f.registry.put(&moving(Phase::Clean, true)).unwrap();
    s.f.move_workspace("alice").await.unwrap();
    assert!(s.a.ws("alice").is_none() && s.a.calls() == ["delete alice"]);
    assert!(!s.b.calls().iter().any(|c| c.starts_with("start")), "a cleaning up starts nothing");
    assert_eq!(s.f.registry.get("alice").unwrap().unwrap().moving, None);
}

/// Nix puts a declared workspace on another node: it moves there, to the
/// address Nix gives it, and runs there unless someone stopped it.
#[tokio::test]
async fn a_declared_workspace_follows_nix() {
    let s = setup();
    s.f.registry.remove("alice").unwrap();
    let declared = std::path::PathBuf::from(&s.f.settings.declared);
    std::fs::create_dir_all(&declared).unwrap();
    let nix = Record { declared: true, ..record("bob", "b", "10.232.1.10") };
    std::fs::write(declared.join("bob.json"), serde_json::to_vec(&nix).unwrap()).unwrap();
    s.f.registry.put(&Record { node: "a".into(), ip: "10.232.0.10".into(), ..nix.clone() }).unwrap();
    // Its deploy stopped it on a, without a stop marker.
    s.a.has(&record("bob", "a", "10.232.0.10"), false, &data());
    s.a.state.lock().unwrap().workspaces.get_mut("bob").unwrap().stopped = false;
    s.f.move_workspace("bob").await.unwrap();
    let r = s.f.registry.get("bob").unwrap().unwrap();
    assert_eq!((r.node.as_str(), r.ip.as_str(), r.moving), ("b", "10.232.1.10", None));
    assert_eq!(s.b.ws("bob").unwrap().state, State::Running);
    assert_eq!(std::fs::read(s.b.disk("bob")).unwrap(), data());
    // Where Nix and the registry agree, there is nothing to move.
    s.f.move_workspace("bob").await.unwrap();
    assert_eq!(s.b.calls().iter().filter(|c| c.starts_with("make")).count(), 1);
    // And a declared one is never moved by asking.
    s.f.ask_move("bob", "a").unwrap();
    let e = s.f.move_workspace("bob").await.unwrap_err();
    assert!(e.downcast_ref::<Refused>().is_some_and(|r| r.0.contains("change its node")), "{e:#}");
}

#[tokio::test]
async fn moves_that_are_refused() {
    let s = setup();
    for (to, why) in [("a", "on a already"), ("c", "isn't a node")] {
        let path = s.f.paths().move_request("alice");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{{\"node\":\"{to}\"}}")).unwrap();
        let e = s.f.move_workspace("alice").await.unwrap_err();
        assert!(format!("{e:#}").contains(why), "{to}: {e:#}");
    }
    crate::fleet::set_closed(&s.f.state_root(), "b", true).unwrap();
    s.f.ask_move("alice", "b").unwrap();
    let e = s.f.move_workspace("alice").await.unwrap_err();
    assert!(format!("{e:#}").contains("closed"), "{e:#}");
    crate::fleet::set_closed(&s.f.state_root(), "b", false).unwrap();
    s.b.state.lock().unwrap().unreachable = true;
    s.f.ask_move("alice", "b").unwrap();
    assert!(s.f.move_workspace("alice").await.is_err(), "a node that doesn't answer takes no move");
    assert_eq!(s.f.registry.get("alice").unwrap().unwrap(), s.r);
    assert!(s.a.calls().is_empty(), "nothing was asked of the source");
    // While it moves, its actions are refused.
    s.f.registry.put(&moving(Phase::Copy, false)).unwrap();
    let e = s.f.control("alice", workbench_model::Verb::Stop).await.unwrap_err();
    assert!(e.downcast_ref::<Refused>().is_some_and(|r| r.0.contains("moving")), "{e:#}");
}
