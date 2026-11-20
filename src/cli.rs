//! The ops commands on the host: `status`, `start`, `stop`, `restart`,
//! `open-link`, `console` and `destroy`. Only start, stop, restart and
//! destroy change a VM; the others talk to it through its sockets.

use std::fmt::Write as _;
use std::io::{self, BufRead, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use vmkit::hostutil;
use vmkit::memory::GIB;
use workbench_model::{Change, Stage, Verb, Workspace};

use crate::config::{self, is_workspace_name};
use crate::state::{self, unit_name};

/// How a command that reported an exit code ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitCode(pub i32);

impl std::fmt::Display for ExitCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit status {}", self.0)
    }
}

impl std::error::Error for ExitCode {}

/// Prints a link that signs a browser in to an app of the workspace: the
/// one named, or its only one.
pub async fn open_link(dirs: &[PathBuf], name: &str, app: Option<&str>) -> anyhow::Result<()> {
    let c = config::load(dirs, name)?;
    let ids: Vec<&str> = c.apps.iter().map(|a| a.id.as_str()).collect();
    let app = match (app, ids.as_slice()) {
        (Some(app), _) => app,
        (None, [only]) => only,
        (None, []) => bail!("{name} has no app"),
        (None, _) => bail!("{name} has several apps; name one of {}", ids.join(", ")),
    };
    let link = state::open_link(&c, app).await?;
    println!("{link}");
    Ok(())
}

/// The apps column: each app, and what the next start changes, `+` for
/// one it brings, `-` for one it leaves out, `*` for a newer one.
fn apps_cell(ws: &Workspace) -> String {
    let mut cells: Vec<String> = ws
        .apps
        .iter()
        .map(|a| {
            let mark = match ws.stage(&a.id) {
                Stage::Coming => "+",
                Stage::Going => "-",
                Stage::Updating => "*",
                Stage::Ready => "",
            };
            format!("{mark}{}", a.id)
        })
        .collect();
    if ws.changes.contains(&Change::Packages) {
        cells.push("*packages".into());
    }
    if cells.is_empty() { "-".into() } else { cells.join(",") }
}

/// Starts, stops or restarts a workspace and waits for systemd to finish.
pub async fn control(dirs: &[PathBuf], state_root: &Path, verb: Verb, name: &str) -> anyhow::Result<()> {
    // A workspace removed from the configuration may still be stopped.
    if verb != Verb::Stop {
        config::load(dirs, name)?;
    }
    state::control(state_root, verb, name, true).await
}

/// Prints the workspaces, as a table or as JSON.
pub async fn status(w: &mut impl Write, dirs: &[PathBuf], state_root: &Path, json: bool) -> anyhow::Result<()> {
    let all = state::collect(dirs, state_root).await?;
    if json {
        serde_json::to_writer_pretty(&mut *w, &all)?;
        writeln!(w)?;
        return Ok(());
    }
    let mut rows = vec![
        ["WORKSPACE", "OWNER", "IP", "STATE", "UNIT", "VM", "MEMORY", "DISK USED/SIZE", "AGENT", "APPS", "URL"]
            .map(String::from),
    ];
    for s in &all {
        let (owner, url) = if s.configured {
            (s.owner.clone(), s.url.clone())
        } else {
            ("-".into(), "(removed from the configuration; disk kept)".into())
        };
        let mem = if s.memory_bytes > 0 {
            format!("{:.1}/{:.0} GiB", gibs(s.memory_bytes), gibs(s.max_memory_bytes))
        } else {
            "-".into()
        };
        #[allow(clippy::cast_possible_wrap)]
        let disk = format!("{:.1}/{:.0} GiB", gibs(s.disk_used_bytes as i64), gibs(s.disk_bytes as i64));
        rows.push([
            s.name.clone(),
            owner,
            or_dash(&s.ip),
            s.state.to_string(),
            s.unit.clone(),
            s.vm.clone(),
            mem,
            disk,
            or_dash(&s.agent),
            apps_cell(s),
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

/// Where the web UI shares the consoles, when it runs.
pub const SHARED_CONSOLES: &str = "/run/workbench-web/console.sock";

/// Attaches the terminal to the VM's serial console until Ctrl-] is typed:
/// the web UI's shared one when it runs, the serial socket otherwise.
pub fn console(dirs: &[PathBuf], name: &str) -> anyhow::Result<()> {
    let c = config::load(dirs, name)?;
    let (conn, shared) = match shared_console(Path::new(SHARED_CONSOLES), name)? {
        Some(conn) => (conn, " (shared with the web UI)"),
        None => (UnixStream::connect(c.serial()).with_context(|| format!("serial console of {name}"))?, ""),
    };
    let raw = RawTerminal::new();
    eprint!("Connected to {name}{shared}; Ctrl-] quits.\r\n");
    attach(conn);
    drop(raw);
    Ok(())
}

/// Asks the web UI's shared consoles for one: the workspace's name and a
/// newline, then `OK` or `ERR <why>`. `None` when the UI does not run.
pub(crate) fn shared_console(socket: &Path, name: &str) -> anyhow::Result<Option<UnixStream>> {
    let Ok(mut conn) = UnixStream::connect(socket) else { return Ok(None) };
    conn.write_all(format!("{name}\n").as_bytes())?;
    // Byte by byte, so nothing the console sends after the answer is
    // buffered away.
    let mut answer = Vec::new();
    let mut byte = [0];
    while answer.len() < 4096 && conn.read(&mut byte)? == 1 && byte[0] != b'\n' {
        answer.push(byte[0]);
    }
    let answer = String::from_utf8_lossy(&answer);
    match answer.strip_prefix("ERR ") {
        _ if answer == "OK" => Ok(Some(conn)),
        Some(why) => bail!("the web UI's console of {name}: {why}"),
        None => bail!("the web UI's console of {name} answered {answer:?}"),
    }
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
    // One made at run time has more than its disk to take away.
    if declared.as_ref().is_some_and(|c| c.self_service) {
        let t = crate::self_service::Template::load(Path::new(crate::self_service::TEMPLATE))?;
        crate::self_service::destroy(&t, name).await?;
        writeln!(out, "Deleted {} and everything of {name}.", state_dir.display())?;
        return Ok(());
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
    // A stop marker would keep the fresh disk of a workspace still
    // declared from being made at the next boot.
    match std::fs::remove_file(state::stop_marker(state_root, name)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
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
    fn shared_console_handshake() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("console.sock");
        assert!(shared_console(&socket, "alice").unwrap().is_none(), "no UI, no shared console");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            for answer in ["OK\nboot log", "ERR no workspace \"bob\"\n"] {
                let (mut conn, _) = listener.accept().unwrap();
                let mut name = String::new();
                io::BufReader::new(&mut conn).read_line(&mut name).unwrap();
                conn.write_all(answer.as_bytes()).unwrap();
            }
        });
        let mut conn = shared_console(&socket, "alice").unwrap().unwrap();
        let mut rest = String::new();
        conn.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "boot log", "the console starts right after the answer");
        let err = shared_console(&socket, "bob").unwrap_err();
        assert!(err.to_string().contains("no workspace"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn table_aligns() {
        let rows = [["A", "BB", "C"].map(String::from), ["aaa", "b", "c"].map(String::from)];
        assert_eq!(table(&rows), "A    BB  C\naaa  b   c\n");
    }
}
