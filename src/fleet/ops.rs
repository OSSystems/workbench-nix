//! What the front's units do, as root, through the registry and the nodes:
//! `workbench self-service create`, `destroy`, `apps`, `memory` and
//! `reconcile` on a front, `workbench front setup` at boot and after a
//! deploy, `workbench forget` and `workbench front import`. The web UI
//! writes the requests and starts the units.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use vmkit::hostutil;
use vmkit::memory::{GIB, MIB};
use workbench_model::fleet::{MakeRequest, Record};
use workbench_model::memory::Ceiling;
use workbench_model::self_service::{AppsRequest, Limits, MemoryRequest, is_reserved, quota_of};
use workbench_model::{State as WsState, Verb, is_workspace_name};

use super::FrontSelfService;
use super::client::CallError;
use super::front::{Fleet, node_error};
use super::placement;
use super::registry::free_address;
use crate::config::Config;
use crate::self_service::{
    self, Owner, Refused, left_marker, read_quotas, read_theirs, record_of, refuse, remove, reporting,
};

/// What the reconciliation leaves in a left owner's mark when it stopped
/// their workspace, which starts again when they come back.
const STOPPED_BY_US: &str = "stopped\n";

impl Fleet {
    fn self_service(&self) -> anyhow::Result<&FrontSelfService> {
        self.settings.self_service.as_ref().context("this front doesn't let people make workspaces")
    }

    /// A workspace made at run time's secrets directory, on the front.
    fn secrets_dir(&self, name: &str) -> anyhow::Result<PathBuf> {
        Ok(Path::new(&self.self_service()?.secrets_root).join(name))
    }

    /// Its host key, which goes with it.
    pub(crate) fn host_key_dir(&self, name: &str) -> Option<PathBuf> {
        (!self.settings.ssh.is_empty()).then(|| Path::new(&self.settings.ssh.host_keys).join(name))
    }

    /// The limits, with `auto` the largest node's memory: the nodes are
    /// asked only then.
    pub async fn limits(&self) -> anyhow::Result<Limits> {
        let limits = &self.self_service()?.limits;
        Ok(if limits.memory_auto() { limits.resolved(&self.nodes_view().await) } else { limits.clone() })
    }

    /// Under the lock: cleans up what a create or a destroy that stopped
    /// half way left: on its node, then on the front. One whose node
    /// doesn't answer stays, its name and address taken, and is tried
    /// again by the next.
    pub async fn finish_pending(&self) {
        let (records, _) = self.registry.all();
        for r in records.into_iter().filter(|r| r.pending) {
            if let Err(e) = self.clean_up(&r).await {
                eprintln!("{}: cleaning up after a create or a destroy: {e:#}", r.name);
            }
        }
    }

    /// Deletes a workspace made at run time everywhere: on its node, its
    /// secrets and host key on the front, then its record.
    async fn clean_up(&self, r: &Record) -> anyhow::Result<()> {
        ensure!(!r.declared, "{} is declared in Nix", r.name);
        match self.node(&r.node)?.delete(&r.name).await {
            Ok(()) => {}
            Err(e) if e.status() == Some(404) => {}
            Err(e) => return Err(node_error(&r.node, e)),
        }
        for p in [
            Some(self.secrets_dir(&r.name)?),
            self.host_key_dir(&r.name),
            Some(left_marker(&self.state_root(), &r.name)),
        ]
        .into_iter()
        .flatten()
        {
            remove(&p).with_context(|| p.display().to_string())?;
        }
        self.registry.remove(&r.name)
    }

    /// `workbench self-service create <ws>` on a front: makes the workspace
    /// the web UI asked for on a node, and writes how it went.
    pub async fn create(&self, name: &str) -> anyhow::Result<()> {
        ensure!(is_workspace_name(name), "invalid workspace name {name:?}");
        let paths = self.paths();
        let failed = "The front couldn't make it";
        reporting(&paths.outcome(name), Some(&paths.request(name)), failed, false, self.make(name)).await
    }

    async fn make(&self, name: &str) -> anyhow::Result<()> {
        let ss = self.self_service()?;
        let paths = self.paths();
        let r = self_service::read_request(&paths, name)?;
        // One look at the nodes, for the placement and for `auto`.
        let views = self.nodes_view().await;
        let limits = ss.limits.resolved(&views);
        r.check(&limits).map_err(refuse)?;

        let lock = self.registry.lock()?;
        self.finish_pending().await;
        // Fails closed: a record that doesn't load may hold any address.
        let (all, unreadable) = self.registry.all();
        ensure!(unreadable.is_empty(), "records the front can't read: {}", unreadable.join("; "));
        let secrets = self.secrets_dir(name)?;
        let leftover = [Some(secrets.clone()), self.host_key_dir(name)];
        if all.iter().any(|c| c.name == name) || leftover.iter().flatten().any(|p| p.exists()) {
            return Err(self_service::taken(name));
        }
        let quota = quota_of(&read_quotas(&paths.quotas())?, ss.limits.quota, &r.owner);
        let owned = u32::try_from(all.iter().filter(|c| !c.declared && c.owner == r.owner).count()).unwrap_or(u32::MAX);
        self_service::check_quota(owned, quota)?;
        let env = self_service::credential(&ss.owner_command, &ss.environment_command, &r.owner).await?;
        let needs = placement::Needs {
            boot: limits.boot_mem_mib * MIB,
            disk: u64::try_from(r.disk_gib).unwrap_or(0) << 30,
            ceiling: match r.max_memory {
                Ceiling::Size(gib) => Some(gib * GIB),
                Ceiling::Auto => None,
            },
        };
        let node = placement::place(&views, needs, &r.node).map_err(refuse)?;
        let ip = free_address(&all, &self.settings.node(&node)?.subnet)?;
        let record = Record { pending: true, ..record_of(&r, &node, &ip) };
        self.registry.put(&record)?;
        let made = async {
            fs::create_dir_all(&secrets)?;
            fs::set_permissions(&secrets, fs::Permissions::from_mode(0o700))?;
            hostutil::atomic_write(secrets.join("environment"), env.as_bytes(), 0o600)?;
            self.node(&node)?
                .make(&MakeRequest { record: record.clone(), for_move: false })
                .await
                .map_err(|e| node_error(&node, e))
        }
        .await;
        if let Err(e) = made {
            if let Err(again) = self.clean_up(&record).await {
                eprintln!("{name}: cleaning up after a failed create: {again:#}");
            }
            return Err(e);
        }
        self.registry.put(&Record { pending: false, ..record })?;
        drop(lock);
        self.node(&node)?.control(name, Verb::Start, false).await.map_err(|e| node_error(&node, e))
    }

    /// `workbench self-service destroy <ws>` on a front: stops a workspace
    /// made at run time and deletes it on its node and the front.
    pub async fn destroy(&self, name: &str) -> anyhow::Result<()> {
        ensure!(is_workspace_name(name), "invalid workspace name {name:?}");
        let paths = self.paths();
        reporting(&paths.outcome(name), None, "The front couldn't destroy it", true, self.take_away(name)).await
    }

    /// Stops a workspace made at run time and deletes it on its node and
    /// the front: the web UI's Destroy, and `workbench destroy` on the
    /// front.
    pub(crate) async fn take_away(&self, name: &str) -> anyhow::Result<()> {
        let _lock = self.registry.lock()?;
        self.finish_pending().await;
        let r = self.made_at_run_time(name, "only the Nix configuration removes it")?;
        // Nothing is marked for deletion while its node can't be asked.
        self.node(&r.node)?.host().await.map_err(|e| node_error(&r.node, e))?;
        self.registry.put(&Record { pending: true, ..r.clone() })?;
        self.clean_up(&r).await?;
        remove(&self.paths().outcome(name))?;
        Ok(())
    }

    /// A workspace made at run time that isn't moving.
    fn made_at_run_time(&self, name: &str, or_else: &str) -> anyhow::Result<Record> {
        let r = self.record(name)?;
        if r.declared {
            bail!(Refused(format!("{name} wasn't made at run time: {or_else}.")));
        }
        if r.is_moving() {
            return Err(super::front::moving(name));
        }
        Ok(r)
    }

    /// `workbench self-service apps <ws>` on a front: the record's apps
    /// become those asked for; the next start brings them.
    pub async fn apps(&self, name: &str) -> anyhow::Result<()> {
        ensure!(is_workspace_name(name), "invalid workspace name {name:?}");
        let paths = self.paths();
        let (outcome, request) = (paths.apps_outcome(name), paths.apps_request(name));
        let work = async {
            let raw = read_theirs(&paths.apps_request(name)).context("the request")?;
            let req: AppsRequest = serde_json::from_slice(&raw).context("the request")?;
            self.self_service()?.limits.check_apps(&req.apps).map_err(refuse)?;
            let _lock = self.registry.lock()?;
            self.finish_pending().await;
            let r = self.made_at_run_time(name, "its apps are in the Nix configuration")?;
            self.registry.put(&Record { apps: req.apps, ..r })
        };
        reporting(&outcome, Some(&request), "The front couldn't change them", false, work).await
    }

    /// `workbench self-service memory <ws>` on a front: the most memory the
    /// record grows to becomes what an admin asked for; the next start
    /// brings it. `auto` is the memory of the workspace's node.
    pub async fn memory(&self, name: &str) -> anyhow::Result<()> {
        ensure!(is_workspace_name(name), "invalid workspace name {name:?}");
        let paths = self.paths();
        let (outcome, request) = (paths.memory_outcome(name), paths.memory_request(name));
        let work = async {
            let raw = read_theirs(&paths.memory_request(name)).context("the request")?;
            let req: MemoryRequest = serde_json::from_slice(&raw).context("the request")?;
            let limits = self.limits().await?;
            limits.check_memory(req.max_memory).map_err(refuse)?;
            let _lock = self.registry.lock()?;
            self.finish_pending().await;
            let r = self.made_at_run_time(name, "its memory is in the Nix configuration")?;
            // The host's memory is its node's.
            if let Ceiling::Size(gib) = req.max_memory {
                let host = self.node(&r.node)?.host().await.map_err(|e| node_error(&r.node, e))?;
                if gib * GIB > host.mem_total {
                    let has = host.mem_total / GIB;
                    return Err(refuse(format!("Memory must be at most {has} GiB on node {}, its memory.", r.node)));
                }
            }
            self.registry.put(&Record { max_memory: req.max_memory, ..r })
        };
        reporting(&outcome, Some(&request), "The front couldn't change it", false, work).await
    }

    /// `workbench self-service reconcile` on a front: stops, through their
    /// nodes, the workspaces of owners the owner command no longer keeps,
    /// and starts again those it stopped of owners who came back.
    pub async fn reconcile(&self) -> anyhow::Result<()> {
        let Some(ss) = &self.settings.self_service else { return Ok(()) };
        if ss.owner_command.is_empty() {
            return Ok(());
        }
        let made = |all: Vec<Record>| all.into_iter().filter(|r| !r.declared && !r.pending && !r.is_moving());
        // Asked without the lock, which a slow directory would hold up.
        let owners: std::collections::BTreeSet<String> = made(self.registry.all().0).map(|r| r.owner).collect();
        let mut said = BTreeMap::new();
        for owner in owners {
            let o = self_service::owner_said(&ss.owner_command, &owner).await;
            said.insert(owner, o);
        }
        let _lock = self.registry.lock()?;
        let mut failed = Vec::new();
        for r in made(self.registry.all().0) {
            let owner = said.get(&r.owner).copied().unwrap_or(Owner::Unknown);
            if let Err(e) = self.settle(&r, owner).await {
                failed.push(format!("{}: {e:#}", r.name));
            }
        }
        ensure!(failed.is_empty(), "{}", failed.join("; "));
        Ok(())
    }

    /// Brings one workspace in line with what the owner command said. The
    /// node's `stop` leaves its stop marker, so a node that reboots keeps
    /// it stopped; the mark says whether this stopped it.
    async fn settle(&self, r: &Record, owner: Owner) -> anyhow::Result<()> {
        let marker = left_marker(&self.state_root(), &r.name);
        let node = self.node(&r.node)?;
        match owner {
            Owner::Unknown => eprintln!("{}: couldn't tell whether {} stays; left as it is", r.name, r.owner),
            Owner::Stays => {
                if marker.exists() {
                    let mark = fs::read_to_string(&marker).unwrap_or_default();
                    // An empty mark is one a host on its own wrote before it
                    // was a fleet: it stopped the VM without a stop marker,
                    // so it starts again unless its node has one.
                    let ours = mark == STOPPED_BY_US
                        || (mark.is_empty() && !self.stopped_on_node(r).await.map_err(|e| node_error(&r.node, e))?);
                    eprintln!("{}: {} is back", r.name, r.owner);
                    if ours {
                        node.control(&r.name, Verb::Start, false).await.map_err(|e| node_error(&r.node, e))?;
                    }
                    remove(&marker)?;
                }
            }
            Owner::Left => {
                if !marker.exists() {
                    fs::create_dir_all(marker.parent().context("marker")?)?;
                    fs::write(&marker, "")?;
                    eprintln!("{}: {} left; stopping it, disk kept", r.name, r.owner);
                }
                let all = node.workspaces().await.map_err(|e| node_error(&r.node, e))?;
                let running = all.iter().any(|w| w.workspace.name == r.name && w.workspace.state != WsState::Stopped);
                if running {
                    fs::write(&marker, STOPPED_BY_US)?;
                    node.control(&r.name, Verb::Stop, false).await.map_err(|e| node_error(&r.node, e))?;
                }
            }
        }
        Ok(())
    }

    /// Whether a workspace's node keeps it stopped, by its stop marker.
    async fn stopped_on_node(&self, r: &Record) -> Result<bool, CallError> {
        let all = self.node(&r.node).map_err(|e| CallError::Unreachable(format!("{e:#}")))?.workspaces().await?;
        Ok(all.iter().any(|w| w.workspace.name == r.name && w.stopped))
    }

    /// Whether a workspace's node answers that it has it no more.
    async fn gone_from_node(&self, r: &Record) -> bool {
        let Ok(node) = self.node(&r.node) else { return false };
        node.workspaces().await.is_ok_and(|all| !all.iter().any(|w| w.workspace.name == r.name))
    }

    /// `workbench front setup`, at boot and after every deploy: cleans up
    /// what stopped half way, and on a machine with both roles makes
    /// records of its node's workspaces made at run time before it was a
    /// fleet. It notes where each declared workspace's disk is the first
    /// time, follows an address Nix changes on the same node, and forgets
    /// one Nix removed once its node has it no more. Then it starts
    /// `workbench-move@` for each move under way and each declared
    /// workspace Nix now puts on another node.
    pub async fn setup(&self) -> anyhow::Result<()> {
        let to_move = {
            let _lock = self.registry.lock()?;
            if self.settings.self_service.is_some() {
                self.finish_pending().await;
            }
            if !self.settings.local_node.is_empty()
                && let Err(e) = self.import_local()
            {
                eprintln!("the local node's workspaces made at run time: {e:#}");
            }
            let (all, unreadable) = self.registry.all();
            for e in &unreadable {
                eprintln!("the registry: {e}");
            }
            let mut to_move = Vec::new();
            for r in all {
                if r.declared {
                    match (self.registry.placed(&r.name)?, self.registry.nix(&r.name)?) {
                        (None, _) => self.registry.put(&r)?,
                        // Nix moved it within its node: the record follows.
                        (Some(p), Some(n)) if p.moving.is_none() && n.node == p.node && n.ip != p.ip => {
                            self.registry.put(&Record { ip: n.ip.clone(), ..r.clone() })?;
                        }
                        // Nix removed it, and its node destroyed its disk.
                        (Some(p), None) if p.moving.is_none() && self.gone_from_node(&r).await => {
                            self.registry.remove(&r.name)?;
                            continue;
                        }
                        _ => {}
                    }
                }
                let elsewhere = r.declared && self.registry.nix(&r.name)?.is_some_and(|n| n.node != r.node);
                if r.is_moving() || elsewhere {
                    to_move.push(r.name);
                }
            }
            to_move
        };
        for name in to_move {
            self_service::run_tool("systemctl", &["--no-block", "start", &format!("workbench-move@{name}.service")])
                .await?;
        }
        Ok(())
    }

    /// `workbench forget <ws>`: frees the name of a workspace whose node is
    /// lost for good, and its secrets and host key on the front. Refused
    /// while its node still has it.
    pub async fn forget(&self, name: &str) -> anyhow::Result<()> {
        ensure!(!is_reserved(name), "{name} is one of the state directory's own names, not a workspace");
        let _lock = self.registry.lock()?;
        let r = self.registry.get(name)?.with_context(|| format!("the registry has no {name}"))?;
        if let Ok(all) = self.node(&r.node)?.workspaces().await
            && all.iter().any(|w| w.workspace.name == name)
        {
            bail!(Refused(format!("Node {} still has {name}: destroy it rather than forget it.", r.node)));
        }
        if !r.declared {
            for p in [Some(self.secrets_dir(name)?), self.host_key_dir(name)].into_iter().flatten() {
                remove(&p).with_context(|| p.display().to_string())?;
            }
        }
        remove(&left_marker(&self.state_root(), name))?;
        self.registry.remove(name)
    }

    /// `workbench front import <node> <dir>`: the configs a host on its own
    /// made at run time become records on `node`, once, when that host
    /// becomes the fleet's node.
    pub fn import(&self, node: &str, dir: &Path) -> anyhow::Result<Vec<String>> {
        // Their secrets and starts are selfService's.
        self.self_service()?;
        self.settings.node(node)?;
        let _lock = self.registry.lock()?;
        let (configs, unreadable) = self_service::every_config(&[dir.to_owned()]);
        ensure!(unreadable.is_empty(), "configs that don't load: {}", unreadable.join("; "));
        self.import_configs(node, configs)
    }

    /// Under the lock: the local node's configs made at run time that the
    /// registry lacks, such as those of a host on its own from before it
    /// was a fleet, become records on it. Those that don't load are
    /// said, and left.
    fn import_local(&self) -> anyhow::Result<Vec<String>> {
        let node = &self.settings.local_node;
        let (configs, unreadable) = self_service::every_config(&[self.state_root().join("configs")]);
        for e in &unreadable {
            eprintln!("{e}");
        }
        let made: Vec<Config> = configs.into_iter().filter(|c| c.self_service).collect();
        if made.is_empty() {
            return Ok(vec![]);
        }
        self.self_service()?;
        let done = self.import_configs(node, made)?;
        for name in &done {
            eprintln!("{name}: a record on {node}, from its config");
        }
        Ok(done)
    }

    /// Under the lock: records of the configs made at run time the
    /// registry doesn't have.
    fn import_configs(&self, node: &str, configs: Vec<Config>) -> anyhow::Result<Vec<String>> {
        let mut done = Vec::new();
        for c in configs.into_iter().filter(|c| c.self_service) {
            if self.registry.get(&c.name)?.is_some() {
                eprintln!("{}: the registry has it already", c.name);
                continue;
            }
            self.registry.put(&self_service::record_of_config(&c, node))?;
            done.push(c.name);
        }
        Ok(done)
    }
}

#[cfg(test)]
mod tests;
