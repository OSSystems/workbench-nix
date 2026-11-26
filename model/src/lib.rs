//! What workbench's host, its CLI and its web UI agree on: workspace and
//! user names, a workspace's status and the state people see, its apps and
//! what waits for a restart, who may control it, how the UI lists
//! workspaces, and what `workbench-connect` exchanges with the UI. It
//! compiles for the host and for the browser, so the web UI's server and
//! its WebAssembly share the same types and checks.

use std::fmt;
use std::str::FromStr;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

pub mod apps;
pub mod connect;
pub mod memory;
pub mod self_service;

pub use apps::{App, Change, OpenBlocker, Stage};

/// A gibibyte.
pub const GIB: i64 = 1 << 30;

/// Whether `name` is a workspace name: a lowercase DNS label of at most 28
/// characters, without `--`, which comes before an app's id in its address.
#[must_use]
pub fn is_workspace_name(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=28).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && b[b.len() - 1] != b'-'
        && !name.contains("--")
}

/// Whether `name` is a user name the guest and the host accept.
#[must_use]
pub fn is_user_name(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=32).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0] == b'_')
        && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'.' | b'-'))
}

/// A workspace's state, as people see it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Running,
    Starting,
    Stopping,
    #[default]
    Stopped,
    Failing,
    /// Never derived from systemd: stopping or starting after a restart
    /// someone asked for.
    Restarting,
}

impl State {
    /// The word for it.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            State::Running => "Running",
            State::Starting => "Starting",
            State::Stopping => "Stopping",
            State::Stopped => "Stopped",
            State::Failing => "Failing",
            State::Restarting => "Restarting",
        }
    }

    /// Its dot's class: on, going, bad or off.
    #[must_use]
    pub fn dot(self) -> &'static str {
        match self {
            State::Running => "on",
            State::Starting | State::Stopping | State::Restarting => "go",
            State::Failing => "bad",
            State::Stopped => "off",
        }
    }

    /// Whether the workspace is on its way to another state, so no action
    /// may start.
    #[must_use]
    pub fn changing(self) -> bool {
        matches!(self, State::Stopping | State::Restarting)
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label().to_ascii_lowercase())
    }
}

/// One workspace as `workbench status --json` and the web UI's
/// `/api/workspaces` give it.
// Independent facts of a status record, not states of one machine.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub name: String,
    /// Whether a config declares it; a removed workspace only has a disk.
    pub configured: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ip: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub os: String,
    pub state: State,
    /// When it entered the state it shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<Timestamp>,
    /// How often systemd restarted the VM since it was started.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub restarts: u32,
    /// Its apps: the config's, then those only the running VM has.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<App>,
    /// What the next start changes, from what the VM booted with; empty
    /// when nothing waits for a restart, or no VM runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<Change>,
    /// The unit's `ActiveState`.
    pub unit: String,
    /// The VM's state, as cloud-hypervisor says, or `-`.
    pub vm: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub memory_bytes: i64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub boot_memory_bytes: i64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub max_memory_bytes: i64,
    /// Its most memory is `auto`: the host's, which `max_memory_bytes` is now.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub memory_auto: bool,
    pub disk_bytes: u64,
    pub disk_used_bytes: u64,
    /// `up` or `down` while the VM runs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    /// Made at run time, by its owner: its owner reaches it only while in
    /// a self-service group, and it may be destroyed from the web UI.
    #[serde(default, skip_serializing_if = "is_false")]
    pub self_service: bool,
    /// Its owner left the self-service groups or the directory: it was
    /// stopped, and its disk kept for an admin.
    #[serde(default, skip_serializing_if = "is_false")]
    pub owner_left: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// Who is asking: single sign-on names them and their groups, which with
/// the configuration make them an admin, or someone who makes their own
/// workspaces.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Viewer {
    pub name: String,
    pub admin: bool,
    /// May make workspaces of their own: in a self-service group, or an
    /// admin.
    #[serde(default)]
    pub creates: bool,
}

impl Viewer {
    /// Whether the viewer sees and controls a workspace: its owner and the
    /// admins do, the owner of one made at run time only while they may
    /// make workspaces. A workspace removed from the configuration is the
    /// admins'.
    #[must_use]
    pub fn may(&self, ws: &Workspace) -> bool {
        self.admin || (ws.configured && self.may_use(&ws.owner, ws.self_service))
    }

    /// The owner rule, from what a workspace's config says: what the list
    /// and nginx's access check both apply.
    #[must_use]
    pub fn may_use(&self, owner: &str, self_service: bool) -> bool {
        self.admin || (owner == self.name && (!self_service || self.creates))
    }

    /// Whether the workspace is the viewer's own.
    #[must_use]
    pub fn owns(&self, ws: &Workspace) -> bool {
        ws.configured && ws.owner == self.name
    }
}

/// What may be done to a workspace's unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verb {
    Start,
    Stop,
    Restart,
}

impl Verb {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Verb::Start => "start",
            Verb::Stop => "stop",
            Verb::Restart => "restart",
        }
    }

    /// The word for it, capitalised.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Verb::Start => "Start",
            Verb::Stop => "Stop",
            Verb::Restart => "Restart",
        }
    }
}

impl fmt::Display for Verb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Verb {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "start" => Ok(Verb::Start),
            "stop" => Ok(Verb::Stop),
            "restart" => Ok(Verb::Restart),
            _ => Err(format!("unknown action {s:?}")),
        }
    }
}

impl Workspace {
    #[must_use]
    pub fn app(&self, id: &str) -> Option<&App> {
        self.apps.iter().find(|a| a.id == id)
    }

    /// Where an app stands between the VM that runs and the next start.
    #[must_use]
    pub fn stage(&self, app: &str) -> Stage {
        Stage::of(app, &self.changes)
    }

    /// What keeps an app from opening now, if anything. One that goes
    /// with the next start still opens until then; one without an address
    /// never opens in a browser.
    #[must_use]
    pub fn open_blocker(&self, app: &str) -> Option<OpenBlocker> {
        match self.app(app) {
            None => return Some(OpenBlocker::Unknown),
            Some(a) if a.url.is_none() => return Some(OpenBlocker::NoAddress),
            Some(_) => {}
        }
        match self.state {
            State::Stopped | State::Stopping => Some(OpenBlocker::Stopped),
            State::Starting | State::Restarting | State::Failing => Some(OpenBlocker::Starting),
            State::Running if !self.stage(app).runs() => Some(OpenBlocker::Coming),
            State::Running => None,
        }
    }

    /// The apps the VM runs, or would run once started: not those that
    /// only come with the next start.
    pub fn running_apps(&self) -> impl Iterator<Item = &App> {
        self.apps.iter().filter(|a| self.stage(&a.id).runs())
    }

    /// The address without its scheme.
    #[must_use]
    pub fn host(&self) -> &str {
        self.url.trim_start_matches("https://").trim_start_matches("http://")
    }
}

/// A workspace in the UI's list: its status, and what the server says of
/// its times, in the host's time zone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub ws: Workspace,
    /// The short text after its name.
    pub note: String,
    /// When it entered its state, briefly, or empty.
    pub since: String,
}

/// What a viewer sees of the host's workspaces: what the list page renders
/// from, and what its live updates carry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub viewer: Viewer,
    pub rows: Vec<Row>,
    /// The apps a workspace made at run time may have.
    #[serde(default)]
    pub apps: Vec<apps::AppInfo>,
    /// The least and the most memory, in GiB, a workspace made at run time
    /// may grow to; none without workspaces made at run time.
    #[serde(default)]
    pub memory_gib: Option<(i64, i64)>,
    /// Whether such a workspace may grow to `auto`, the host's memory.
    #[serde(default)]
    pub memory_auto: bool,
}

impl Snapshot {
    /// The workspaces `viewer` may see; only admins see their addresses.
    #[must_use]
    pub fn of(viewer: Viewer, rows: impl IntoIterator<Item = Row>) -> Self {
        let rows = rows
            .into_iter()
            .filter(|r| viewer.may(&r.ws))
            .map(|mut r| {
                if !viewer.admin {
                    r.ws.ip.clear();
                }
                r
            })
            .collect();
        Snapshot { viewer, rows, apps: vec![], memory_gib: None, memory_auto: false }
    }

    #[must_use]
    pub fn find(&self, name: &str) -> Option<&Row> {
        self.rows.iter().find(|r| r.ws.name == name)
    }
}

/// Which workspaces the list shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Filter {
    #[default]
    All,
    Mine,
    Running,
    Stopped,
}

impl Filter {
    /// The filter a query names; anything else is all. Only admins have
    /// one for their own.
    #[must_use]
    pub fn parse(s: &str, admin: bool) -> Self {
        match s {
            "mine" if admin => Filter::Mine,
            "running" => Filter::Running,
            "stopped" => Filter::Stopped,
            _ => Filter::All,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::Mine => "mine",
            Filter::Running => "running",
            Filter::Stopped => "stopped",
        }
    }

    fn shows(self, viewer: &Viewer, ws: &Workspace) -> bool {
        match self {
            Filter::All => true,
            Filter::Mine => viewer.owns(ws),
            Filter::Running => ws.state != State::Stopped,
            Filter::Stopped => ws.state == State::Stopped,
        }
    }
}

/// A group of the list, by state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub state: State,
    pub label: &'static str,
    /// Indexes into the snapshot's rows.
    pub rows: Vec<usize>,
}

/// The list page: its groups, the selection, and the viewer's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct List {
    pub filter: Filter,
    /// The workspace in the side panel, if any.
    pub selected: Option<String>,
    /// The address names the selection, which on a phone shows the panel
    /// as a page of its own.
    pub picked: bool,
    pub groups: Vec<Group>,
    /// Indexes of the viewer's own workspaces.
    pub mine: Vec<usize>,
}

const GROUPS: [(&str, &[State]); 5] = [
    ("Failing", &[State::Failing]),
    ("Changing", &[State::Restarting, State::Stopping]),
    ("Starting", &[State::Starting]),
    ("Running", &[State::Running]),
    ("Stopped", &[State::Stopped]),
];

impl List {
    /// Groups what the filter and the search show by state, and selects
    /// the named workspace if it is shown, or else the first.
    #[must_use]
    pub fn build(snap: &Snapshot, filter: Filter, selected: &str, find: &str) -> Self {
        let find = find.trim().to_lowercase();
        let mut groups = Vec::new();
        let mut picked = false;
        for (label, states) in GROUPS {
            let rows: Vec<usize> = (0..snap.rows.len())
                .filter(|&i| {
                    let ws = &snap.rows[i].ws;
                    filter.shows(&snap.viewer, ws) && states.contains(&ws.state) && ws.name.contains(&find)
                })
                .collect();
            picked |= rows.iter().any(|&i| snap.rows[i].ws.name == selected);
            if !rows.is_empty() {
                groups.push(Group { state: states[0], label, rows });
            }
        }
        let selected = if picked {
            Some(selected.to_owned())
        } else {
            groups.first().map(|g: &Group| snap.rows[g.rows[0]].ws.name.clone())
        };
        let mine = (0..snap.rows.len()).filter(|&i| snap.viewer.owns(&snap.rows[i].ws)).collect();
        List { filter, selected, picked, groups, mine }
    }
}

/// A size in GiB, without decimals when whole.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn gib(bytes: i64) -> String {
    let v = bytes as f64 / GIB as f64;
    if v.fract() == 0.0 { format!("{v:.0}") } else { format!("{v:.1}") }
}

/// `part` as a percentage of `whole`, at most 100, for a CSS width.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn pct(part: i64, whole: i64) -> String {
    if whole <= 0 {
        return "0".into();
    }
    format!("{:.1}", (part as f64 * 100.0 / whole as f64).clamp(0.0, 100.0))
}

/// The host page: its memory, each pool's share and the running
/// workspaces'.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostView {
    pub total: i64,
    pub available: i64,
    pub reserve: i64,
    pub free: i64,
    pub pools: Vec<PoolView>,
    pub workspaces: Vec<Workspace>,
    /// When it was read, as a time of day.
    pub at: String,
    /// People's quotas, when the host lets people make workspaces.
    #[serde(default)]
    pub quotas: Option<Quotas>,
}

/// The host page's quotas: the host's, and each person's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quotas {
    pub default: u32,
    pub people: Vec<self_service::QuotaRow>,
}

/// One memory pool on the host page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolView {
    pub name: String,
    pub committed: i64,
    pub pending: i64,
    /// When one of its VMs was last denied memory, or empty.
    pub denied: String,
    /// When it last reported.
    pub report: String,
    /// It has not reported for too long.
    pub stale: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["a", "ab", "a-b", "alice-dev", &"a".repeat(28)] {
            assert!(is_workspace_name(ok), "{ok}");
        }
        for bad in ["", "-a", "a-", "1a", "A", "a_b", "a.b", ".stopped", "a--b", &"a".repeat(29)] {
            assert!(!is_workspace_name(bad), "{bad}");
        }
        assert!(is_user_name("_svc.x-1") && !is_user_name("Bob") && !is_user_name("a/b"));
    }

    fn ws(name: &str, owner: &str, state: State) -> Row {
        Row {
            ws: Workspace { name: name.into(), owner: owner.into(), configured: true, state, ..Default::default() },
            ..Default::default()
        }
    }

    #[test]
    fn who_sees_what() {
        let rows = [ws("alice", "alice", State::Running), ws("bob", "bob", State::Stopped)];
        let mut removed = ws("old", "", State::Stopped);
        removed.ws.configured = false;
        let alice = Snapshot::of(
            Viewer { name: "alice".into(), admin: false, creates: false },
            [rows[0].clone(), rows[1].clone()],
        );
        assert_eq!(alice.rows.len(), 1);
        let carol = Viewer { name: "carol".into(), admin: true, creates: true };
        assert!(carol.may(&rows[1].ws) && carol.may(&removed.ws));
        let mut mallory = removed.clone();
        mallory.ws.owner = "mallory".into();
        assert!(
            !Viewer { name: "mallory".into(), admin: false, creates: false }.may(&mallory.ws),
            "a removed workspace is no one's"
        );
        let mut made = rows[0].clone();
        made.ws.self_service = true;
        let alice = |creates| Viewer { name: "alice".into(), admin: false, creates };
        assert!(
            alice(true).may(&made.ws) && !alice(false).may(&made.ws),
            "a workspace made at run time needs the group"
        );
        assert!(alice(false).may(&rows[0].ws), "one from the configuration does not");
    }

    #[test]
    fn list_groups_and_selects() {
        let viewer = Viewer { name: "alice".into(), admin: true, creates: true };
        let snap = Snapshot::of(
            viewer,
            [
                ws("a", "alice", State::Running),
                ws("b", "bob", State::Failing),
                ws("c", "alice", State::Stopped),
                ws("d", "bob", State::Restarting),
            ],
        );
        let l = List::build(&snap, Filter::All, "c", "");
        let labels: Vec<_> = l.groups.iter().map(|g| g.label).collect();
        assert_eq!(labels, ["Failing", "Changing", "Running", "Stopped"]);
        assert_eq!((l.selected.as_deref(), l.picked), (Some("c"), true));
        assert_eq!(l.mine, [0, 2]);
        let l = List::build(&snap, Filter::Mine, "b", "");
        assert_eq!((l.selected.as_deref(), l.picked), (Some("a"), false), "a hidden workspace is not selected");
        assert_eq!(List::build(&snap, Filter::Stopped, "", "").groups.len(), 1);
        let l = List::build(&snap, Filter::All, "a", " B ");
        let labels: Vec<_> = l.groups.iter().map(|g| g.label).collect();
        assert_eq!(labels, ["Failing"], "a search leaves no empty group");
        assert_eq!(l.selected.as_deref(), Some("b"), "nor a selection it does not match");
        assert_eq!(Filter::parse("mine", false), Filter::All, "only admins filter for their own");
    }

    #[test]
    fn open_blockers() {
        let mut w = ws("a", "alice", State::Running).ws;
        let app = |id: &str| App {
            id: id.into(),
            name: id.into(),
            url: Some(String::new()),
            open_link: true,
            ..App::default()
        };
        w.apps = vec![app("t3code"), app("herdr"), app("old"), App { url: None, ..app("term") }];
        w.changes = vec![Change::AppAdded { app: "herdr".into() }, Change::AppRemoved { app: "old".into() }];
        assert_eq!(w.open_blocker("t3code"), None);
        assert_eq!(w.open_blocker("herdr"), Some(OpenBlocker::Coming));
        assert_eq!(w.open_blocker("old"), None, "it goes at the next start, not before");
        assert_eq!(w.open_blocker("nope"), Some(OpenBlocker::Unknown));
        assert_eq!(w.open_blocker("term"), Some(OpenBlocker::NoAddress), "reached from a terminal");
        assert_eq!(w.running_apps().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["t3code", "old", "term"]);
        w.state = State::Failing;
        assert_eq!(w.open_blocker("t3code"), Some(OpenBlocker::Starting));
        w.state = State::Stopped;
        assert_eq!(w.open_blocker("t3code"), Some(OpenBlocker::Stopped));
    }

    /// The JSON `workbench status --json` and `/api/workspaces` give.
    #[test]
    fn workspace_json() {
        let w = Workspace {
            name: "alice".into(),
            configured: true,
            owner: "alice".into(),
            state: State::Running,
            since: Some("2026-11-16T09:12:00Z".parse().unwrap()),
            apps: vec![App {
                id: "t3code".into(),
                name: "T3 Code".into(),
                url: Some("https://alice--t3code.ws.test".into()),
                open_link: true,
                ..App::default()
            }],
            unit: "active".into(),
            vm: "Running".into(),
            memory_bytes: 3 * GIB,
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&w).unwrap(),
            concat!(
                r#"{"name":"alice","configured":true,"owner":"alice","state":"running","since":"2026-11-16T09:12:00Z","#,
                r#""apps":[{"id":"t3code","name":"T3 Code","url":"https://alice--t3code.ws.test","openLink":true}],"#,
                r#""unit":"active","vm":"Running","memoryBytes":3221225472,"diskBytes":0,"diskUsedBytes":0}"#
            )
        );
    }

    #[test]
    fn sizes() {
        assert_eq!(gib(16 * GIB), "16");
        assert_eq!(gib(3 * GIB / 2), "1.5");
        assert_eq!(pct(GIB, 4 * GIB), "25.0");
        assert_eq!(pct(5, 0), "0");
        assert_eq!(pct(8, 4), "100.0");
    }
}
