//! The agent inside the guest, and the host's side of it. The agent sets
//! memory hotplug up, starts the apps, reports memory on the control port
//! and answers the host on the agent port, which only the host reaches. Its
//! requests and replies are a contract: see `docs/contracts.md`.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use std::{fs, io};

use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use vmkit::{control, hostutil, vsock};

use crate::config::Config;
use crate::launch::{GuestApp, GuestConfig};

/// Where the guest mounts the workspace's packages.
pub const GUEST_PACKAGES: &str = "/run/workspace/config/app";
/// Where the guest mounts the apps' bundles, each at its id. One runs its
/// `bin/serve`, and may have `bin/open-link`, a hook that, run as the owner,
/// prints one URL that signs a browser in to the app, and nothing else.
pub const GUEST_APPS: &str = "/run/workspace/config/apps";
/// Where the guest mounts what the host shares: the packages, the agent
/// at `agent` and the apps.
const GUEST_CONFIG: &str = "/run/workspace/config";
/// Where a guest with a Nix store of its own keeps its GC roots.
const GC_ROOTS: &str = "/nix/var/nix/gcroots";
/// The largest request the agent reads.
const MAX_REQUEST: u64 = 64 << 10;
/// The largest reply the host reads.
const MAX_REPLY: u64 = 1 << 20;
/// How long a host request may take to arrive.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an app's sign-in hook may run.
const HOOK_TIMEOUT: Duration = Duration::from_secs(60);
/// The most of a hook's stdout and stderr each kept.
const MAX_OUTPUT: u64 = 64 << 10;
/// How often the guest reports its memory.
const STATS_EVERY: Duration = Duration::from_secs(2);

/// What the host asks the agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub op: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

/// The agent's answer: what a command printed and how it exited, or why
/// the request failed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reply {
    /// In a ping's answer: the apps with a sign-in hook.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_links: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stdout: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stderr: String,
    #[serde(default)]
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

impl Reply {
    fn error(reason: impl Into<String>) -> Self {
        Reply { error: reason.into(), ..Reply::default() }
    }
}

/// The agent, as root in the guest.
pub async fn run(config_path: &Path) -> anyhow::Result<()> {
    let raw = hostutil::read_bounded(config_path, 1 << 20).with_context(|| config_path.display().to_string())?;
    let gc: GuestConfig = serde_json::from_slice(&raw).context("workspace.json")?;
    configure_hotplug();
    if let Err(e) = root_shared(Path::new(GUEST_CONFIG), Path::new(GC_ROOTS)) {
        eprintln!("rooting the agent and the apps: {e}");
    }
    let control_port = u32::try_from(gc.control_port).context("controlPort")?;
    let agent_port = u32::try_from(gc.agent_port).context("agentPort")?;
    tokio::spawn(report_stats(control_port));
    let listener = vsock::listen(agent_port)?;
    start_apps(&gc).await;
    loop {
        match listener.accept().await {
            Ok((conn, _)) => {
                let gc = gc.clone();
                tokio::spawn(async move { serve(conn, &gc).await });
            }
            // Out of descriptors, or a connection gone before it was
            // taken: the agent keeps listening.
            Err(e) => {
                eprintln!("accepting a host connection: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Keeps the agent and the apps' bundles in a guest's own store, which has
/// a copy of them: the packages refer to neither, and a guest module from
/// before roots only the packages. Roots of apps the VM no longer has go.
/// Nothing to do without a store.
fn root_shared(config: &Path, gcroots: &Path) -> io::Result<()> {
    if !gcroots.is_dir() {
        return Ok(());
    }
    let mut roots = Vec::new();
    match fs::read_link(config.join("agent")) {
        Ok(target) => roots.push(("workspace-agent".to_owned(), target)),
        // A host from before the agent was apart shares none.
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if let Ok(apps) = fs::read_dir(config.join("apps")) {
        // One the host shares wrong roots nothing, and keeps nothing else
        // from being rooted.
        for app in apps.flatten() {
            if let Ok(target) = fs::read_link(app.path()) {
                roots.push((format!("workspace-app-{}", app.file_name().to_string_lossy()), target));
            }
        }
    }
    for old in fs::read_dir(gcroots)? {
        let name = old?.file_name().to_string_lossy().into_owned();
        if name.starts_with("workspace-app-") && !roots.iter().any(|(n, _)| *n == name) {
            fs::remove_file(gcroots.join(&name))?;
        }
    }
    let tmp = gcroots.join(".workspace-root.tmp");
    for (name, target) in roots {
        let _ = fs::remove_file(&tmp);
        std::os::unix::fs::symlink(&target, &tmp)?;
        fs::rename(&tmp, gcroots.join(name))?;
    }
    Ok(())
}

/// Starts each app as a unit of its own, as the owner, the way one of the
/// guest's services would run: systemd keeps it up and its journal is the
/// app's. A unit an agent that ran before started stays as it is.
async fn start_apps(gc: &GuestConfig) {
    for app in &gc.apps {
        let argv = app_unit_argv(gc, app, &home_of(&gc.owner));
        let out = tokio::process::Command::new("systemd-run")
            .args(&argv)
            .env_clear()
            .env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin")
            .stdin(Stdio::null())
            .output()
            .await;
        match out {
            Ok(o) if o.status.success() => eprintln!("started {}", app.id),
            Ok(o) => eprintln!("starting {}: {}", app.id, String::from_utf8_lossy(&o.stderr).trim()),
            Err(e) => eprintln!("starting {}: systemd-run: {e}", app.id),
        }
    }
}

/// The owner's home, or where it will be: a NixOS guest makes the owner
/// after the agent starts.
fn home_of(owner: &str) -> String {
    match nix::unistd::User::from_name(owner) {
        Ok(Some(u)) => u.dir.to_string_lossy().into_owned(),
        _ => format!("/home/{owner}"),
    }
}

/// `systemd-run`'s arguments for an app's unit, `workspace-app-<id>`: its
/// bundle's `bin/serve`, as the owner in their home, with the workspace's
/// environment and secrets, and `APP_PORT` and `APP_URL`.
pub(crate) fn app_unit_argv(gc: &GuestConfig, app: &GuestApp, home: &str) -> Vec<String> {
    let path = format!(
        "{GUEST_PACKAGES}/sw/bin:{home}/.nix-profile/bin:/etc/profiles/per-user/{owner}/bin:/run/wrappers/bin:\
         /run/current-system/sw/bin:/usr/local/bin:/usr/bin:/bin",
        owner = gc.owner
    );
    let mut argv: Vec<String> = [
        format!("--unit=workspace-app-{}", app.id),
        format!("--description=Workspace app {}", app.name),
        format!("--uid={}", gc.owner),
        "--no-block".into(),
        "--property=WorkingDirectory=~".into(),
        "--property=After=workspace-init.service network-online.target".into(),
        "--property=Wants=network-online.target".into(),
        "--property=Restart=always".into(),
        "--property=RestartSec=5".into(),
        "--property=StartLimitIntervalSec=0".into(),
        "--property=KillMode=mixed".into(),
        "--property=EnvironmentFile=/run/workspace/config/environment".into(),
        "--property=EnvironmentFile=-/run/workspace/secrets/environment".into(),
        format!("--setenv=PATH={path}"),
        format!("--setenv=APP_PORT={}", app.port),
        format!("--setenv=APP_URL={}", app.url),
        format!("--setenv=WORKSPACE_URL={}", gc.url),
    ]
    .into();
    argv.push(format!("{GUEST_APPS}/{}/bin/serve", app.id));
    argv
}

/// Onlines memory the host plugs as movable, so it can be unplugged again.
fn configure_hotplug() {
    let path = "/sys/devices/system/memory/auto_online_blocks";
    if let Err(e) = std::fs::write(path, "online_movable") {
        eprintln!("{path}: {e}");
    }
}

async fn report_stats(port: u32) {
    let mut tick = tokio::time::interval(STATS_EVERY);
    loop {
        tick.tick().await;
        let Ok(meminfo) = tokio::fs::read_to_string("/proc/meminfo").await else { continue };
        let pressure = tokio::fs::read_to_string("/proc/pressure/memory").await.unwrap_or_default();
        let Ok(req) = control::Request::stats_from(&meminfo, &pressure) else { continue };
        let Ok(mut line) = serde_json::to_vec(&req) else { continue };
        line.push(b'\n');
        let send = async {
            let mut conn = vsock::dial(vsock::CID_HOST, port).await?;
            conn.write_all(&line).await?;
            control::read_line(&mut BufReader::new(conn), control::MAX_REQUEST).await
        };
        let _ = tokio::time::timeout(STATS_EVERY, send).await;
    }
}

/// Answers one host request.
pub(crate) async fn serve<S: AsyncRead + AsyncWrite + Unpin>(conn: S, gc: &GuestConfig) {
    let mut conn = BufReader::new(conn);
    let reply = match tokio::time::timeout(REQUEST_TIMEOUT, control::read_line(&mut conn, MAX_REQUEST)).await {
        Err(_) => Reply::error("no request"),
        Ok(Err(e)) => Reply::error(e.to_string()),
        Ok(Ok(line)) => match serde_json::from_slice::<Request>(&line) {
            Err(_) => Reply::error("invalid JSON"),
            Ok(req) => answer(req, gc, Path::new(GUEST_APPS)).await,
        },
    };
    let Ok(mut raw) = serde_json::to_vec(&reply) else { return };
    raw.push(b'\n');
    let conn = conn.get_mut();
    let _ = conn.write_all(&raw).await;
    let _ = conn.shutdown().await;
}

/// An app's sign-in hook, in the bundles at `apps`.
fn open_link_hook(apps: &Path, id: &str) -> std::path::PathBuf {
    apps.join(id).join("bin/open-link")
}

async fn answer(req: Request, gc: &GuestConfig, apps: &Path) -> Reply {
    match req.op.as_str() {
        "ping" => Reply {
            open_links: gc.apps.iter().filter(|a| open_link_hook(apps, &a.id).exists()).map(|a| a.id.clone()).collect(),
            ..Reply::default()
        },
        "open-link" => {
            // Only an app the VM booted with: the id never makes a path.
            let Some(app) = req.args.first().and_then(|id| gc.app(id)) else {
                return Reply::error("no such app");
            };
            let hook = open_link_hook(apps, &app.id);
            if !hook.exists() {
                return Reply::error("the app has no sign-in hook");
            }
            let argv = [hook.to_string_lossy().into_owned()];
            run_as_owner(gc, Some(app), &argv, HOOK_TIMEOUT).await.unwrap_or_else(|e| Reply::error(format!("{e:#}")))
        }
        _ => Reply::error("unknown op"),
    }
}

/// Runs a command as the owner, in their home, with the workspace's URL in
/// `WORKSPACE_URL` and the app's in `APP_URL`, and keeps the start of what
/// it prints.
pub(crate) async fn run_as_owner(
    gc: &GuestConfig,
    app: Option<&GuestApp>,
    argv: &[String],
    timeout: Duration,
) -> anyhow::Result<Reply> {
    let user = nix::unistd::User::from_name(&gc.owner)?.ok_or_else(|| anyhow!("unknown user {:?}", gc.owner))?;
    let home = user.dir.to_string_lossy().into_owned();
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(&home)
        .env_clear()
        .env("HOME", &home)
        .env("USER", &gc.owner)
        .env("LOGNAME", &gc.owner)
        .env("WORKSPACE_URL", &gc.url)
        .env(
            "PATH",
            format!(
                "{GUEST_PACKAGES}/sw/bin:/run/wrappers/bin:/run/current-system/sw/bin:/usr/local/bin:/usr/bin:/bin"
            ),
        )
        // Supplementary groups are dropped along with root.
        .uid(user.uid.as_raw())
        .gid(user.gid.as_raw())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(app) = app {
        cmd.env("APP_URL", &app.url).env("APP_PORT", app.port.to_string());
    }
    let mut child = cmd.spawn().with_context(|| argv[0].clone())?;
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let run = async {
        let (out, err, status) = tokio::join!(read_limited(stdout), read_limited(stderr), child.wait());
        anyhow::Ok((out, err, status?))
    };
    let (stdout, stderr, status) = tokio::time::timeout(timeout, run)
        .await
        .map_err(|_| anyhow!("{} did not finish within {timeout:?}", argv[0]))??;
    Ok(Reply { stdout, stderr, exit_code: status.code().unwrap_or(-1), ..Reply::default() })
}

/// Keeps the first [`MAX_OUTPUT`] bytes of a pipe and drains the rest, so
/// the command never blocks on it.
async fn read_limited(pipe: Option<impl AsyncRead + Unpin>) -> String {
    let Some(mut pipe) = pipe else { return String::new() };
    let mut kept = Vec::new();
    let _ = (&mut pipe).take(MAX_OUTPUT).read_to_end(&mut kept).await;
    let _ = tokio::io::copy(&mut pipe, &mut tokio::io::sink()).await;
    String::from_utf8_lossy(&kept).into_owned()
}

/// Why a request found no agent to answer it.
#[derive(Debug)]
pub enum AskError {
    /// The VM's vsock socket, or the agent behind it, could not be reached,
    /// or the exchange broke.
    Unreachable(anyhow::Error),
    /// No answer in time.
    Timeout,
}

impl std::fmt::Display for AskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AskError::Unreachable(e) => write!(f, "{e:#}"),
            AskError::Timeout => f.write_str("the guest agent did not answer in time"),
        }
    }
}

impl std::error::Error for AskError {}

/// Sends one request to a workspace's agent over vsock and returns its
/// reply, failed or not.
pub async fn exchange(c: &Config, req: &Request, timeout: Duration) -> Result<Reply, AskError> {
    let exchange = async {
        let conn = vmkit::ch::dial_guest(c.vsock(), c.agent_port_u32(), timeout)
            .await
            .with_context(|| format!("reaching the guest agent of {}", c.name))?;
        let mut conn = BufReader::new(conn);
        let mut line = serde_json::to_vec(req)?;
        line.push(b'\n');
        conn.get_mut().write_all(&line).await?;
        let line = control::read_line(&mut conn, MAX_REPLY).await.context("the agent's reply")?;
        anyhow::Ok(serde_json::from_slice::<Reply>(&line)?)
    };
    match tokio::time::timeout(timeout, exchange).await {
        Err(_) => Err(AskError::Timeout),
        Ok(r) => r.map_err(AskError::Unreachable),
    }
}

/// Sends one request to a workspace's agent; a failed reply is an error.
pub async fn ask(c: &Config, req: &Request, timeout: Duration) -> anyhow::Result<Reply> {
    let reply = exchange(c, req, timeout).await.with_context(|| format!("the guest agent of {}", c.name))?;
    if !reply.error.is_empty() {
        bail!("{}", reply.error);
    }
    Ok(reply)
}

/// A host directory for the agent's tests; the guest's app is not there.
#[cfg(test)]
pub(crate) fn test_runtime(c: &mut Config) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    c.runtime_dir = dir.path().to_string_lossy().into();
    std::fs::create_dir_all(c.vm_dir()).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::test_config;
    use std::path::PathBuf;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    use tokio::net::UnixListener;

    /// The agent roots itself in a guest's own store, and only there.
    #[test]
    fn shared_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, gcroots) = (tmp.path().join("config"), tmp.path().join("gcroots"));
        fs::create_dir_all(config.join("apps")).unwrap();
        root_shared(&config, &gcroots).unwrap();
        fs::create_dir(&gcroots).unwrap();
        std::os::unix::fs::symlink("/nix/store/p-packages", gcroots.join("workspace-app")).unwrap();
        std::os::unix::fs::symlink("/nix/store/o-old", gcroots.join("workspace-app-old")).unwrap();
        root_shared(&config, &gcroots).unwrap();
        let roots = || {
            let mut r: Vec<_> = fs::read_dir(&gcroots)
                .unwrap()
                .map(|e| {
                    let e = e.unwrap();
                    (e.file_name().into_string().unwrap(), fs::read_link(e.path()).unwrap())
                })
                .collect();
            r.sort();
            r
        };
        assert_eq!(roots(), [("workspace-app".into(), "/nix/store/p-packages".into())], "a host that shares no agent");
        for (agent, t3) in
            [("/nix/store/x-agent", "/nix/store/x-t3code"), ("/nix/store/y-agent", "/nix/store/y-t3code")]
        {
            for (link, to) in [(config.join("agent"), agent), (config.join("apps/t3code"), t3)] {
                let _ = fs::remove_file(&link);
                std::os::unix::fs::symlink(to, link).unwrap();
            }
            fs::create_dir_all(config.join("apps/broken")).unwrap();
            root_shared(&config, &gcroots).unwrap();
            assert_eq!(
                roots(),
                [
                    ("workspace-agent".into(), PathBuf::from(agent)),
                    ("workspace-app".into(), "/nix/store/p-packages".into()),
                    ("workspace-app-t3code".into(), PathBuf::from(t3)),
                ],
                "the packages' root is the guest module's"
            );
        }
    }

    /// The ping names the apps with a hook; a link needs one, of an app
    /// the VM booted with.
    #[tokio::test]
    async fn open_link_needs_the_hook() {
        let dir = tempfile::tempdir().unwrap();
        let apps = dir.path();
        let ping = Request { op: "ping".into(), args: vec![] };
        let gc = GuestConfig::of(&test_config());
        assert!(answer(ping.clone(), &gc, apps).await.open_links.is_empty());
        let open = |id: &str| Request { op: "open-link".into(), args: vec![id.into()] };
        assert_eq!(answer(open("t3code"), &gc, apps).await.error, "the app has no sign-in hook");
        std::fs::create_dir_all(apps.join("t3code/bin")).unwrap();
        std::fs::write(apps.join("t3code/bin/open-link"), "").unwrap();
        assert_eq!(answer(ping, &gc, apps).await.open_links, ["t3code"]);
        std::fs::create_dir_all(apps.join("other/bin")).unwrap();
        std::fs::write(apps.join("other/bin/open-link"), "").unwrap();
        for id in ["other", "../t3code", ""] {
            assert_eq!(answer(open(id), &gc, apps).await.error, "no such app", "{id}");
        }
    }

    #[test]
    fn app_units() {
        let gc = GuestConfig::of(&test_config());
        let argv = app_unit_argv(&gc, &gc.apps[0], "/home/alice");
        assert_eq!(argv[0], "--unit=workspace-app-t3code");
        assert_eq!(argv.last().unwrap(), "/run/workspace/config/apps/t3code/bin/serve");
        for want in [
            "--uid=alice",
            "--setenv=APP_PORT=3773",
            "--setenv=APP_URL=https://alice-dev--t3code.ws.test",
            "--property=EnvironmentFile=-/run/workspace/secrets/environment",
        ] {
            assert!(argv.iter().any(|a| a == want), "{want} missing from {argv:?}");
        }
    }

    #[tokio::test]
    async fn output_is_bounded() {
        let big = vec![b'x'; 200 << 10];
        let kept = read_limited(Some(&big[..])).await;
        assert_eq!(kept.len() as u64, MAX_OUTPUT);
    }

    /// Stands in for cloud-hypervisor's host-side vsock socket with one
    /// guest agent behind it on `port`.
    fn fake_guest(socket: PathBuf, port: u32) {
        let listener = UnixListener::bind(socket).unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((conn, _)) = listener.accept().await else { return };
                let mut conn = BufReader::new(conn);
                let mut line = String::new();
                conn.read_line(&mut line).await.unwrap();
                if line != format!("CONNECT {port}\n") {
                    let _ = conn.get_mut().write_all(b"refused\n").await;
                    continue;
                }
                conn.get_mut().write_all(b"OK 1073741824\n").await.unwrap();
                serve(conn, &GuestConfig::default()).await;
            }
        });
    }

    #[tokio::test]
    async fn ask_over_vsock() {
        let mut c = test_config();
        let _dir = test_runtime(&mut c);
        fake_guest(c.vsock(), c.agent_port_u32());
        let t = Duration::from_secs(2);
        ask(&c, &Request { op: "ping".into(), args: vec![] }, t).await.unwrap();
        let err = ask(&c, &Request { op: "shell".into(), args: vec!["id".into()] }, t).await.unwrap_err();
        assert!(err.to_string().contains("unknown op"), "unknown op answered: {err}");
        c.agent_port = 9;
        assert!(ask(&c, &Request { op: "ping".into(), args: vec![] }, t).await.is_err(), "a refused port was reached");
    }

    #[tokio::test]
    async fn serve_rejects_garbage() {
        let (mut client, server) = tokio::io::duplex(4096);
        tokio::spawn(async move { serve(server, &GuestConfig::default()).await });
        client.write_all(b"not json\n").await.unwrap();
        let mut out = String::new();
        client.read_to_string(&mut out).await.unwrap();
        let reply: Reply = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(reply.error, "invalid JSON");
    }

    /// Replies keep the field names the host of an earlier release reads.
    #[test]
    fn reply_format() {
        let raw = r#"{"stdout":"x\n","stderr":"","exitCode":2}"#;
        let reply: Reply = serde_json::from_str(raw).unwrap();
        assert_eq!(reply.exit_code, 2);
        assert_eq!(serde_json::to_string(&reply).unwrap(), r#"{"stdout":"x\n","exitCode":2}"#);
        assert_eq!(serde_json::to_string(&Reply::default()).unwrap(), r#"{"exitCode":0}"#);
        let ping = Reply { open_links: vec!["t3code".into()], ..Reply::default() };
        assert_eq!(serde_json::to_string(&ping).unwrap(), r#"{"openLinks":["t3code"],"exitCode":0}"#);
        let old: Reply = serde_json::from_str(r#"{"signIn":true,"exitCode":0}"#).unwrap();
        assert!(old.open_links.is_empty(), "an agent from before apps names none");
    }
}
