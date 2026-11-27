//! The ops commands on a node: `status`, `start`, `stop`, `restart`,
//! `open-link`, `console` and `destroy`, and `move` on the front. Only
//! start, stop, restart and destroy change a VM; the others talk to it
//! through its sockets.

use std::fmt::Write as _;
use std::io::{self, BufRead, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use vmkit::memory::GIB;
use workbench_model::{Change, Stage, Verb, Workspace};

use workbench_model::self_service::is_reserved;

use crate::config::{self, Config, is_workspace_name};
use crate::fleet::front::Fleet;
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
/// one named, or its only one with an address.
pub async fn open_link(dirs: &[PathBuf], name: &str, app: Option<&str>) -> anyhow::Result<()> {
    let c = config::load(dirs, name)?;
    let app = match app {
        Some(app) => app,
        None => only_app(&c)?,
    };
    let link = state::open_link(&c, app).await?;
    println!("{link}");
    Ok(())
}

/// The one app of a workspace that a browser opens, at an address.
fn only_app(c: &config::Config) -> anyhow::Result<&str> {
    let ids: Vec<&str> = c.apps.iter().filter(|a| a.port.is_some()).map(|a| a.id.as_str()).collect();
    match ids.as_slice() {
        [only] => Ok(only),
        [] => bail!("{} has no app opened in a browser", c.name),
        _ => bail!("{} has several apps; name one of {}", c.name, ids.join(", ")),
    }
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

/// Attaches the terminal to the VM's serial console until Ctrl-] is typed:
/// the one the node shares with the front, or the serial socket while
/// `workbench-node` doesn't run.
pub fn console(dirs: &[PathBuf], name: &str) -> anyhow::Result<()> {
    let c = config::load(dirs, name)?;
    let (conn, shared) = match shared_console(Path::new(crate::fleet::NODE_CONSOLES), name)? {
        Some(conn) => (conn, " (shared with the front)"),
        None => (UnixStream::connect(c.serial()).with_context(|| format!("serial console of {name}"))?, ""),
    };
    let raw = RawTerminal::new();
    eprint!("Connected to {name}{shared}; Ctrl-] quits.\r\n");
    attach(conn);
    drop(raw);
    Ok(())
}

/// Asks the node's shared consoles for one: the workspace's name and a
/// newline, then `OK` or `ERR <why>`. `None` when the node's API doesn't
/// run.
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
        Some(why) => bail!("the shared console of {name}: {why}"),
        None => bail!("the shared console of {name} answered {answer:?}"),
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

/// `workbench move <ws> <node>` on a front: asks for the move as the web
/// UI does, runs `workbench-move@<ws>` and says how it went.
pub async fn request_move(fleet: &Fleet, name: &str, node: &str, out: &mut impl Write) -> anyhow::Result<()> {
    let asked = std::time::SystemTime::now();
    fleet.ask_move(name, node)?;
    let unit = format!("workbench-move@{name}.service");
    let ran = crate::self_service::run_tool("systemctl", &["start", &unit]).await;
    // An outcome from an earlier move says nothing of this one.
    match (crate::self_service::read_outcome_since(&fleet.paths().move_outcome(name), asked), ran) {
        (Some(o), _) if o.ok => {
            writeln!(out, "{name} is on {node}.")?;
            Ok(())
        }
        (Some(o), _) => bail!("{}", o.error),
        (None, Ok(())) => bail!("{unit} said nothing of the move"),
        (None, Err(e)) => Err(e.context("and it said nothing of the move")),
    }
}

/// Stops a workspace and deletes its disk, after the name is typed again
/// unless `yes`. It is the only way workspace data is deleted. One made at
/// run time is the front's: on the front it goes as the web UI's Destroy
/// takes it, with its record, secrets and host key; on a node it is
/// refused.
pub async fn destroy(
    dirs: &[PathBuf],
    state_root: &Path,
    front: Option<&Fleet>,
    name: &str,
    yes: bool,
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    ensure!(is_workspace_name(name), "invalid workspace name {name:?}");
    // `registry`, `closed`, `ssh`… are the state directory's own.
    ensure!(!is_reserved(name), "{name} is one of the state directory's own names, not a workspace");
    if let Some(fleet) = front
        && let Some(r) = fleet.record(name).ok().filter(|r| !r.declared && !r.owner.is_empty())
    {
        if r.is_moving() {
            return Err(crate::fleet::front::moving(name));
        }
        let what = format!(
            "This stops workspace {name} and deletes it: its disk and everything in it on node {}, its record, its secrets and its host key.",
            r.node
        );
        confirm(&what, name, yes, input, out)?;
        fleet.take_away(name).await?;
        writeln!(out, "Deleted {name} and everything of it, on node {} and here.", r.node)?;
        return Ok(());
    }
    let declared = config::load(dirs, name).ok();
    // One made at run time is the front's: its record would outlive it.
    ensure!(
        !declared.as_ref().is_some_and(|c| c.self_service),
        "{name} was made at run time: destroy it on the front, `sudo workbench destroy {name}`, or in the web UI"
    );
    let state_dir = declared.as_ref().map_or_else(|| state_root.join(name), |c| PathBuf::from(&c.state_dir));
    ensure!(state_dir.exists(), "workspace {name} has no data in {}", state_dir.display());
    let what =
        format!("This stops workspace {name} and deletes {}, its disk and everything in it.", state_dir.display());
    confirm(&what, name, yes, input, out)?;
    let unit = unit_name(name);
    crate::self_service::run_tool("systemctl", &["stop", &unit]).await.with_context(|| format!("stopping {unit}"))?;
    // An error counts as running: nothing is deleted under a VM.
    if crate::self_service::unit_running(&unit).await {
        bail!("{unit} is still running or about to start; nothing deleted");
    }
    std::fs::remove_dir_all(&state_dir)?;
    // Its SSH host key goes with it, here or on the front of the same
    // machine: a workspace made again with its name is another machine.
    let keys = declared.as_ref().and_then(Config::host_key_dir).or_else(|| front?.host_key_dir(name));
    if let Some(keys) = keys {
        match std::fs::remove_dir_all(&keys) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
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

/// Says what a destroy deletes and has the name typed again, unless `yes`.
fn confirm(what: &str, name: &str, yes: bool, input: &mut impl BufRead, out: &mut impl Write) -> anyhow::Result<()> {
    if yes {
        return Ok(());
    }
    write!(out, "{what}\nType the workspace name to confirm: ")?;
    out.flush()?;
    let mut answer = String::new();
    let _ = input.read_line(&mut answer);
    ensure!(answer.trim() == name, "not confirmed; nothing deleted");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// destroy deletes nothing unless the workspace name is typed back.
    /// Without a name, the one app a browser opens: an app reached from a
    /// terminal has no link.
    #[test]
    fn the_app_to_open() {
        let mut c = crate::config::tests::test_config();
        assert_eq!(only_app(&c).unwrap(), "t3code");
        let terminal = workbench_model::apps::BootApp { id: "herdr".into(), port: None, ..c.apps[0].clone() };
        c.apps.push(terminal);
        assert_eq!(only_app(&c).unwrap(), "t3code", "Herdr has no address");
        c.apps.push(workbench_model::apps::BootApp { id: "notes".into(), port: Some(8080), ..c.apps[0].clone() });
        assert!(only_app(&c).unwrap_err().to_string().contains("name one of t3code, notes"));
        c.apps.retain(|a| a.port.is_none());
        assert!(only_app(&c).unwrap_err().to_string().contains("no app opened in a browser"));
    }

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
            let err =
                destroy(&dirs, root.path(), None, "bob", false, &mut answer.as_bytes(), &mut out).await.unwrap_err();
            assert!(err.to_string().contains("not confirmed"), "answer {answer:?}: {err}");
        }
        assert!(disk.exists(), "disk deleted without confirmation");
        assert!(String::from_utf8_lossy(&out).contains("Type the workspace name"));
        assert!(
            destroy(&[], root.path(), None, "../etc", true, &mut &b""[..], &mut out).await.is_err(),
            "destroyed a path outside the state root"
        );
        let err = destroy(&[], root.path(), None, "carol", true, &mut &b""[..], &mut out).await.unwrap_err();
        assert!(err.to_string().contains("no data"), "destroying an unknown workspace: {err}");
    }

    /// On a node, a workspace made at run time is refused: the front keeps
    /// its record.
    #[tokio::test]
    async fn one_made_at_run_time_is_the_fronts_to_destroy() {
        let root = tempfile::tempdir().unwrap();
        let configs = root.path().join("configs");
        std::fs::create_dir_all(&configs).unwrap();
        let mut c = crate::config::tests::test_config();
        c.state_dir = root.path().join("alice-dev").display().to_string();
        c.self_service = true;
        std::fs::create_dir_all(&c.state_dir).unwrap();
        std::fs::write(configs.join("alice-dev.json"), serde_json::to_vec(&c).unwrap()).unwrap();
        let mut out = Vec::new();
        let err = destroy(&[configs], root.path(), None, "alice-dev", true, &mut &b""[..], &mut out).await.unwrap_err();
        assert!(err.to_string().contains("destroy it on the front, `sudo workbench destroy alice-dev`"), "{err}");
        assert!(Path::new(&c.state_dir).exists(), "nothing deleted");
    }

    /// On the front, a workspace made at run time goes as the web UI's
    /// Destroy takes it: on its node, and its record, secrets and host key
    /// on the front, once its name is typed again; not while it moves.
    #[tokio::test]
    async fn on_the_front_one_made_at_run_time_goes_with_its_record() {
        use workbench_model::fleet::{Move, Phase, Record};
        let dir = tempfile::tempdir().unwrap();
        let a = crate::fleet::fake::FakeNode::new("a", dir.path().join("a"), 14);
        let fleet = crate::fleet::fake::fleet(crate::fleet::tests::front_settings(dir.path()), &[&a]);
        let r = crate::fleet::registry::tests::record("alice-dev", "a", "10.232.0.128");
        fleet.registry.put(&r).unwrap();
        a.has(&r, true, b"disk");
        let secrets = dir.path().join("secrets/alice-dev");
        std::fs::create_dir_all(&secrets).unwrap();
        let nowhere = dir.path().join("nowhere");
        let mut out = Vec::new();
        let gone = |a: &crate::fleet::fake::FakeNode| a.ws("alice-dev").is_none();

        let err = destroy(&[], &nowhere, Some(&fleet), "alice-dev", false, &mut &b"alice\n"[..], &mut out).await;
        assert!(err.unwrap_err().to_string().contains("not confirmed"));
        assert!(String::from_utf8_lossy(&out).contains("on node a, its record"), "it says what goes");
        let moving = Move {
            from: "a".into(),
            from_ip: "10.232.0.128".into(),
            to: "b".into(),
            ip: "10.232.1.128".into(),
            phase: Phase::Copy,
            ran: true,
        };
        fleet.registry.put(&Record { moving: Some(moving), ..r.clone() }).unwrap();
        let err = destroy(&[], &nowhere, Some(&fleet), "alice-dev", true, &mut &b""[..], &mut out).await;
        assert!(err.unwrap_err().to_string().contains("moving"), "refused while it moves");
        assert!(!gone(&a) && fleet.registry.get("alice-dev").unwrap().is_some(), "nothing deleted");

        fleet.registry.put(&r).unwrap();
        destroy(&[], &nowhere, Some(&fleet), "alice-dev", false, &mut &b"alice-dev\n"[..], &mut out).await.unwrap();
        assert!(gone(&a), "its node deleted it");
        assert!(fleet.registry.get("alice-dev").unwrap().is_none() && !secrets.exists(), "and the front its record");
    }

    /// The state directory's own names are no workspace's: `destroy
    /// registry` deletes nothing.
    #[tokio::test]
    async fn no_workspace_is_named_as_the_state_directory_s_own() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("registry")).unwrap();
        let mut out = Vec::new();
        for name in ["registry", "closed", "ssh", "configs"] {
            let err = destroy(&[], root.path(), None, name, true, &mut &b""[..], &mut out).await.unwrap_err();
            assert!(err.to_string().contains("state directory's own names"), "{name}: {err}");
        }
        assert!(root.path().join("registry").exists());
    }

    /// A declared workspace's host key, on the front of the same machine,
    /// goes with its disk: one made again with its name is another
    /// machine.
    #[tokio::test]
    async fn a_declared_workspace_goes_with_its_host_key() {
        let dir = tempfile::tempdir().unwrap();
        let a = crate::fleet::fake::FakeNode::new("a", dir.path().join("a"), 14);
        let mut settings = crate::fleet::tests::front_settings(dir.path());
        let hosts = dir.path().join("hosts");
        settings.ssh = config::SshCa {
            keygen: "/bin/ssh-keygen".into(),
            host_ca: "/ca/host-ca".into(),
            user_ca: "/ca/user-ca.pub".into(),
            host_keys: hosts.display().to_string(),
        };
        let fleet = crate::fleet::fake::fleet(settings, &[&a]);
        let configs = dir.path().join("configs");
        std::fs::create_dir_all(&configs).unwrap();
        let mut c = crate::config::tests::test_config();
        c.state_dir = dir.path().join("alice-dev").display().to_string();
        c.ssh = config::SshCa::default();
        std::fs::write(configs.join("alice-dev.json"), serde_json::to_vec(&c).unwrap()).unwrap();
        std::fs::create_dir_all(&c.state_dir).unwrap();
        std::fs::create_dir_all(hosts.join("alice-dev")).unwrap();
        // systemctl stops it, and says it no longer runs.
        crate::self_service::tools::stub(|_, args| {
            anyhow::ensure!(args.first() != Some(&"is-active"), "inactive");
            Ok(())
        });
        let mut out = Vec::new();
        destroy(&[configs], dir.path(), Some(&fleet), "alice-dev", true, &mut &b""[..], &mut out).await.unwrap();
        assert!(!Path::new(&c.state_dir).exists(), "its disk");
        assert!(!hosts.join("alice-dev").exists(), "its host key");
    }

    /// `workbench move` says how this move went, not an earlier one.
    #[tokio::test]
    async fn a_move_says_how_this_one_went() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (
            crate::fleet::fake::FakeNode::new("a", dir.path().join("a"), 14),
            crate::fleet::fake::FakeNode::new("b", dir.path().join("b"), 34),
        );
        let fleet = crate::fleet::fake::fleet(crate::fleet::tests::front_settings(dir.path()), &[&a, &b]);
        let outcome = fleet.paths().move_outcome("alice");
        std::fs::create_dir_all(outcome.parent().unwrap()).unwrap();
        std::fs::write(&outcome, br#"{"ok":true,"error":""}"#).unwrap();
        let last_week = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 24 * 3600);
        std::fs::File::options().write(true).open(&outcome).unwrap().set_modified(last_week).unwrap();
        crate::self_service::tools::stub(|_, _| Ok(()));
        let mut out = Vec::new();
        let err = request_move(&fleet, "alice", "b", &mut out).await.unwrap_err();
        assert!(err.to_string().contains("said nothing of the move"), "{err}");
        let written = outcome.clone();
        crate::self_service::tools::stub(move |_, _| {
            std::fs::write(&written, br#"{"ok":false,"error":"Node b is closed: it takes no moves."}"#)?;
            Ok(())
        });
        let err = request_move(&fleet, "alice", "b", &mut out).await.unwrap_err();
        assert!(err.to_string().contains("is closed"), "{err}");
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
        let err = request_move(&fleet, "registry", "b", &mut out).await.unwrap_err();
        assert!(err.to_string().contains("state directory's own names"), "{err}");
    }

    #[test]
    fn shared_console_handshake() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("console.sock");
        assert!(shared_console(&socket, "alice").unwrap().is_none(), "no node's API, no shared console");
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
