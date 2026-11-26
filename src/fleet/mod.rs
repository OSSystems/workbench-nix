//! workbench on several machines: a front, which keeps the registry, the
//! secrets and the host keys and decides, and nodes, which run the
//! workspaces' VMs and keep their disks. `docs/multi-node.md` is the
//! specification; its "What the modules render" lists the files and
//! commands below.
//!
//! - [`node`]: `workbench node serve`, the node's API, which answers the
//!   front only.
//! - [`front`]: `workbench front serve`, the front's API, which answers
//!   a node's starts, and [`front::Fleet`], what the front does through
//!   the nodes: what the web UI and the front's units call.
//! - [`run`]: `workbench run` on a node, which asks the front.
//! - [`registry`], [`disk`], [`placement`], [`moving`]: the registry's
//!   files, the disk stream, where a new workspace goes, and moves.
//!
//! A machine without these settings runs as a host on its own did before
//! (until the modules render a fleet).

pub mod client;
pub mod contract;
pub mod disk;
mod http;
pub mod node;
pub mod registry;
pub mod run;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use vmkit::hostutil;
use workbench_model::apps::{AppInfo, BootApp, is_app_id};
use workbench_model::fleet::is_node_name;
use workbench_model::self_service::Limits;

use crate::config::{DEFAULT_CONFIG_DIRS, SshCa};

/// The build a front and its nodes must share: `WORKBENCH_BUILD` at build
/// time, an identifier of the source the package is built from, which the
/// Nix package sets; the crate's version where nothing sets it, as for a
/// plain `cargo build` and the tests. Its characters are a header's.
pub const VERSION: &str = match option_env!("WORKBENCH_BUILD") {
    Some(build) => build,
    None => env!("CARGO_PKG_VERSION"),
};

/// A node's settings, which the node module renders.
pub const NODE_SETTINGS: &str = "/etc/workbench-node.json";
/// The front's settings, which the front module renders.
pub const FRONT_SETTINGS: &str = "/etc/workbench-front.json";
/// The node's API, on a machine with both roles.
pub const NODE_SOCKET: &str = "/run/workbench-node/api.sock";
/// The front's API, on a machine with both roles.
pub const FRONT_SOCKET: &str = "/run/workbench-front/api.sock";
/// The node's shared consoles, for `workbench console` on the node.
pub const NODE_CONSOLES: &str = "/run/workbench-node/console.sock";

/// The largest settings file read.
const MAX_SETTINGS: u64 = 4 << 20;

/// Where an API listens or is reached: an address and port, or a Unix
/// socket, written as its absolute path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Endpoint {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

impl FromStr for Endpoint {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        if s.starts_with('/') {
            return Ok(Endpoint::Unix(PathBuf::from(s)));
        }
        s.parse().map(Endpoint::Tcp).map_err(|_| format!("{s:?} is neither <address>:<port> nor a socket's path"))
    }
}

impl TryFrom<String> for Endpoint {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        s.parse()
    }
}

impl From<Endpoint> for String {
    fn from(e: Endpoint) -> String {
        e.to_string()
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Endpoint::Tcp(a) => write!(f, "{a}"),
            Endpoint::Unix(p) => write!(f, "{}", p.display()),
        }
    }
}

/// A memory pool whose status the host page shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pool {
    pub name: String,
    /// Its controller's status file.
    pub status: String,
}

/// A node's settings, `/etc/workbench-node.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NodeSettings {
    /// Its name in the fleet.
    pub name: String,
    /// Where its API listens: its `WireGuard` address and port 7910.
    #[serde(default)]
    pub listen: Option<SocketAddr>,
    /// The Unix socket its API also listens on, on a machine with both
    /// roles; empty for none.
    #[serde(default)]
    pub socket: String,
    /// The group that may use that socket: the web UI's.
    #[serde(default)]
    pub socket_group: String,
    /// The front's `WireGuard` address: the only one its API answers.
    pub front_address: Ipv4Addr,
    /// Where the front's API is: its `WireGuard` address and port 7911, or
    /// its socket on a machine with both roles.
    pub front: Endpoint,
    /// Where its workspaces' disks, configs and stop markers are.
    pub state_dir: String,
    /// The template workspaces made at run time are made from.
    pub template: String,
    /// Where the configs Nix renders and those made at run time are.
    #[serde(default = "default_config_dirs")]
    pub config_dirs: Vec<String>,
    /// The memory it keeps for itself, as the memory daemon does.
    #[serde(default = "default_reserve_percent")]
    pub reserve_percent: f64,
    #[serde(rename = "reserveMinGiB", default = "default_reserve_min_gib")]
    pub reserve_min_gib: f64,
    #[serde(default)]
    pub pools: Vec<Pool>,
}

fn default_config_dirs() -> Vec<String> {
    DEFAULT_CONFIG_DIRS.map(String::from).into()
}

fn default_reserve_percent() -> f64 {
    10.0
}

fn default_reserve_min_gib() -> f64 {
    2.0
}

/// An absolute path below `/` with no empty, `.` or `..` component.
fn is_clean_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else { return false };
    !path.contains('\n') && rest.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

impl NodeSettings {
    /// Reads and checks a node's settings.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = hostutil::read_bounded(path, MAX_SETTINGS).with_context(|| path.display().to_string())?;
        let s: NodeSettings = serde_json::from_slice(&raw).with_context(|| path.display().to_string())?;
        s.validate().with_context(|| path.display().to_string())?;
        Ok(s)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(is_node_name(&self.name), "invalid node name {:?}", self.name);
        ensure!(self.listen.is_some() || !self.socket.is_empty(), "the API listens nowhere");
        ensure!(self.socket.is_empty() || is_clean_path(&self.socket), "socket must be a clean absolute path");
        for (field, path) in [("stateDir", &self.state_dir), ("template", &self.template)] {
            ensure!(is_clean_path(path), "{field} must be a clean absolute path");
        }
        for d in &self.config_dirs {
            ensure!(is_clean_path(d), "configDirs must be clean absolute paths");
        }
        ensure!(
            (0.0..100.0).contains(&self.reserve_percent) && self.reserve_min_gib >= 0.0,
            "the reserve must be a percentage below 100 and a size"
        );
        Ok(())
    }

    #[must_use]
    pub fn config_dirs(&self) -> Vec<PathBuf> {
        self.config_dirs.iter().map(PathBuf::from).collect()
    }

    #[must_use]
    pub fn state_root(&self) -> PathBuf {
        PathBuf::from(&self.state_dir)
    }

    /// Its settings, if this machine is a node of a fleet.
    pub fn of_this_machine() -> anyhow::Result<Option<Self>> {
        let path = Path::new(NODE_SETTINGS);
        if !path.exists() {
            return Ok(None);
        }
        Self::load(path).map(Some)
    }
}

/// A node, as the front knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NodeEntry {
    /// Its `WireGuard` address: the only one the front takes its requests
    /// from, never one behind it.
    pub address: Ipv4Addr,
    /// The first three octets of its workspaces' /24.
    pub subnet: String,
    /// Where its API is: its `WireGuard` address and port 7910, or its
    /// socket on a machine with both roles.
    pub api: Endpoint,
}

/// What the front lets people make, besides the node's template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrontSelfService {
    pub limits: Limits,
    /// The apps offered, by id, in order: some of the fleet's.
    #[serde(default)]
    pub apps: Vec<String>,
    /// Each workspace's secrets directory is a directory of this.
    pub secrets_root: String,
    /// The environment files every such workspace has, after its own.
    #[serde(default)]
    pub environment_files: Vec<String>,
    /// Prints a workspace's secret environment for its owner.
    pub environment_command: String,
    /// Exits 0 while an owner may keep their workspaces; empty asks no one.
    #[serde(default)]
    pub owner_command: String,
}

/// The front's settings, `/etc/workbench-front.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrontSettings {
    /// Workspaces are `<scheme>://<name>.<domain>`.
    pub domain: String,
    pub scheme: String,
    /// Where the registry, the requests, the quotas and the closed nodes
    /// are.
    pub state_dir: String,
    /// Where its API listens: its `WireGuard` address and port 7911.
    #[serde(default)]
    pub listen: Option<SocketAddr>,
    /// The Unix socket its API also listens on, for the node on this
    /// machine; empty for none.
    #[serde(default)]
    pub socket: String,
    /// The node on this machine, which asks through the socket.
    #[serde(default)]
    pub local_node: String,
    pub nodes: BTreeMap<String, NodeEntry>,
    /// Where Nix renders the declared workspaces' records.
    pub declared: String,
    /// The fleet's apps, as the nodes' templates have them: for their
    /// addresses, and to tell what waits for a restart.
    #[serde(default)]
    pub apps: Vec<BootApp>,
    #[serde(default)]
    pub default_apps: Vec<String>,
    /// The CAs and the guests' host keys, root's.
    #[serde(default)]
    pub ssh: SshCa,
    /// Set when people make their own workspaces.
    #[serde(default)]
    pub self_service: Option<FrontSelfService>,
}

impl FrontSettings {
    /// Reads and checks the front's settings; its limits offer its apps.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = hostutil::read_bounded(path, MAX_SETTINGS).with_context(|| path.display().to_string())?;
        let mut s: FrontSettings = serde_json::from_slice(&raw).with_context(|| path.display().to_string())?;
        s.validate().with_context(|| path.display().to_string())?;
        let (apps, defaults) = (s.apps.clone(), s.default_apps.clone());
        if let Some(ss) = &mut s.self_service {
            ss.limits.apps = ss
                .apps
                .iter()
                .filter_map(|id| apps.iter().find(|a| a.id == *id))
                .map(|a| AppInfo {
                    id: a.id.clone(),
                    name: a.name.clone(),
                    terminal: a.port.is_none(),
                    default: defaults.contains(&a.id),
                })
                .collect();
        }
        Ok(s)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(!self.domain.is_empty() && !self.domain.contains(['/', ' ']), "invalid domain {:?}", self.domain);
        ensure!(matches!(self.scheme.as_str(), "https" | "http"), "invalid scheme {:?}", self.scheme);
        for (field, path) in [("stateDir", &self.state_dir), ("declared", &self.declared)] {
            ensure!(is_clean_path(path), "{field} must be a clean absolute path");
        }
        ensure!(self.socket.is_empty() || is_clean_path(&self.socket), "socket must be a clean absolute path");
        ensure!(
            self.local_node.is_empty() || self.nodes.contains_key(&self.local_node),
            "localNode {:?} isn't one of the nodes",
            self.local_node
        );
        let (mut subnets, mut addresses) = (BTreeSet::new(), BTreeSet::new());
        for (name, n) in &self.nodes {
            ensure!(is_node_name(name), "invalid node name {name:?}");
            ensure!(is_subnet(&n.subnet), "node {name}: invalid subnet {:?}", n.subnet);
            ensure!(subnets.insert(&n.subnet), "node {name}: subnet {} is another node's", n.subnet);
            ensure!(addresses.insert(n.address), "node {name}: address {} is another node's", n.address);
        }
        for a in &self.apps {
            ensure!(is_app_id(&a.id), "invalid app id {:?}", a.id);
        }
        if !self.ssh.is_empty() {
            for path in [&self.ssh.keygen, &self.ssh.host_ca, &self.ssh.user_ca, &self.ssh.host_keys] {
                ensure!(is_clean_path(path), "ssh: {path:?} must be a clean absolute path");
            }
        }
        if let Some(ss) = &self.self_service {
            ensure!(is_clean_path(&ss.secrets_root), "selfService.secretsRoot must be a clean absolute path");
            for f in &ss.environment_files {
                ensure!(f.starts_with('/'), "environment file {f:?} must be absolute");
            }
        }
        Ok(())
    }

    /// Its settings, if this machine is a fleet's front.
    pub fn of_this_machine() -> anyhow::Result<Option<Self>> {
        let path = Path::new(FRONT_SETTINGS);
        if !path.exists() {
            return Ok(None);
        }
        Self::load(path).map(Some)
    }

    #[must_use]
    pub fn state_root(&self) -> PathBuf {
        PathBuf::from(&self.state_dir)
    }

    /// A workspace's address: `<scheme>://<name>.<domain>`.
    #[must_use]
    pub fn url(&self, name: &str) -> String {
        format!("{}://{name}.{}", self.scheme, self.domain)
    }

    /// The node whose `WireGuard` address this is.
    #[must_use]
    pub fn node_at(&self, address: Ipv4Addr) -> Option<&str> {
        self.nodes.iter().find(|(_, n)| n.address == address).map(|(name, _)| name.as_str())
    }

    /// The fleet's apps `ids` name, those it no longer has left out.
    #[must_use]
    pub fn apps_of(&self, ids: &[String]) -> Vec<BootApp> {
        ids.iter().filter_map(|id| self.apps.iter().find(|a| a.id == *id).cloned()).collect()
    }

    /// A node's name, if it is one of the fleet's.
    pub fn node(&self, name: &str) -> anyhow::Result<&NodeEntry> {
        match self.nodes.get(name) {
            Some(n) => Ok(n),
            None => bail!("{name:?} isn't a node of this fleet"),
        }
    }
}

/// Three octets: a /24's first.
fn is_subnet(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| p.parse::<u8>().is_ok() && (p.len() == 1 || !p.starts_with('0')))
}

/// Where the front keeps a closed node's mark: a closed node runs what it
/// has, and takes neither new workspaces nor moves. The web UI writes it.
#[must_use]
pub fn closed_marker(state_root: &Path, node: &str) -> PathBuf {
    state_root.join("closed").join(node)
}

/// Whether an admin closed a node.
#[must_use]
pub fn is_closed(state_root: &Path, node: &str) -> bool {
    closed_marker(state_root, node).exists()
}

/// Closes a node, or reopens it.
pub fn set_closed(state_root: &Path, node: &str, closed: bool) -> anyhow::Result<()> {
    ensure!(is_node_name(node), "invalid node name {node:?}");
    let marker = closed_marker(state_root, node);
    if closed {
        std::fs::create_dir_all(marker.parent().context("closed")?)?;
        hostutil::atomic_write(&marker, b"", 0o644)?;
    } else {
        match std::fs::remove_file(&marker) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn front_settings(state: &Path) -> FrontSettings {
        FrontSettings {
            domain: "ws.test".into(),
            scheme: "https".into(),
            state_dir: state.join("state").display().to_string(),
            listen: Some("10.231.0.1:7911".parse().unwrap()),
            socket: String::new(),
            local_node: String::new(),
            nodes: [
                (
                    "a".to_owned(),
                    NodeEntry {
                        address: "10.231.0.10".parse().unwrap(),
                        subnet: "10.232.0".into(),
                        api: "10.231.0.10:7910".parse().unwrap(),
                    },
                ),
                (
                    "b".to_owned(),
                    NodeEntry {
                        address: "10.231.0.11".parse().unwrap(),
                        subnet: "10.232.1".into(),
                        api: "10.231.0.11:7910".parse().unwrap(),
                    },
                ),
            ]
            .into(),
            declared: state.join("declared").display().to_string(),
            apps: crate::self_service::tests::template().apps,
            default_apps: vec!["t3code".into()],
            ssh: SshCa::default(),
            self_service: Some(FrontSelfService {
                limits: crate::self_service::tests::template().limits,
                apps: vec!["t3code".into()],
                secrets_root: state.join("secrets").display().to_string(),
                environment_files: vec![],
                environment_command: "/bin/true".into(),
                owner_command: String::new(),
            }),
        }
    }

    #[test]
    fn endpoints() {
        assert_eq!("10.231.0.1:7911".parse::<Endpoint>().unwrap(), Endpoint::Tcp("10.231.0.1:7911".parse().unwrap()));
        assert_eq!(
            "/run/workbench-front/api.sock".parse::<Endpoint>().unwrap(),
            Endpoint::Unix("/run/workbench-front/api.sock".into())
        );
        assert!("front.example.com".parse::<Endpoint>().is_err());
        let json = serde_json::to_string(&Endpoint::Unix("/run/x.sock".into())).unwrap();
        assert_eq!(json, "\"/run/x.sock\"");
    }

    /// The settings files the modules render, as `docs/multi-node.md`
    /// writes them.
    #[test]
    fn settings_files() {
        let node: NodeSettings = serde_json::from_value(serde_json::json!({
            "name": "b",
            "listen": "10.231.0.11:7910",
            "frontAddress": "10.231.0.1",
            "front": "10.231.0.1:7911",
            "stateDir": "/var/lib/workbench",
            "template": "/etc/workbench-self-service.json",
            "pools": [{ "name": "workspaces", "status": "/run/vmkit/memory/workspaces.json" }]
        }))
        .unwrap();
        node.validate().unwrap();
        assert_eq!(node.config_dirs(), [PathBuf::from("/etc/workbench"), PathBuf::from("/var/lib/workbench/configs")]);
        assert!(
            serde_json::from_value::<NodeSettings>(serde_json::json!({"name": "b", "sudo": true})).is_err(),
            "unknown fields"
        );
        // As docs/multi-node.md has it.
        let documented: FrontSettings = serde_json::from_value(serde_json::json!({
            "domain": "ws.example.com",
            "scheme": "https",
            "stateDir": "/var/lib/workbench",
            "listen": "10.231.0.1:7911",
            "socket": "",
            "localNode": "",
            "nodes": {
                "a": { "address": "10.231.0.10", "subnet": "10.232.0", "api": "10.231.0.10:7910" },
                "b": { "address": "10.231.0.11", "subnet": "10.232.1", "api": "10.231.0.11:7910" }
            },
            "declared": "/etc/workbench-front",
            "apps": [{ "id": "t3code", "name": "T3 Code", "port": 3773, "bundle": "/nix/store/x-t3code-app" }],
            "defaultApps": ["t3code"],
            "ssh": {
                "keygen": "/nix/store/x/bin/ssh-keygen",
                "hostCa": "/var/lib/workbench/ssh/host-ca",
                "userCa": "/var/lib/workbench/ssh/user-ca.pub",
                "hostKeys": "/var/lib/workbench/ssh/hosts"
            },
            "selfService": {
                "limits": { "os": ["ubuntu"], "bootMemMiB": 2048, "maxMemoryGiB": 16, "maxDiskGiB": 128, "quota": 1 },
                "apps": ["t3code"],
                "secretsRoot": "/var/lib/workbench-secrets",
                "environmentFiles": [],
                "environmentCommand": "/path/to/workspace-env",
                "ownerCommand": ""
            }
        }))
        .unwrap();
        documented.validate().unwrap();
        let declared: workbench_model::fleet::Record = serde_json::from_value(serde_json::json!({
            "name": "alice", "owner": "alice", "node": "a", "ip": "10.232.0.10",
            "os": "ubuntu", "apps": ["t3code"], "maxMemoryGiB": 16, "diskGiB": 64,
            "sshAuthorizedKeys": ["ssh-ed25519 AAAA"],
            "secretsDir": "/var/lib/workbench-secrets/alice",
            "environmentFiles": ["/run/secrets/llm.env"]
        }))
        .unwrap();
        declared.validate().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let front = front_settings(dir.path());
        front.validate().unwrap();
        let path = dir.path().join("front.json");
        std::fs::write(&path, serde_json::to_vec(&front).unwrap()).unwrap();
        let loaded = FrontSettings::load(&path).unwrap();
        assert_eq!(loaded.self_service.unwrap().limits.apps[0].id, "t3code");
        assert_eq!(front.node_at("10.231.0.11".parse().unwrap()), Some("b"));
        assert_eq!(front.node_at("10.232.0.130".parse().unwrap()), None, "an address behind a node is none");
        let mut same = front.clone();
        same.nodes.get_mut("b").unwrap().subnet = "10.232.0".into();
        assert!(same.validate().is_err(), "two nodes, one subnet");
        let mut lost = front.clone();
        lost.local_node = "c".into();
        assert!(lost.validate().is_err(), "a local node that isn't one");
        assert!(!is_subnet("10.232.0.0") && !is_subnet("10.232.01") && is_subnet("10.232.0"));
    }

    #[test]
    fn closing_nodes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_closed(dir.path(), "a"));
        set_closed(dir.path(), "a", true).unwrap();
        assert!(is_closed(dir.path(), "a"));
        set_closed(dir.path(), "a", false).unwrap();
        set_closed(dir.path(), "a", false).unwrap();
        assert!(!is_closed(dir.path(), "a"));
        assert!(set_closed(dir.path(), "../a", true).is_err());
    }
}
