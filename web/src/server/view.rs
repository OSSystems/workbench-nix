//! What the server tells pages about times and memory, in the host's time
//! zone: the browser shows these as they are.

use jiff::{SignedDuration, Timestamp, ZonedDateTime, tz::TimeZone};
use vmkit::hostutil;
use vmkit::memory::{self, Status};
use workbench_model::{HostView, PoolView, Row, Stage, State, Workspace};

use super::Options;

/// A workspace as the list shows it.
pub fn row(ws: Workspace) -> Row {
    let now = Timestamp::now();
    let since = ws.since.map(|t| when(t, now)).unwrap_or_default();
    let note = note(&ws, &since);
    Row { ws, note, since }
}

/// The short text after a workspace's name in its row.
fn note(ws: &Workspace, since: &str) -> String {
    match ws.state {
        _ if !ws.configured => "removed from the configuration · disk kept".into(),
        State::Stopped if ws.owner_left => "owner left · disk kept".into(),
        State::Running => apps_note(ws),
        State::Starting => "booting".into(),
        State::Restarting => "restarting · shutting down the guest".into(),
        State::Stopping => "stopping · shutting down the guest".into(),
        State::Failing if ws.restarts > 0 => format!("restarted {} times", ws.restarts),
        State::Failing => "the VM won't stay up".into(),
        State::Stopped if !since.is_empty() => format!("since {since}"),
        State::Stopped => String::new(),
    }
}

/// A running workspace's apps, and those that wait for a restart.
fn apps_note(ws: &Workspace) -> String {
    if ws.apps.is_empty() {
        return "no apps".into();
    }
    let names: Vec<String> = ws
        .apps
        .iter()
        .map(|a| match ws.stage(&a.id) {
            Stage::Coming => format!("{} after a restart", a.name),
            _ => a.name.clone(),
        })
        .collect();
    names.join(" · ")
}

fn local(t: Timestamp) -> ZonedDateTime {
    t.to_zoned_datetime(TimeZone::system())
}

/// When something happened, as briefly as is clear: the time today, the
/// weekday this week, the date before.
#[must_use]
pub fn when(t: Timestamp, now: Timestamp) -> String {
    let (z, today) = (local(t), local(now));
    let age = now.duration_since(t);
    if age < SignedDuration::from_hours(20) && z.date() == today.date() {
        z.strftime("%H:%M").to_string()
    } else if age < SignedDuration::from_hours(6 * 24) {
        z.strftime("%a %H:%M").to_string()
    } else if z.year() == today.year() {
        z.strftime("%b %-d").to_string()
    } else {
        z.strftime("%b %-d, %Y").to_string()
    }
}

/// How long ago something happened.
#[must_use]
pub fn ago(t: Timestamp, now: Timestamp) -> String {
    let d = now.duration_since(t);
    let s = d.as_secs();
    if s < 60 {
        format!("{} s ago", s.max(0))
    } else if s < 3600 {
        format!("{} min ago", s / 60)
    } else if s < 48 * 3600 {
        format!("{} h ago", s / 3600)
    } else {
        when(t, now)
    }
}

/// How long ago something happened, from seconds since the epoch.
#[must_use]
pub fn ago_s(at: i64, now: Timestamp) -> String {
    Timestamp::from_second(at).map(|t| ago(t, now)).unwrap_or_default()
}

/// The host page: its memory, each pool's share and the running
/// workspaces'.
pub fn host(opts: &Options, all: &[Workspace]) -> anyhow::Result<HostView> {
    let now = Timestamp::now();
    let info = hostutil::meminfo_kib()?;
    let field = |k: &str| info.get(k).copied().unwrap_or(0) << 10;
    let (total, available) = (field("MemTotal"), field("MemAvailable"));
    let reserve = memory::reserve(total, opts.reserve_percent, opts.reserve_min_gib);
    let pools = opts.pools.iter().map(|(name, file)| pool(name, file, now)).collect();
    Ok(HostView {
        total,
        available,
        reserve,
        free: (available - reserve).max(0),
        pools,
        workspaces: all.iter().filter(|ws| ws.memory_bytes > 0).cloned().collect(),
        at: local(now).strftime("%H:%M:%S").to_string(),
        quotas: None,
    })
}

/// One pool, from its status file; a stale one still shows when it last
/// reported.
fn pool(name: &str, file: &std::path::Path, now: Timestamp) -> PoolView {
    let status = hostutil::read_bounded(file, memory::STATUS_MAX_BYTES)
        .ok()
        .and_then(|raw| serde_json::from_slice::<Status>(&raw).ok());
    let mut p = PoolView { name: name.to_owned(), ..PoolView::default() };
    match status {
        None => {
            p.stale = true;
            p.report = "no data".into();
        }
        Some(s) => {
            p.committed = s.committed_bytes;
            p.pending = s.pending_bytes;
            p.denied = s.denied_at.map(|d| ago(d, now)).unwrap_or_default();
            p.stale = now.duration_since(s.updated_at) > memory::STATUS_STALE;
            p.report =
                if p.stale { format!("no data since {}", when(s.updated_at, now)) } else { ago(s.updated_at, now) };
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes() {
        let mut ws = Workspace {
            name: "alice".into(),
            configured: true,
            url: "https://alice.ws.test".into(),
            state: State::Running,
            ..Workspace::default()
        };
        assert_eq!(note(&ws, ""), "no apps");
        let app =
            |id: &str, name: &str| workbench_model::App { id: id.into(), name: name.into(), ..Default::default() };
        ws.apps = vec![app("t3code", "T3 Code"), app("herdr", "herdr")];
        ws.changes = vec![workbench_model::Change::AppAdded { app: "herdr".into() }];
        assert_eq!(note(&ws, ""), "T3 Code · herdr after a restart");
        ws.state = State::Failing;
        ws.restarts = 6;
        assert_eq!(note(&ws, ""), "restarted 6 times");
        ws.state = State::Stopped;
        assert_eq!(note(&ws, "Nov 3"), "since Nov 3");
        ws.owner_left = true;
        assert_eq!(note(&ws, "Nov 3"), "owner left · disk kept");
        ws.configured = false;
        assert_eq!(note(&ws, ""), "removed from the configuration · disk kept");
    }

    #[test]
    fn ages() {
        let now: Timestamp = "2026-11-19T12:00:00Z".parse().unwrap();
        assert_eq!(ago(now - SignedDuration::from_secs(5), now), "5 s ago");
        assert_eq!(ago(now - SignedDuration::from_mins(4), now), "4 min ago");
        assert_eq!(ago(now - SignedDuration::from_hours(3), now), "3 h ago");
    }

    #[test]
    fn stale_pools() {
        let dir = tempfile::tempdir().unwrap();
        let now = Timestamp::now();
        let file = dir.path().join("p.json");
        let p = pool("p", &file, now);
        assert!(p.stale && p.report == "no data");
        let old = Status {
            updated_at: now - SignedDuration::from_mins(5),
            committed_bytes: 1,
            pending_bytes: 0,
            denied_at: None,
        };
        memory::write_status(&file, &old).unwrap();
        let p = pool("p", &file, now);
        assert!(p.stale && p.report.starts_with("no data since"), "{p:?}");
        let fresh = Status { updated_at: now, committed_bytes: 1, pending_bytes: 2, denied_at: Some(now) };
        memory::write_status(&file, &fresh).unwrap();
        let p = pool("p", &file, now);
        assert!(!p.stale && p.pending == 2 && p.denied == "0 s ago", "{p:?}");
    }
}
