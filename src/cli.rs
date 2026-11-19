//! The ops commands on the host: `status`, `pair`, `console` and `destroy`.
//! They only talk to running VMs through their sockets, so they never stop
//! one by accident.

use std::fmt::Write as _;
use std::io::{self, BufRead, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use serde::Serialize;
use vmkit::ch::Client;
use vmkit::hostutil;
use vmkit::memory::{GIB, MIB};

use crate::agent::{self, Request};
use crate::config::{self, is_workspace_name};

/// The unit of a workspace.
#[must_use]
pub fn unit_name(name: &str) -> String {
    format!("workbench@{name}.service")
}

/// How a command that reported an exit code ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitCode(pub i32);

impl std::fmt::Display for ExitCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit status {}", self.0)
    }
}

impl std::error::Error for ExitCode {}

/// Runs `t3 auth pairing create --base-url <url>` as the owner inside the
/// guest and prints what it prints.
pub async fn pair(dirs: &[PathBuf], name: &str, args: Vec<String>) -> anyhow::Result<()> {
    let c = config::load(dirs, name)?;
    let reply = agent::ask(&c, &Request { op: "pair".into(), args }, Duration::from_secs(90)).await?;
    print!("{}", reply.stdout);
    eprint!("{}", reply.stderr);
    if reply.exit_code != 0 {
        return Err(ExitCode(reply.exit_code).into());
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceStatus {
    pub name: String,
    pub configured: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub owner: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub ip: String,
    pub unit: String,
    pub vm: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub memory_bytes: i64,
    #[serde(rename = "maxMemoryBytes", skip_serializing_if = "is_zero")]
    pub max_mem_bytes: i64,
    pub disk_bytes: u64,
    #[serde(rename = "diskUsedBytes")]
    pub disk_used: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub agent: String,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// A unit's `ActiveState`, or `unknown`.
pub async fn unit_state(unit: &str) -> String {
    let out = tokio::process::Command::new("systemctl")
        .args(["show", "--property=ActiveState", "--value", unit])
        .output()
        .await;
    match out {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        _ => "unknown".into(),
    }
}

/// The configured workspaces and the disks left by removed ones.
pub async fn statuses(dirs: &[PathBuf], state_root: &Path) -> Vec<WorkspaceStatus> {
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
    // Each VM may take seconds to answer: ask them all at once.
    let mut probes = tokio::task::JoinSet::new();
    for (i, name) in names.into_iter().enumerate() {
        let (dirs, state_root) = (dirs.to_vec(), state_root.to_owned());
        probes.spawn(async move { (i, probe(&dirs, &state_root, name).await) });
    }
    let mut all = probes.join_all().await;
    all.sort_by_key(|(i, _)| *i);
    all.into_iter().map(|(_, s)| s).collect()
}

/// One workspace, from its config, its unit, its VM's API and its agent.
async fn probe(dirs: &[PathBuf], state_root: &Path, name: String) -> WorkspaceStatus {
    let mut s = WorkspaceStatus { unit: unit_state(&unit_name(&name)).await, vm: "-".into(), ..Default::default() };
    let mut disk = state_root.join(&name).join("disk.raw");
    if let Ok(c) = config::load(dirs, &name) {
        s.configured = true;
        s.owner.clone_from(&c.owner);
        s.url.clone_from(&c.url);
        s.ip.clone_from(&c.ip);
        disk = c.disk();
        s.max_mem_bytes = c.max_mem_mib * MIB;
        let info = tokio::time::timeout(Duration::from_secs(2), Client::new(c.api_socket()).info()).await;
        if let Ok(Ok(info)) = info {
            s.vm = info.state;
            s.memory_bytes = info.memory_actual_size;
            let ping = agent::ask(&c, &Request { op: "ping".into(), args: vec![] }, Duration::from_secs(2)).await;
            s.agent = if ping.is_ok() { "up" } else { "down" }.into();
        }
    }
    if let Ok(m) = std::fs::metadata(&disk) {
        s.disk_bytes = m.len();
        s.disk_used = m.blocks() * 512;
    }
    s.name = name;
    s
}

/// Prints the workspaces, as a table or as JSON.
pub async fn status(w: &mut impl Write, dirs: &[PathBuf], state_root: &Path, json: bool) -> anyhow::Result<()> {
    let all = statuses(dirs, state_root).await;
    if json {
        serde_json::to_writer_pretty(&mut *w, &all)?;
        writeln!(w)?;
        return Ok(());
    }
    let mut rows =
        vec![["WORKSPACE", "OWNER", "IP", "UNIT", "VM", "MEMORY", "DISK USED/SIZE", "AGENT", "URL"].map(String::from)];
    for s in &all {
        let (owner, url) = if s.configured {
            (s.owner.clone(), s.url.clone())
        } else {
            ("-".into(), "(removed from the configuration; disk kept)".into())
        };
        let mem = if s.memory_bytes > 0 {
            format!("{:.1}/{:.0} GiB", gibs(s.memory_bytes), gibs(s.max_mem_bytes))
        } else {
            "-".into()
        };
        #[allow(clippy::cast_possible_wrap)]
        let disk = format!("{:.1}/{:.0} GiB", gibs(s.disk_used as i64), gibs(s.disk_bytes as i64));
        rows.push([
            s.name.clone(),
            owner,
            or_dash(&s.ip),
            s.unit.clone(),
            s.vm.clone(),
            mem,
            disk,
            or_dash(&s.agent),
            url,
        ]);
    }
    w.write_all(table(&rows).as_bytes())?;
    Ok(())
}

/// Aligns columns two spaces apart; the last one is not padded.
fn table<const N: usize>(rows: &[[String; N]]) -> String {
    let mut widths = [0; N];
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i + 1 == N {
                out += cell;
            } else {
                let _ = write!(out, "{cell:<0$}  ", widths[i]);
            }
        }
        out.push('\n');
    }
    out
}

#[allow(clippy::cast_precision_loss)]
fn gibs(b: i64) -> f64 {
    b as f64 / GIB as f64
}

fn or_dash(s: &str) -> String {
    if s.is_empty() { "-".into() } else { s.into() }
}

/// Attaches the terminal to the VM's serial port until Ctrl-] is typed.
pub fn console(dirs: &[PathBuf], name: &str) -> anyhow::Result<()> {
    let c = config::load(dirs, name)?;
    let conn = UnixStream::connect(c.serial()).with_context(|| format!("serial console of {name}"))?;
    let raw = RawTerminal::new();
    eprint!("Connected to {name}; Ctrl-] quits.\r\n");
    attach(conn);
    drop(raw);
    Ok(())
}

/// Copies the console both ways until either side ends or Ctrl-] is typed.
pub(crate) fn attach(conn: UnixStream) {
    let (done, finished) = std::sync::mpsc::channel();
    let mut from = conn.try_clone().expect("cloning a Unix socket");
    let out_done = done.clone();
    std::thread::spawn(move || {
        // Locking stdout only per write, so the end of the console never
        // waits on this thread.
        let mut buf = [0; 4096];
        while let Ok(n @ 1..) = from.read(&mut buf) {
            let mut out = io::stdout();
            if out.write_all(&buf[..n]).and_then(|()| out.flush()).is_err() {
                break;
            }
        }
        let _ = out_done.send(());
    });
    let mut to = conn;
    std::thread::spawn(move || {
        let mut buf = [0; 1024];
        let mut stdin = io::stdin().lock();
        loop {
            let n = match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if let Some(i) = buf[..n].iter().position(|&b| b == 0x1d) {
                let _ = to.write_all(&buf[..i]);
                break;
            }
            if to.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        let _ = done.send(());
    });
    let _ = finished.recv();
}

/// Puts stdin's terminal in raw mode until dropped; nothing if stdin is no
/// terminal.
struct RawTerminal(Option<nix::sys::termios::Termios>);

impl RawTerminal {
    fn new() -> Self {
        use nix::sys::termios::{SetArg, cfmakeraw, tcgetattr, tcsetattr};
        let stdin = io::stdin();
        let Ok(old) = tcgetattr(&stdin) else { return RawTerminal(None) };
        let mut raw = old.clone();
        cfmakeraw(&mut raw);
        if tcsetattr(&stdin, SetArg::TCSANOW, &raw).is_err() {
            return RawTerminal(None);
        }
        RawTerminal(Some(old))
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        if let Some(old) = &self.0 {
            let _ = nix::sys::termios::tcsetattr(io::stdin(), nix::sys::termios::SetArg::TCSANOW, old);
        }
    }
}

/// Stops a workspace and deletes its disk, after the name is typed again
/// unless `yes`. It is the only way workspace data is deleted.
pub async fn destroy(
    dirs: &[PathBuf],
    state_root: &Path,
    name: &str,
    yes: bool,
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    ensure!(is_workspace_name(name), "invalid workspace name {name:?}");
    let declared = config::load(dirs, name).ok();
    let state_dir = declared.as_ref().map_or_else(|| state_root.join(name), |c| PathBuf::from(&c.state_dir));
    ensure!(state_dir.exists(), "workspace {name} has no data in {}", state_dir.display());
    if !yes {
        write!(
            out,
            "This stops workspace {name} and deletes {}, its disk and everything in it.\nType the workspace name to confirm: ",
            state_dir.display()
        )?;
        out.flush()?;
        let mut answer = String::new();
        let _ = input.read_line(&mut answer);
        ensure!(answer.trim() == name, "not confirmed; nothing deleted");
    }
    let unit = unit_name(name);
    let stop = tokio::process::Command::new("systemctl").args(["stop", &unit]).output().await?;
    if !stop.status.success() {
        bail!("stopping {unit}: {}: {}", stop.status, String::from_utf8_lossy(&stop.stderr).trim());
    }
    // An error counts as running: nothing is deleted under a VM.
    if hostutil::unit_running(&unit).await.unwrap_or(true) {
        bail!("{unit} is still running or about to start; nothing deleted");
    }
    std::fs::remove_dir_all(&state_dir)?;
    writeln!(out, "Deleted {}.", state_dir.display())?;
    if declared.is_some() {
        writeln!(
            out,
            "{name} is still declared: its next start makes a fresh disk. Remove it from the configuration to keep it gone."
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// destroy deletes nothing unless the workspace name is typed back.
    #[tokio::test]
    async fn destroy_needs_confirmation() {
        let root = tempfile::tempdir().unwrap();
        let disk = root.path().join("bob/disk.raw");
        std::fs::create_dir_all(disk.parent().unwrap()).unwrap();
        std::fs::write(&disk, "data").unwrap();
        let nowhere = tempfile::tempdir().unwrap();
        let dirs = [nowhere.path().to_owned()];
        let mut out = Vec::new();
        for answer in ["", "alice\n", "bo\n", "yes\n"] {
            let err = destroy(&dirs, root.path(), "bob", false, &mut answer.as_bytes(), &mut out).await.unwrap_err();
            assert!(err.to_string().contains("not confirmed"), "answer {answer:?}: {err}");
        }
        assert!(disk.exists(), "disk deleted without confirmation");
        assert!(String::from_utf8_lossy(&out).contains("Type the workspace name"));
        assert!(
            destroy(&[], root.path(), "../etc", true, &mut &b""[..], &mut out).await.is_err(),
            "destroyed a path outside the state root"
        );
        let err = destroy(&[], root.path(), "carol", true, &mut &b""[..], &mut out).await.unwrap_err();
        assert!(err.to_string().contains("no data"), "destroying an unknown workspace: {err}");
    }

    #[test]
    fn table_aligns() {
        let rows = [["A", "BB", "C"].map(String::from), ["aaa", "b", "c"].map(String::from)];
        assert_eq!(table(&rows), "A    BB  C\naaa  b   c\n");
    }
}
