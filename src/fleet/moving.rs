//! Moving a workspace from one node to another: `workbench move <ws>
//! <node>` or the host page's Move ask for it, `workbench-move@<ws>` runs
//! it as root on the front, in phases the registry keeps:
//!
//! 1. **Reserve** an address in the destination's subnet, and make the
//!    workspace there without a disk.
//! 2. **Stop** it on the source, noting whether it ran.
//! 3. **Copy** its disk from the source to the destination, through the
//!    front; both sides' sha256 must be the stream's.
//! 4. **Switch** the record's node and address to the destination's, in
//!    one write.
//! 5. **Start** it on the destination if it ran, until its agent answers.
//! 6. **Clean** up: delete it on the source.
//!
//! A move that fails before the switch deletes the destination's copy and
//! starts the workspace on the source again if it ran; one that fails to
//! start switches back, then does the same. A move picked up after the
//! front stopped goes back if it hadn't switched, and on if it had.
//!
//! A declared workspace moves the same way when Nix puts it on another
//! node, to the address Nix gives it.

use std::fs;
use std::time::{Duration, Instant};

use anyhow::{Context, bail, ensure};
use tokio::sync::mpsc;
use workbench_model::fleet::{MakeRequest, Move, MoveRequest, Phase, Record};
use workbench_model::self_service::is_reserved;
use workbench_model::{State as WsState, Verb, is_workspace_name};

use super::client::{CallError, Node};
use super::disk;
use super::front::{Fleet, node_error};
use super::registry::free_address;
use crate::self_service::{Refused, read_theirs, refuse, reporting};

/// How long a moved workspace's agent may take to answer on its new node.
pub const START_WAIT: Duration = Duration::from_secs(600);
/// How often it is asked.
const START_POLL: Duration = Duration::from_secs(3);

impl Fleet {
    /// Asks for a workspace's move to `node`, as the web UI does: the
    /// request `workbench-move@<ws>` takes, which the UI starts next.
    pub fn ask_move(&self, name: &str, node: &str) -> anyhow::Result<()> {
        ensure!(is_workspace_name(name), "invalid workspace name {name:?}");
        ensure!(!is_reserved(name), "{name} is one of the state directory's own names, not a workspace");
        self.settings.node(node)?;
        let path = self.paths().move_request(name);
        fs::create_dir_all(path.parent().context("request")?)?;
        vmkit::hostutil::atomic_write(&path, &serde_json::to_vec(&MoveRequest { node: node.to_owned() })?, 0o640)?;
        Ok(())
    }

    /// `workbench self-service move <ws>`, `workbench-move@<ws>`'s: picks
    /// up the workspace's move under way, or starts the one an admin asked
    /// for or Nix declares, and writes how it went.
    pub async fn move_workspace(&self, name: &str) -> anyhow::Result<()> {
        ensure!(is_workspace_name(name), "invalid workspace name {name:?}");
        let paths = self.paths();
        let (outcome, request) = (paths.move_outcome(name), paths.move_request(name));
        reporting(&outcome, Some(&request), "The front couldn't move it", false, self.run_move(name)).await
    }

    async fn run_move(&self, name: &str) -> anyhow::Result<()> {
        let (mut r, fresh) = {
            let _lock = self.registry.lock()?;
            match self.begin(name).await? {
                Some(m) => m,
                None => return Ok(()),
            }
        };
        let m = r.moving.clone().context("no move")?;
        if !r.switched() {
            if !fresh {
                self.go_back(&r).await?;
                bail!("the move of {name} to {} stopped before its switch: it went back to {}", m.to, m.from);
            }
            if let Err(e) = self.forward(&mut r).await {
                if let Err(again) = self.go_back(&r).await {
                    bail!("{e:#}; going back to {} failed too: {again:#}", m.from);
                }
                return Err(e.context(format!("moving {name} to {}; it stays on {}", m.to, m.from)));
            }
        }
        if r.moving.as_ref().is_some_and(|m| m.phase <= Phase::Start) {
            if let Err(e) = self.start_there(&r).await {
                self.switch_back(&mut r)?;
                if let Err(again) = self.go_back(&r).await {
                    bail!("{e:#}; going back to {} failed too: {again:#}", m.from);
                }
                return Err(e.context(format!("starting {name} on {}; it went back to {}", m.to, m.from)));
            }
            self.phase(&mut r, Phase::Clean)?;
        }
        self.clean_source(&mut r).await
    }

    /// Under the lock: the move under way, or a new one, from the admin's
    /// request or from Nix; whether it is new.
    async fn begin(&self, name: &str) -> anyhow::Result<Option<(Record, bool)>> {
        let r = self.record(name)?;
        if r.is_moving() {
            return Ok(Some((r, false)));
        }
        let paths = self.paths();
        let (to, ip) = if paths.move_request(name).exists() {
            let raw = read_theirs(&paths.move_request(name)).context("the request")?;
            let req: MoveRequest = serde_json::from_slice(&raw).context("the request")?;
            if r.declared {
                bail!(Refused(format!("{name} is declared in Nix: change its node there and deploy.")));
            }
            if req.node == r.node {
                bail!(Refused(format!("{name} is on {} already.", r.node)));
            }
            let entry = self.settings.node(&req.node).map_err(|e| refuse(format!("{e:#}")))?;
            let (all, unreadable) = self.registry.all();
            ensure!(unreadable.is_empty(), "records the front can't read: {}", unreadable.join("; "));
            let ip = free_address(&all, &entry.subnet)?;
            (req.node, ip)
        } else if let Some(nix) = self.registry.nix(name)?.filter(|n| n.node != r.node) {
            (nix.node, nix.ip)
        } else {
            return Ok(None);
        };
        if super::is_closed(&self.state_root(), &to) {
            bail!(Refused(format!("Node {to} is closed: it takes no moves.")));
        }
        self.node(&to)?.host().await.map_err(|e| node_error(&to, e))?;
        let moving = Move { from: r.node.clone(), from_ip: r.ip.clone(), to, ip, phase: Phase::Reserve, ran: false };
        let r = Record { moving: Some(moving), ..r };
        self.registry.put(&r)?;
        Ok(Some((r, true)))
    }

    /// Writes the move's phase.
    fn phase(&self, r: &mut Record, phase: Phase) -> anyhow::Result<()> {
        if let Some(m) = &mut r.moving {
            m.phase = phase;
        }
        self.registry.put(r)
    }

    /// A record's move, and its source and destination.
    fn moving<'a>(&'a self, r: &Record) -> anyhow::Result<(Move, &'a dyn Node, &'a dyn Node)> {
        let m = r.moving.clone().context("no move")?;
        let (from, to) = (self.node(&m.from)?.as_ref(), self.node(&m.to)?.as_ref());
        Ok((m, from, to))
    }

    /// The phases up to the switch.
    async fn forward(&self, r: &mut Record) -> anyhow::Result<()> {
        let name = r.name.clone();
        let (m, from, to) = self.moving(r)?;
        let dest = Record { node: m.to.clone(), ip: m.ip.clone(), moving: None, ..r.clone() };
        to.make(&MakeRequest { record: dest, for_move: true }).await.map_err(|e| node_error(&m.to, e))?;

        let there = from.workspaces().await.map_err(|e| node_error(&m.from, e))?;
        let ws = there.iter().find(|w| w.workspace.name == name);
        // A declared one its deploy stopped runs where Nix puts it now,
        // unless someone stopped it.
        let ran = if r.declared {
            ws.is_some_and(|w| !w.stopped)
        } else {
            ws.is_some_and(|w| w.workspace.state != WsState::Stopped)
        };
        if let Some(mv) = &mut r.moving {
            mv.ran = ran;
        }
        self.phase(r, Phase::Stop)?;
        from.control(&name, Verb::Stop, true).await.map_err(|e| node_error(&m.from, e))?;
        let there = from.workspaces().await.map_err(|e| node_error(&m.from, e))?;
        if there.iter().any(|w| w.workspace.name == name && w.workspace.state != WsState::Stopped) {
            bail!("{name} didn't stop on {}", m.from);
        }

        self.phase(r, Phase::Copy)?;
        copy(&name, from, to, self.copy_idle)
            .await
            .with_context(|| format!("copying {name}'s disk from {} to {}", m.from, m.to))?;

        // The switch, in one write.
        r.node.clone_from(&m.to);
        r.ip.clone_from(&m.ip);
        self.phase(r, Phase::Start)
    }

    /// Starts it on the destination if it ran, and waits for its agent.
    async fn start_there(&self, r: &Record) -> anyhow::Result<()> {
        let (m, _, to) = self.moving(r)?;
        if !m.ran {
            return Ok(());
        }
        to.control(&r.name, Verb::Start, false).await.map_err(|e| node_error(&m.to, e))?;
        let deadline = Instant::now() + self.start_wait;
        loop {
            if let Ok(all) = to.workspaces().await
                && all.iter().any(|w| w.workspace.name == r.name && w.workspace.agent == "up")
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("{}'s agent didn't answer on {} within {}s", r.name, m.to, self.start_wait.as_secs());
            }
            tokio::time::sleep(START_POLL.min(self.start_wait)).await;
        }
    }

    /// After a failed start: the record goes back to the source.
    fn switch_back(&self, r: &mut Record) -> anyhow::Result<()> {
        let m = r.moving.clone().context("no move")?;
        r.node = m.from;
        r.ip = m.from_ip;
        self.registry.put(r)
    }

    /// Before the switch, or after switching back: the destination's copy
    /// goes, and the workspace starts on the source again if it ran.
    async fn go_back(&self, r: &Record) -> anyhow::Result<()> {
        let (m, from, to) = self.moving(r)?;
        ensure!(r.node == m.from, "{} has switched to {}: it goes on, not back", r.name, m.to);
        match to.delete(&r.name).await {
            Ok(()) => {}
            Err(e) if e.status() == Some(404) => {}
            Err(e) => return Err(node_error(&m.to, e)),
        }
        // The move ends before the source starts it, so its start isn't
        // refused as moving away, and a start that fails leaves it no
        // longer moving.
        self.registry.put(&Record { moving: None, ..r.clone() })?;
        if m.ran {
            match from.control(&r.name, Verb::Start, false).await {
                Ok(()) => {}
                // A declared one Nix no longer puts on the source: it runs
                // where Nix puts it, once a move gets it there.
                Err(e) if e.status() == Some(404) => {
                    eprintln!("{}: went back to {}, which doesn't run it: {e}", r.name, m.from);
                }
                Err(e) => {
                    return Err(node_error(&m.from, e).context(format!("{} went back to {}, stopped", r.name, m.from)));
                }
            }
        }
        Ok(())
    }

    /// The last phase: the source's copy goes, and the move with it.
    async fn clean_source(&self, r: &mut Record) -> anyhow::Result<()> {
        let (m, from, _) = self.moving(r)?;
        match from.delete(&r.name).await {
            Ok(()) => {}
            Err(e) if e.status() == Some(404) => {}
            Err(e) => return Err(node_error(&m.from, e)),
        }
        r.moving = None;
        self.registry.put(r)
    }
}

/// Streams a stopped workspace's disk from one node into another, through
/// the front, which checks the stream's sha256 and that the destination
/// took the same.
/// How long the destination may take to answer once the whole stream is
/// in: it syncs the disk first.
const ANSWER_WAIT: Duration = Duration::from_secs(600);

/// Streams a stopped workspace's disk from one node into another, through
/// the front, which checks the stream's sha256 and that the destination
/// took the same. A side silent for `idle` breaks it: a node that lost
/// power closes nothing.
async fn copy(name: &str, from: &dyn Node, to: &dyn Node, idle: Duration) -> anyhow::Result<()> {
    let mut src = from.disk_out(name).await.map_err(anyhow::Error::from)?;
    let (tx, rx) = mpsc::channel(8);
    let pump = async move {
        let mut tail = disk::Tail::default();
        loop {
            let piece = match tokio::time::timeout(idle, src.recv()).await {
                Err(_) => bail!("the source said nothing for {}s", idle.as_secs()),
                Ok(None) => break,
                Ok(Some(piece)) => piece.map_err(|e| anyhow::anyhow!("the source's stream broke: {e}"))?,
            };
            tail.push(&piece);
            match tokio::time::timeout(idle, tx.send(piece)).await {
                Err(_) => bail!("the destination took nothing for {}s", idle.as_secs()),
                Ok(Err(_)) => bail!("the destination stopped taking the stream"),
                Ok(Ok(())) => {}
            }
        }
        drop(tx);
        tail.finish()
    };
    let upload = to.disk_in(name, rx);
    tokio::pin!(pump, upload);
    // A pump that fails ends the upload with it.
    let (sent, took): ([u8; 32], Result<String, CallError>) = tokio::select! {
        sent = &mut pump => {
            let took = tokio::time::timeout(ANSWER_WAIT, &mut upload)
                .await
                .map_err(|_| anyhow::anyhow!("the destination didn't answer within {}s", ANSWER_WAIT.as_secs()))?;
            (sent?, took)
        }
        took = &mut upload => {
            // It refused, or took the end of the stream before the pump
            // saw it go.
            let took = took.map_err(anyhow::Error::from)?;
            let sent = tokio::time::timeout(idle, &mut pump)
                .await
                .map_err(|_| anyhow::anyhow!("the source said nothing for {}s", idle.as_secs()))??;
            (sent, Ok(took))
        }
    };
    let took = took.map_err(anyhow::Error::from)?;
    let sent = disk::hex(&sent);
    ensure!(sent == took, "the destination took {took}, the source sent {sent}");
    Ok(())
}

#[cfg(test)]
mod tests;
