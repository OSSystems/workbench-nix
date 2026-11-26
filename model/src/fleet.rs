//! What a front and its nodes exchange, and what the front keeps of every
//! workspace: the registry's records, the node's API's requests and
//! answers, and what the UI shows of the nodes. `docs/multi-node.md` is
//! the specification.

use serde::{Deserialize, Serialize};

use crate::apps::Boot;
use crate::memory::Ceiling;
use crate::{Workspace, is_user_name, is_workspace_name};

/// The header every request and answer between a front and its nodes
/// carries: the sender's version, which must be the receiver's.
pub const VERSION_HEADER: &str = "workbench-version";
/// The port of a node's API, on its `WireGuard` address.
pub const NODE_PORT: u16 = 7910;
/// The port of the front's API, on its `WireGuard` address.
pub const FRONT_PORT: u16 = 7911;

/// The front's record of a workspace, in its registry: who owns it, the
/// node its disk is on and its address there, what it is made of, and a
/// move under way. The front, never a node, says who owns a workspace and
/// where it is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Record {
    pub name: String,
    pub owner: String,
    /// The node its disk is on.
    pub node: String,
    /// Its address, in that node's subnet.
    pub ip: String,
    pub os: String,
    pub ssh_authorized_keys: Vec<String>,
    /// The most memory it grows to, or `auto`: its node's.
    #[serde(rename = "maxMemoryGiB")]
    pub max_memory: Ceiling,
    #[serde(rename = "diskGiB")]
    pub disk_gib: i64,
    /// Its apps, by id.
    pub apps: Vec<String>,
    /// Declared in Nix, which places it: not made at run time.
    #[serde(skip_serializing_if = "is_false")]
    pub declared: bool,
    /// A declared one's secrets directory and environment files, paths on
    /// the front; one made at run time has the front's.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub secrets_dir: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub environment_files: Vec<String>,
    /// Being made or taken away: its name and address are taken, and if
    /// what makes it dies half way, the front cleans up after it.
    #[serde(skip_serializing_if = "is_false")]
    pub pending: bool,
    /// A move under way.
    #[serde(rename = "move", skip_serializing_if = "Option::is_none")]
    pub moving: Option<Move>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

impl Record {
    /// Checks what the front relies on: its names and its address.
    pub fn validate(&self) -> Result<(), String> {
        if !is_workspace_name(&self.name) {
            return Err(format!("invalid workspace name {:?}", self.name));
        }
        if !self.owner.is_empty() && (!is_user_name(&self.owner) || self.owner == "root") {
            return Err(format!("{}: invalid owner {:?}", self.name, self.owner));
        }
        if !is_node_name(&self.node) {
            return Err(format!("{}: invalid node {:?}", self.name, self.node));
        }
        let addresses = std::iter::once(&self.ip).chain(self.moving.iter().flat_map(|m| [&m.ip, &m.from_ip]));
        for ip in addresses {
            if ip.parse::<std::net::Ipv4Addr>().is_err() {
                return Err(format!("{}: invalid address {ip:?}", self.name));
            }
        }
        if let Some(m) = &self.moving
            && (!is_node_name(&m.from) || !is_node_name(&m.to) || m.from == m.to)
        {
            return Err(format!("{}: a move from {:?} to {:?}", self.name, m.from, m.to));
        }
        Ok(())
    }

    /// Whether it is moving, after the switch or before.
    #[must_use]
    pub fn is_moving(&self) -> bool {
        self.moving.is_some()
    }

    /// Whether a move has switched it to its destination: its node is the
    /// destination's, and a move that stops now goes on, not back.
    #[must_use]
    pub fn switched(&self) -> bool {
        self.moving.as_ref().is_some_and(|m| self.node == m.to)
    }
}

/// A move under way, as the registry keeps it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Move {
    /// The node it comes from, and its address there.
    pub from: String,
    pub from_ip: String,
    /// The node it goes to, and its address there.
    pub to: String,
    pub ip: String,
    /// The phase it is in.
    pub phase: Phase,
    /// Whether it ran when it was stopped on the source: it starts on the
    /// destination only then.
    #[serde(default)]
    pub ran: bool,
}

/// The phases of a move, in order. Until the switch a move that stops
/// goes back; after it, on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// An address in the destination's subnet, and the workspace made
    /// there without a disk.
    #[default]
    Reserve,
    /// Stopped on the source, noting whether it ran.
    Stop,
    /// The disk streamed from the source to the destination.
    Copy,
    /// The record's node and address become the destination's.
    Switch,
    /// Started on the destination if it ran, until its agent answers.
    Start,
    /// Deleted on the source.
    Clean,
}

impl Phase {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Reserve => "reserve",
            Phase::Stop => "stop",
            Phase::Copy => "copy",
            Phase::Switch => "switch",
            Phase::Start => "start",
            Phase::Clean => "clean",
        }
    }
}

/// Whether `name` is a node's name: a workspace name's rule.
#[must_use]
pub fn is_node_name(name: &str) -> bool {
    is_workspace_name(name)
}

/// What a node says of itself: `GET /host`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostInfo {
    pub version: String,
    /// `MemTotal` and `MemAvailable`, in bytes.
    pub mem_total: i64,
    pub mem_available: i64,
    /// What the node keeps for itself, which no workspace grows into.
    pub reserve: i64,
    /// Its memory pools.
    #[serde(default)]
    pub pools: Vec<PoolInfo>,
    /// The file system its disks are on: its size and what is free.
    pub disk_total: u64,
    pub disk_free: u64,
    /// Its workspaces' maximum memory, together, in bytes.
    #[serde(default)]
    pub max_memory: i64,
}

impl HostInfo {
    /// Memory free for a new workspace: what is available less the
    /// reserve, as the host page shows it.
    #[must_use]
    pub fn free(&self) -> i64 {
        (self.mem_available - self.reserve).max(0)
    }
}

/// A memory pool's status on a node, as its controller publishes it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolInfo {
    pub name: String,
    /// None when its status file can't be read.
    #[serde(default)]
    pub status: Option<PoolStatus>,
}

/// vmkit's pool status: see vmkit's `docs/contracts.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolStatus {
    pub updated_at: jiff::Timestamp,
    pub committed_bytes: i64,
    pub pending_bytes: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied_at: Option<jiff::Timestamp>,
}

/// A workspace as a node has it: `GET /workspaces`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeWorkspace {
    /// What `workbench status` collects on the node.
    pub workspace: Workspace,
    /// What its VM booted with, while one runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub booted: Option<Boot>,
    /// What its next start boots, from the node's config; the front puts
    /// the record's apps in it for one made at run time.
    #[serde(default)]
    pub next: Boot,
    /// It has the stop marker that keeps it stopped.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stopped: bool,
}

/// `PUT /workspaces/<ws>`: make it from its record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MakeRequest {
    pub record: Record,
    /// For a move: without a disk, which comes by `PUT disk`, and stopped
    /// until the move starts it.
    #[serde(default)]
    pub for_move: bool,
}

/// `POST /workspaces/<ws>/open-link`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenLinkRequest {
    pub app: String,
}

/// The answer to `open-link`: the link, or why there is none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase", tag = "kind")]
pub enum OpenLinkAnswer {
    Link {
        link: String,
    },
    /// The agent doesn't answer.
    Starting {
        detail: String,
    },
    /// The hook failed.
    Failed {
        exit_code: i32,
        detail: String,
    },
    /// The hook printed something that isn't a link to the workspace.
    NotALink,
    Timeout,
}

/// What a refused or failed request answers, with a status that says
/// which: 400 a bad request, 403 not the caller's, 404 no such workspace,
/// 409 a conflict (the version, a workspace moving or running), 500 the
/// host failing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}

/// The answer to `PUT /workspaces/<ws>/disk`: the stream's sha256, in hex.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskAnswer {
    pub sha256: String,
}

/// A move an admin asks for: what the UI writes and `workbench-move@`
/// makes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveRequest {
    pub node: String,
}

/// A node as the host page shows it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeState {
    #[default]
    Up,
    /// An admin closed it: it runs what it has and takes no new one.
    Closed,
    /// It doesn't answer.
    Unreachable,
    /// It answers with another version, and isn't controlled.
    OtherVersion,
}

/// One node on the host page and in New workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeView {
    pub name: String,
    pub state: NodeState,
    /// What it said of itself, when it answered.
    #[serde(default)]
    pub host: Option<HostInfo>,
    /// Why it isn't up, for an admin.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

impl NodeView {
    /// Whether a new workspace or a move may go to it.
    #[must_use]
    pub fn takes(&self) -> bool {
        self.state == NodeState::Up && self.host.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Record {
        Record {
            name: "alice".into(),
            owner: "alice".into(),
            node: "a".into(),
            ip: "10.232.0.130".into(),
            os: "ubuntu".into(),
            max_memory: Ceiling::Size(8),
            disk_gib: 64,
            apps: vec!["t3code".into()],
            ..Record::default()
        }
    }

    /// The registry's files: camelCase, the sizes in the units Nix writes,
    /// and only what is set.
    #[test]
    fn record_json() {
        let mut r = record();
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            concat!(
                r#"{"name":"alice","owner":"alice","node":"a","ip":"10.232.0.130","os":"ubuntu","#,
                r#""sshAuthorizedKeys":[],"maxMemoryGiB":8,"diskGiB":64,"apps":["t3code"]}"#
            )
        );
        r.moving = Some(Move {
            from: "a".into(),
            from_ip: "10.232.0.130".into(),
            to: "b".into(),
            ip: "10.232.1.128".into(),
            phase: Phase::Copy,
            ran: true,
        });
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["move"]["phase"], "copy");
        assert_eq!(v["move"]["fromIp"], "10.232.0.130");
        assert_eq!(serde_json::from_value::<Record>(v).unwrap(), r);
        let declared: Record =
            serde_json::from_str(r#"{"name":"bob","owner":"bob","node":"b","ip":"10.232.1.10","declared":true}"#)
                .unwrap();
        assert!(declared.declared && declared.validate().is_ok(), "a declared record needs no more");
        assert!(serde_json::from_str::<Record>(r#"{"name":"x","sudo":true}"#).is_err(), "unknown fields");
    }

    #[test]
    fn records_validate() {
        record().validate().unwrap();
        let bad = [
            Record { name: "Alice".into(), ..record() },
            Record { owner: "root".into(), ..record() },
            Record { node: "../a".into(), ..record() },
            Record { ip: "10.232.0".into(), ..record() },
            Record {
                moving: Some(Move { from: "a".into(), to: "a".into(), ip: "10.232.0.131".into(), ..Move::default() }),
                ..record()
            },
        ];
        for r in bad {
            assert!(r.validate().is_err(), "{r:?}");
        }
    }

    /// A move has switched once the record is on its destination.
    #[test]
    fn switched() {
        let m = Move {
            from: "a".into(),
            from_ip: "10.232.0.130".into(),
            to: "b".into(),
            ip: "10.232.1.128".into(),
            ..Move::default()
        };
        let mut r = Record { moving: Some(m), ..record() };
        assert!(r.is_moving() && !r.switched());
        r.node = "b".into();
        assert!(r.switched());
        assert!(Phase::Copy < Phase::Switch && Phase::Switch < Phase::Start);
    }

    #[test]
    fn open_link_answers() {
        let v = serde_json::to_value(OpenLinkAnswer::Failed { exit_code: 2, detail: "x".into() }).unwrap();
        assert_eq!(v, serde_json::json!({"kind": "failed", "exitCode": 2, "detail": "x"}));
        let v = serde_json::to_value(OpenLinkAnswer::Link { link: "https://a--b.ws.test/".into() }).unwrap();
        assert_eq!(v["kind"], "link");
    }

    #[test]
    fn free_memory() {
        let h = HostInfo { mem_available: 10, reserve: 4, ..HostInfo::default() };
        assert_eq!(h.free(), 6);
        assert_eq!(HostInfo { reserve: 20, ..h }.free(), 0);
    }
}
