//! What a workspace is doing, from its unit, its VM and its agent; what
//! its next start changes; start, stop and restart; and an app's sign-in
//! link. The CLI and the web UI both call these.

use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use jiff::Timestamp;
use tokio::process::Command;
use vmkit::ch::Client;
use vmkit::hostutil;
use vmkit::memory::MIB;
use workbench_model::apps::{self, App, Boot, BootApp, app_url};
use workbench_model::{State, Verb, Workspace, is_workspace_name};

use crate::agent::{self, Request};
use crate::config::{self, Config};

/// How long each VM may take to answer the API and the agent.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long an app's sign-in hook may take, the agent's minute and some.
pub const OPEN_LINK_TIMEOUT: Duration = Duration::from_secs(90);

/// The unit of a workspace.
#[must_use]
pub fn unit_name(name: &str) -> String {
    format!("workbench@{name}.service")
}

/// What systemd says about a workspace's unit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnitInfo {
    pub active: String,
    pub sub: String,
    pub active_enter: Option<Timestamp>,
    pub inactive_enter: Option<Timestamp>,
    pub restarts: u32,
}

const UNIT_PROPERTIES: &str = "ActiveState,SubState,ActiveEnterTimestamp,InactiveEnterTimestamp,NRestarts";

/// Asks systemd about several units in one call.
pub async fn show_units(units: &[String]) -> anyhow::Result<Vec<UnitInfo>> {
    if units.is_empty() {
        return Ok(vec![]);
    }
    let out = Command::new("systemctl")
        .args(["show", "--timestamp=unix", &format!("--property={UNIT_PROPERTIES}")])
        .args(units)
        .kill_on_drop(true)
        .output()
        .await
        .context("systemctl show")?;
    if !out.status.success() {
        bail!("systemctl show: {}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim());
    }
    let infos = parse_show(&String::from_utf8_lossy(&out.stdout));
    ensure!(infos.len() == units.len(), "systemctl show answered for {} of {} units", infos.len(), units.len());
    Ok(infos)
}

/// Reads `systemctl show` output: one block of `Key=value` lines per unit,
/// blocks separated by an empty line.
#[must_use]
pub fn parse_show(out: &str) -> Vec<UnitInfo> {
    let mut infos: Vec<UnitInfo> = Vec::new();
    let mut in_block = false;
    for line in out.lines() {
        if line.is_empty() {
            in_block = false;
            continue;
        }
        if !in_block {
            infos.push(UnitInfo::default());
            in_block = true;
        }
        let Some(cur) = infos.last_mut() else { continue };
        let (key, value) = line.split_once('=').unwrap_or((line, ""));
        match key {
            "ActiveState" => cur.active = value.into(),
            "SubState" => cur.sub = value.into(),
            "ActiveEnterTimestamp" => cur.active_enter = unix_timestamp(value),
            "InactiveEnterTimestamp" => cur.inactive_enter = unix_timestamp(value),
            "NRestarts" => cur.restarts = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    infos
}

fn unix_timestamp(v: &str) -> Option<Timestamp> {
    match v.trim_start_matches('@').parse::<i64>() {
        Ok(n) if n != 0 => Timestamp::from_second(n).ok(),
        _ => None,
    }
}

/// What a workspace shows as. The unit restarts a VM that dies, so one
/// that keeps dying cycles through `activating`/`auto-restart`: that is
/// failing, not starting.
#[must_use]
pub fn derive_state(u: &UnitInfo, vm: &str, agent_up: bool) -> State {
    match u.active.as_str() {
        "failed" => State::Failing,
        _ if u.sub == "auto-restart" => State::Failing,
        "deactivating" => State::Stopping,
        "active" if vm == "Running" && agent_up => State::Running,
        "activating" | "reloading" | "active" => State::Starting,
        _ => State::Stopped,
    }
}

/// When the workspace entered the state it shows: a failed unit, when it
/// failed.
#[must_use]
pub fn since(u: &UnitInfo, state: State) -> Option<Timestamp> {
    match state {
        State::Stopped => u.inactive_enter,
        State::Failing if u.active == "failed" => u.inactive_enter.or(u.active_enter),
        _ => u.active_enter,
    }
}

/// Every configured workspace and the disks left by removed ones: systemd
/// for all of them in one call, then each running VM's API and agent at
/// once.
pub async fn collect(dirs: &[PathBuf], state_root: &Path) -> anyhow::Result<Vec<Workspace>> {
    let mut names = config::configured(dirs);
    if let Ok(entries) = std::fs::read_dir(state_root) {
        for e in entries.flatten() {
            let Some(name) = e.file_name().to_str().map(str::to_owned) else { continue };
            if is_workspace_name(&name) && !names.contains(&name) && e.path().join("disk.raw").is_file() {
                names.push(name);
            }
        }
    }
    names.sort();
    let units: Vec<String> = names.iter().map(|n| unit_name(n)).collect();
    let infos = show_units(&units).await?;
    let mut probes = tokio::task::JoinSet::new();
    for (i, (name, u)) in names.into_iter().zip(infos).enumerate() {
        let (dirs, state_root) = (dirs.to_vec(), state_root.to_owned());
        probes.spawn(async move { (i, probe(&dirs, &state_root, name, u).await) });
    }
    let mut all = probes.join_all().await;
    all.sort_by_key(|(i, _)| *i);
    Ok(all.into_iter().map(|(_, ws)| ws).collect())
}

/// One workspace, from its config, its unit, its VM's API and its agent.
async fn probe(dirs: &[PathBuf], state_root: &Path, name: String, u: UnitInfo) -> Workspace {
    let mut ws = Workspace { unit: u.active.clone(), vm: "-".into(), restarts: u.restarts, ..Workspace::default() };
    let mut disk = state_root.join(&name).join("disk.raw");
    let mut agent_up = false;
    if let Ok(c) = config::load(dirs, &name) {
        ws.configured = true;
        ws.self_service = c.self_service;
        ws.owner_left = crate::self_service::left_marker(state_root, &name).exists();
        ws.owner.clone_from(&c.owner);
        ws.url.clone_from(&c.url);
        ws.ip.clone_from(&c.ip);
        ws.os.clone_from(&c.os);
        disk = c.disk();
        ws.boot_memory_bytes = c.boot_mem_mib * MIB;
        ws.max_memory_bytes = c.max_mem_mib * MIB;
        let mut open_links = vec![];
        let mut running = None;
        if matches!(u.active.as_str(), "active" | "deactivating") {
            if let Ok(Ok(info)) = tokio::time::timeout(PROBE_TIMEOUT, Client::new(c.api_socket()).info()).await {
                ws.vm = info.state;
                ws.memory_bytes = info.memory_actual_size;
                match agent::ask(&c, &Request { op: "ping".into(), args: vec![] }, PROBE_TIMEOUT).await {
                    Ok(reply) => {
                        ws.agent = "up".into();
                        open_links = reply.open_links;
                        agent_up = true;
                    }
                    Err(_) => ws.agent = "down".into(),
                }
            }
            running = booted(&c);
        }
        let next = c.boot();
        ws.changes = apps::changes(running.as_ref(), &next);
        ws.apps = union(&next, running.as_ref())
            .map(|a| App {
                id: a.id.clone(),
                name: a.name.clone(),
                url: a.port.map(|_| app_url(&c.url, &a.id)),
                open_link: a.port.is_some() && open_links.contains(&a.id),
                connect: a.connect.clone(),
            })
            .collect();
    }
    ws.state = derive_state(&u, &ws.vm, agent_up);
    ws.since = since(&u, ws.state);
    if let Ok(m) = std::fs::metadata(&disk) {
        use std::os::unix::fs::MetadataExt;
        ws.disk_bytes = m.len();
        ws.disk_used_bytes = m.blocks() * 512;
    }
    ws.name = name;
    ws
}

/// What the running VM booted with, from the record `workbench run` left;
/// none before it is written. A VM a host from before apps started left
/// only its packages, and runs no app the host knows of.
#[must_use]
pub fn booted(c: &Config) -> Option<Boot> {
    if let Ok(raw) = hostutil::read_bounded(c.booted(), 1 << 20) {
        return serde_json::from_slice(&raw).ok();
    }
    let old = hostutil::read_bounded(Path::new(&c.runtime_dir).join("app"), 4096).ok()?;
    Some(Boot { packages: String::from_utf8_lossy(&old).trim().to_owned(), apps: vec![] })
}

/// The config's apps, then those only `running` has.
fn union<'a>(next: &'a Boot, running: Option<&'a Boot>) -> impl Iterator<Item = &'a BootApp> {
    let only_running = running.into_iter().flat_map(|r| &r.apps).filter(|a| next.app(&a.id).is_none());
    next.apps.iter().chain(only_running)
}

/// The apps a browser may reach now: the config's, and those the running
/// VM has that its next start leaves out.
#[must_use]
pub fn reachable_apps(c: &Config) -> Vec<BootApp> {
    let running = booted(c);
    union(&c.boot(), running.as_ref()).cloned().collect()
}

/// Keeps a stopped workspace stopped across host reboots: its unit has
/// `ConditionPathExists=!<marker>`. Names starting with a dot are never
/// workspaces, so `.stopped` cannot clash with one.
#[must_use]
pub fn stop_marker(state_root: &Path, name: &str) -> PathBuf {
    state_root.join(".stopped").join(name)
}

/// Starts, stops or restarts a workspace's unit. Stop leaves the marker
/// that keeps it stopped; start and restart remove it. Unless `wait`, it
/// returns once systemd has the job.
pub async fn control(state_root: &Path, verb: Verb, name: &str, wait: bool) -> anyhow::Result<()> {
    ensure!(is_workspace_name(name), "invalid workspace name {name:?}");
    let marker = stop_marker(state_root, name);
    match verb {
        Verb::Start | Verb::Restart => match std::fs::remove_file(&marker) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| marker.display().to_string());
            }
            _ => {}
        },
        Verb::Stop => {
            if let Some(dir) = marker.parent() {
                std::fs::create_dir_all(dir)?;
            }
            // The directory is the web UI's: root never follows a link there.
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
                .open(&marker)
                .with_context(|| marker.display().to_string())?;
        }
    }
    let unit = unit_name(name);
    let mut cmd = Command::new("systemctl");
    if !wait {
        cmd.arg("--no-block");
    }
    let out = cmd.args(["--no-ask-password", verb.as_str(), &unit]).kill_on_drop(true).output().await?;
    if !out.status.success() {
        let why = [out.stderr, out.stdout].concat();
        bail!("systemctl {verb} {unit}: {}: {}", out.status, String::from_utf8_lossy(&why).trim());
    }
    Ok(())
}

/// Why a workspace's app gave no sign-in link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenLinkError {
    /// The agent does not answer.
    Starting(String),
    /// The hook failed, with its exit code and what it printed on stderr.
    Failed { exit_code: i32, detail: String },
    /// The hook printed something that is not a link to the workspace.
    NotALink,
    /// No answer in time.
    Timeout,
}

impl std::fmt::Display for OpenLinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenLinkError::Starting(d) => write!(f, "the guest agent isn't answering: {d}"),
            OpenLinkError::Failed { exit_code: 0, detail } => write!(f, "the sign-in hook failed: {detail}"),
            OpenLinkError::Failed { exit_code, detail } => {
                write!(f, "the sign-in hook failed (exit status {exit_code}): {detail}")
            }
            OpenLinkError::NotALink => {
                f.write_str("the sign-in hook printed something that isn't a link to the workspace")
            }
            OpenLinkError::Timeout => f.write_str("the app took too long to answer"),
        }
    }
}

impl std::error::Error for OpenLinkError {}

/// Asks an app of the workspace, through the agent, for a link that signs
/// a browser in to it.
pub async fn open_link(c: &Config, app: &str) -> Result<String, OpenLinkError> {
    if c.apps.iter().any(|a| a.id == app && a.port.is_none()) {
        return Err(OpenLinkError::Failed { exit_code: 0, detail: "the app has no address".into() });
    }
    let req = Request { op: "open-link".into(), args: vec![app.to_owned()] };
    let reply = match agent::exchange(c, &req, OPEN_LINK_TIMEOUT).await {
        Err(agent::AskError::Timeout) => return Err(OpenLinkError::Timeout),
        Err(agent::AskError::Unreachable(e)) => return Err(OpenLinkError::Starting(format!("{e:#}"))),
        Ok(reply) => reply,
    };
    if !reply.error.is_empty() {
        return Err(OpenLinkError::Failed { exit_code: 0, detail: reply.error });
    }
    if reply.exit_code != 0 {
        return Err(OpenLinkError::Failed { exit_code: reply.exit_code, detail: reply.stderr.trim().to_owned() });
    }
    check_open_link(&reply.stdout, &app_url(&c.url, app)).ok_or(OpenLinkError::NotALink)
}

/// Accepts what the hook printed only if it is one absolute URL on the
/// app's own origin, so the UI cannot be made to send a browser anywhere
/// else.
#[must_use]
pub fn check_open_link(out: &str, base: &str) -> Option<String> {
    let link = out.trim();
    if link.is_empty() || link.contains(char::is_whitespace) {
        return None;
    }
    let (u, b) = (url::Url::parse(link).ok()?, url::Url::parse(base).ok()?);
    let same_origin = u.scheme() == b.scheme()
        && u.host_str().is_some()
        && u.host_str() == b.host_str()
        && u.port_or_known_default() == b.port_or_known_default();
    (same_origin && u.username().is_empty() && u.password().is_none() && !u.cannot_be_a_base()).then(|| u.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_show() {
        let out = "ActiveState=active\nSubState=running\nActiveEnterTimestamp=@1795018501\nInactiveEnterTimestamp=\nNRestarts=0\n\n\
                   ActiveState=activating\nSubState=auto-restart\nActiveEnterTimestamp=\nInactiveEnterTimestamp=@1795018000\nNRestarts=6\n";
        let infos = parse_show(out);
        assert_eq!(infos.len(), 2);
        assert_eq!(
            infos[0],
            UnitInfo {
                active: "active".into(),
                sub: "running".into(),
                active_enter: Some(Timestamp::from_second(1_795_018_501).unwrap()),
                inactive_enter: None,
                restarts: 0
            }
        );
        assert_eq!((infos[1].sub.as_str(), infos[1].restarts), ("auto-restart", 6));
        assert_eq!(infos[1].inactive_enter, Some(Timestamp::from_second(1_795_018_000).unwrap()));
    }

    #[test]
    fn derives_state() {
        for (active, sub, vm, agent, want) in [
            ("active", "running", "Running", true, State::Running),
            ("active", "running", "Running", false, State::Starting),
            ("active", "running", "-", false, State::Starting),
            ("activating", "start", "-", false, State::Starting),
            ("activating", "auto-restart", "-", false, State::Failing),
            ("failed", "failed", "-", false, State::Failing),
            ("deactivating", "stop-sigterm", "Running", true, State::Stopping),
            ("inactive", "dead", "-", false, State::Stopped),
        ] {
            let u = UnitInfo { active: active.into(), sub: sub.into(), ..UnitInfo::default() };
            assert_eq!(derive_state(&u, vm, agent), want, "{active}/{sub} vm {vm} agent {agent}");
        }
    }

    /// The UI redirects a browser to whatever the hook prints, so only a
    /// link on the workspace's own origin passes.
    #[test]
    fn open_link_check() {
        let base = "https://alice--t3code.ws.test";
        for (out, ok) in [
            ("https://alice--t3code.ws.test/pair#token=abc\n", true),
            ("https://alice--t3code.ws.test", true),
            ("https://alice.ws.test/pair#token=abc", false),
            ("https://alice--other.ws.test/", false),
            ("https://evil.test/?alice--t3code.ws.test", false),
            ("https://alice--t3code.ws.test.evil.test/", false),
            ("http://alice--t3code.ws.test/", false),
            ("https://alice--t3code.ws.test:8443/", false),
            ("https://user@alice--t3code.ws.test/", false),
            ("Pairing link: https://alice--t3code.ws.test/x", false),
            ("https://alice--t3code.ws.test/a\nhttps://b/", false),
            ("//evil.test/", false),
            ("javascript:alert(1)", false),
            ("", false),
        ] {
            assert_eq!(check_open_link(out, base).is_some(), ok, "{out:?}");
        }
    }

    #[test]
    fn since_when() {
        let (up, down) = (Timestamp::from_second(1000).unwrap(), Timestamp::from_second(2000).unwrap());
        let mut u = UnitInfo {
            active: "failed".into(),
            active_enter: Some(up),
            inactive_enter: Some(down),
            ..UnitInfo::default()
        };
        assert_eq!(since(&u, State::Failing), Some(down), "a failed unit since it failed");
        u.active = "active".into();
        assert_eq!(since(&u, State::Running), Some(up));
        assert_eq!(since(&u, State::Stopped), Some(down));
    }

    /// What the VM booted with: the record, or for one a host from before
    /// apps started, its packages and no app.
    #[test]
    fn booted_records() {
        let mut c = crate::config::tests::test_config();
        let dir = tempfile::tempdir().unwrap();
        c.runtime_dir = dir.path().to_string_lossy().into();
        assert_eq!(booted(&c), None);
        std::fs::write(dir.path().join("app"), "/nix/store/old-app\n").unwrap();
        let old = booted(&c).unwrap();
        assert_eq!((old.packages.as_str(), old.apps.len()), ("/nix/store/old-app", 0));
        assert_eq!(apps::changes(Some(&old), &c.boot()).len(), 2, "its app comes, and new packages");
        std::fs::write(c.booted(), serde_json::to_vec(&c.boot()).unwrap()).unwrap();
        assert_eq!(booted(&c), Some(c.boot()));
        let running = c.boot();
        c.apps.clear();
        std::fs::write(c.booted(), serde_json::to_vec(&running).unwrap()).unwrap();
        assert_eq!(reachable_apps(&c), running.apps, "an app that goes at the next start is reached until then");
    }

    #[test]
    fn stop_marker_is_no_workspace() {
        let m = stop_marker(Path::new("/var/lib/workbench"), "bob");
        assert_eq!(m, Path::new("/var/lib/workbench/.stopped/bob"));
        assert!(!is_workspace_name(".stopped"));
    }
}
