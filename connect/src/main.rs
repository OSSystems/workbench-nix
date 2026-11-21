//! `workbench-connect`: reaches a person's workbench workspaces over SSH,
//! from their own computer, through the web UI, whatever network is in
//! between.
//!
//! - `setup <code>` installs it in `~/.local/bin`, trades a pairing code
//!   the web UI made for a credential of this computer's, and writes
//!   `~/.config/workbench`: the credential, a key pair, its certificate,
//!   `known_hosts` with the host's CA and `ssh_config`, which
//!   `~/.ssh/config` includes first.
//! - `login` gets a new SSH certificate when the one it has ends soon. ssh
//!   loads its certificate before it runs `ProxyCommand`, so `ssh_config`
//!   runs this in a `Match exec`, which ssh runs while it reads its
//!   configuration, before.
//! - `tunnel <host>`, ssh's `ProxyCommand`, passes ssh's stdin and stdout
//!   through a WebSocket to the workspace's SSH server.
//!
//! Linux only, for now.

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs, io};

use serde::{Deserialize, Serialize};

const USAGE: &str = "\
workbench-connect reaches your workbench workspaces over SSH, from this computer.

Usage:
  workbench-connect setup [--server https://<domain>] <code>
      Pair this computer with a code from a workspace's Connect page.
  workbench-connect login
      Get a new SSH certificate when this one ends soon; ssh runs it.
  workbench-connect tunnel <workspace>.<domain>
      ssh's ProxyCommand: the way to a workspace's SSH server.

Then: ssh <workspace>.<domain>
";

/// A certificate is renewed when it has less than this left, in seconds.
const RENEW_BEFORE: i64 = 15 * 60;
/// The largest answer read from the server.
const MAX_ANSWER: u64 = 64 << 10;

type Result<T> = std::result::Result<T, String>;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let run = match args.first().map(String::as_str) {
        Some("setup") => setup(&args[1..]),
        Some("login") => Paths::from_env().and_then(|p| login(&p, false)),
        Some("tunnel") if args.len() == 2 => tunnel(&args[1]),
        Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            Ok(())
        }
        Some("--version" | "-V") => {
            println!("workbench-connect {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => Err(USAGE.trim_end().to_owned()),
    };
    match run {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("workbench-connect: {e}");
            ExitCode::FAILURE
        }
    }
}

/// What `setup` learned, which the other commands read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    /// `https://<domain>`, the web UI.
    server: String,
    domain: String,
    /// The person, whose name the workspaces' SSH servers let in.
    user: String,
}

/// The server's answers, as `workbench_model::connect` has them.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Paired {
    credential: String,
    user: String,
    domain: String,
    host_ca: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Cert {
    certificate: String,
    valid_before: i64,
}

#[derive(Debug, Deserialize)]
struct Refusal {
    error: String,
}

/// Where everything is: `~/.config/workbench`, `~/.local/bin` and
/// `~/.ssh/config`.
#[derive(Debug, Clone)]
struct Paths {
    dir: PathBuf,
    /// Where `setup` installs it, and what `ssh_config` runs.
    bin: PathBuf,
    ssh_config: PathBuf,
}

impl Paths {
    fn from_env() -> Result<Self> {
        let home = env::var_os("HOME").filter(|h| !h.is_empty()).ok_or("HOME isn't set")?;
        Ok(Paths::of(Path::new(&home)))
    }

    fn of(home: &Path) -> Self {
        Paths {
            dir: home.join(".config/workbench"),
            bin: home.join(".local/bin/workbench-connect"),
            ssh_config: home.join(".ssh/config"),
        }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn key(&self) -> PathBuf {
        self.file("id_ed25519")
    }

    fn cert(&self) -> PathBuf {
        self.file("id_ed25519-cert.pub")
    }

    fn settings(&self) -> Result<Settings> {
        let raw = fs::read(self.file("config.json")).map_err(|e| {
            format!("this computer isn't set up ({e}); run the command from a workspace's Connect page")
        })?;
        serde_json::from_slice(&raw).map_err(|e| format!("{}: {e}", self.file("config.json").display()))
    }

    fn credential(&self) -> Result<String> {
        fs::read_to_string(self.file("credential"))
            .map(|c| c.trim().to_owned())
            .map_err(|e| format!("this computer has no credential ({e}); set it up again"))
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Writes a file whole, readable by its owner only, or replaces it.
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(data).and_then(|()| f.sync_all()))
        .and_then(|()| fs::rename(&tmp, path));
    written.map_err(|e| format!("{}: {e}", path.display()))
}

fn private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|e| format!("{}: {e}", dir.display()))
}

/// The system's CA certificates, which the web UI's must be among.
fn roots() -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let found = rustls_native_certs::load_native_certs();
    if found.certs.is_empty() {
        return Err("no CA certificates on this computer to check the server with".into());
    }
    Ok(found.certs)
}

fn tls() -> Result<Arc<rustls::ClientConfig>> {
    let mut store = rustls::RootCertStore::empty();
    store.add_parsable_certificates(roots()?);
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(store)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Posts JSON to the server and reads its JSON answer, or why it refused.
fn post<T: for<'de> Deserialize<'de>>(
    server: &str,
    path: &str,
    bearer: Option<&str>,
    body: &serde_json::Value,
) -> Result<T> {
    let certs: Vec<ureq::tls::Certificate<'static>> =
        roots()?.iter().map(|c| ureq::tls::Certificate::from_der(c.as_ref()).to_owned()).collect();
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(ureq::tls::RootCerts::new_with_certs(&certs))
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build();
    let agent: ureq::Agent = ureq::Agent::config_builder().tls_config(tls).http_status_as_error(false).build().into();
    let mut req = agent.post(format!("{server}{path}")).header("Accept", "application/json");
    if let Some(b) = bearer {
        req = req.header("Authorization", format!("Bearer {b}"));
    }
    let mut res = req.send_json(body).map_err(|e| format!("{server}: {e}"))?;
    let status = res.status();
    let raw = res.body_mut().with_config().limit(MAX_ANSWER).read_to_vec().map_err(|e| format!("{server}: {e}"))?;
    if !status.is_success() {
        let why = serde_json::from_slice::<Refusal>(&raw)
            .map_or_else(|_| format!("{server}{path} answered {status}"), |r| r.error);
        return Err(why);
    }
    serde_json::from_slice(&raw).map_err(|e| format!("{server}{path}: {e}"))
}

/// The certificate's end, as `login` noted it, or 0.
fn cert_ends(p: &Paths) -> i64 {
    fs::read_to_string(p.file("cert-valid-before")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

/// Whether a certificate ending at `valid_before` should be renewed now.
fn renew(valid_before: i64, now: i64) -> bool {
    valid_before - now < RENEW_BEFORE
}

/// Gets a new certificate for this computer's key, unless the one it has
/// lasts a while yet, or `force`.
fn login(p: &Paths, force: bool) -> Result<()> {
    if !force && p.cert().exists() && !renew(cert_ends(p), now()) {
        return Ok(());
    }
    let settings = p.settings()?;
    let credential = p.credential()?;
    let public =
        fs::read_to_string(p.key().with_extension("pub")).map_err(|e| format!("{}: {e}", p.key().display()))?;
    let cert: Cert =
        post(&settings.server, "/connect/cert", Some(&credential), &serde_json::json!({ "publicKey": public.trim() }))?;
    write_private(&p.cert(), format!("{}\n", cert.certificate.trim()).as_bytes())?;
    write_private(&p.file("cert-valid-before"), cert.valid_before.to_string().as_bytes())
}

/// This computer's name, for the person's list of devices.
fn label() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname").map(|h| h.trim().to_owned()).unwrap_or_default()
}

/// `setup [--server <url>] <code>`.
fn setup(args: &[String]) -> Result<()> {
    let (server, code) = match args {
        [flag, server, code] if flag == "--server" => (Some(server.clone()), code.clone()),
        [code] => (None, code.clone()),
        _ => return Err(USAGE.trim_end().to_owned()),
    };
    let p = Paths::from_env()?;
    let server = match server {
        Some(s) => s.trim_end_matches('/').to_owned(),
        None => p.settings().map(|s| s.server).map_err(|_| "say which server: --server https://<domain>")?,
    };
    if !server.starts_with("https://") {
        return Err(format!("{server} isn't an https:// address"));
    }
    install(&p)?;
    private_dir(&p.dir)?;
    if !p.key().exists() {
        let out = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", &format!("workbench-connect@{}", label()), "-f"])
            .arg(p.key())
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("ssh-keygen: {e}; is OpenSSH installed?"))?;
        if !out.status.success() {
            return Err(format!("ssh-keygen: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
    }
    let paired: Paired =
        post(&server, "/connect/pair", None, &serde_json::json!({ "code": code.trim(), "label": label() }))?;
    let settings = Settings { server, domain: paired.domain.clone(), user: paired.user.clone() };
    write_private(&p.file("credential"), format!("{}\n", paired.credential).as_bytes())?;
    write_private(&p.file("config.json"), &serde_json::to_vec_pretty(&settings).map_err(|e| e.to_string())?)?;
    write_private(&p.file("known_hosts"), known_hosts(&paired.domain, &paired.host_ca).as_bytes())?;
    write_private(&p.file("ssh_config"), ssh_config(&p, &settings).as_bytes())?;
    login(&p, true)?;
    let ssh = p.ssh_config.parent().ok_or("no ~/.ssh")?;
    private_dir(ssh)?;
    include_first(&p.ssh_config, &include_line(&p))?;
    println!("This computer reaches {}'s workspaces at <workspace>.{}:", settings.user, settings.domain);
    println!("  ssh <workspace>.{}", settings.domain);
    Ok(())
}

/// Copies this program to `~/.local/bin`, unless it runs from there.
fn install(p: &Paths) -> Result<()> {
    let me = env::current_exe().map_err(|e| format!("where this program is: {e}"))?;
    if fs::canonicalize(&me).ok() == fs::canonicalize(&p.bin).ok() {
        return Ok(());
    }
    let dir = p.bin.parent().ok_or("no ~/.local/bin")?;
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = p.bin.with_extension("new");
    fs::copy(&me, &tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &p.bin).map_err(|e| format!("{}: {e}", p.bin.display()))
}

/// The host CA, for every workspace.
fn known_hosts(domain: &str, host_ca: &str) -> String {
    format!("@cert-authority *.{domain} {}\n", host_ca.trim())
}

/// What ssh does for the workspaces: the person's name, the tunnel, this
/// computer's key and certificate, and the host CA.
fn ssh_config(p: &Paths, s: &Settings) -> String {
    let (bin, dir) = (p.bin.display(), p.dir.display());
    format!(
        "# Written by workbench-connect setup, which writes it again when it runs.\n\
         Host *.{domain}\n\
         \x20 User {user}\n\
         \x20 ProxyCommand {bin} tunnel %h\n\
         \x20 IdentityFile {dir}/id_ed25519\n\
         \x20 CertificateFile {dir}/id_ed25519-cert.pub\n\
         \x20 IdentitiesOnly yes\n\
         \x20 UserKnownHostsFile {dir}/known_hosts\n\
         \n\
         # ssh reads the certificate before it runs ProxyCommand: this gets a\n\
         # new one first when it ends soon.\n\
         Match host *.{domain} exec \"{bin} login\"\n",
        domain = s.domain,
        user = s.user,
    )
}

/// The line `~/.ssh/config` starts with.
fn include_line(p: &Paths) -> String {
    let home = p.ssh_config.parent().and_then(Path::parent);
    let path = match home.and_then(|h| p.file("ssh_config").strip_prefix(h).ok().map(Path::to_path_buf)) {
        Some(rel) => format!("~/{}", rel.display()),
        None => p.file("ssh_config").display().to_string(),
    };
    format!("Include {path}")
}

/// Puts `line` at the top of an ssh config, once: what comes first wins.
fn include_first(config: &Path, line: &str) -> Result<()> {
    let existing = match fs::read_to_string(config) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", config.display())),
    };
    if existing.lines().next().map(str::trim) == Some(line) {
        return Ok(());
    }
    let rest: Vec<&str> = existing.lines().filter(|l| l.trim() != line).collect();
    let rest = rest.join("\n");
    let rest = rest.trim_start_matches('\n');
    let new = if rest.is_empty() { format!("{line}\n") } else { format!("{line}\n\n{rest}\n") };
    write_private(config, new.as_bytes())
}

/// The workspace a host name is, in the domain this computer reaches.
fn workspace_of<'a>(host: &'a str, domain: &str) -> Result<&'a str> {
    let name = host.strip_suffix(domain).and_then(|h| h.strip_suffix('.')).unwrap_or_default();
    let b = name.as_bytes();
    let ok = (1..=28).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && b[b.len() - 1] != b'-'
        && !name.contains("--");
    if ok { Ok(name) } else { Err(format!("{host} isn't a workspace of {domain}")) }
}

/// `tunnel <host>`: ssh's stdin and stdout, through the WebSocket.
fn tunnel(host: &str) -> Result<()> {
    let p = Paths::from_env()?;
    // `login` ran as ssh read its configuration; this is for an ssh
    // without it, whose next connection has the new certificate.
    if let Err(e) = login(&p, false) {
        eprintln!("workbench-connect: {e}");
    }
    let settings = p.settings()?;
    let credential = p.credential()?;
    let name = workspace_of(host, &settings.domain)?;
    let url = format!("{}/connect/ssh/{name}", settings.server.replacen("https://", "wss://", 1));
    let tls = tls()?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(pump(&url, &credential, tls))
}

async fn pump(url: &str, credential: &str, tls: Arc<rustls::ClientConfig>) -> Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest};

    let mut req = url.into_client_request().map_err(|e| e.to_string())?;
    let bearer = format!("Bearer {credential}").parse().map_err(|_| "the credential isn't a header value")?;
    req.headers_mut().insert("authorization", bearer);
    let connector = tokio_tungstenite::Connector::Rustls(tls);
    let (ws, _) = match tokio_tungstenite::connect_async_tls_with_config(req, None, false, Some(connector)).await {
        Ok(ok) => ok,
        Err(tungstenite::Error::Http(res)) => {
            let why = res.body().as_ref().and_then(|b| serde_json::from_slice::<Refusal>(b).ok());
            return Err(why.map_or_else(|| format!("{url} answered {}", res.status()), |r| r.error));
        }
        Err(e) => return Err(format!("{url}: {e}")),
    };
    let (mut to_server, mut from_server) = ws.split();
    let up = async {
        let mut stdin = tokio::io::stdin();
        let mut buf = vec![0; 32 << 10];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to_server.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = to_server.close().await;
    };
    let down = async {
        let mut stdout = tokio::io::stdout();
        while let Some(Ok(msg)) = from_server.next().await {
            match msg {
                Message::Binary(b) if stdout.write_all(&b).await.is_err() || stdout.flush().await.is_err() => break,
                Message::Close(_) => break,
                _ => {}
            }
        }
    };
    tokio::select! {
        () = up => {}
        () = down => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings { server: "https://ws.test".into(), domain: "ws.test".into(), user: "alice".into() }
    }

    #[test]
    fn what_ssh_is_told() {
        let p = Paths::of(Path::new("/home/me"));
        let c = ssh_config(&p, &settings());
        for want in [
            "Host *.ws.test\n",
            "  User alice\n",
            "  ProxyCommand /home/me/.local/bin/workbench-connect tunnel %h\n",
            "  CertificateFile /home/me/.config/workbench/id_ed25519-cert.pub\n",
            "  UserKnownHostsFile /home/me/.config/workbench/known_hosts\n",
            "Match host *.ws.test exec \"/home/me/.local/bin/workbench-connect login\"\n",
        ] {
            assert!(c.contains(want), "{want:?} missing from\n{c}");
        }
        assert_eq!(known_hosts("ws.test", "ssh-ed25519 AAAA ca\n"), "@cert-authority *.ws.test ssh-ed25519 AAAA ca\n");
        assert_eq!(include_line(&p), "Include ~/.config/workbench/ssh_config");
    }

    #[test]
    fn the_include_comes_first_once() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        let line = "Include ~/.config/workbench/ssh_config";
        include_first(&config, line).unwrap();
        assert_eq!(fs::read_to_string(&config).unwrap(), format!("{line}\n"));
        let mode = fs::metadata(&config).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        fs::write(&config, format!("Host *\n  User me\n{line}\n")).unwrap();
        include_first(&config, line).unwrap();
        include_first(&config, line).unwrap();
        assert_eq!(fs::read_to_string(&config).unwrap(), format!("{line}\n\nHost *\n  User me\n"));
    }

    #[test]
    fn workspaces_and_renewals() {
        assert_eq!(workspace_of("alice-dev.ws.test", "ws.test"), Ok("alice-dev"));
        for bad in ["ws.test", "alice.other.test", "a.b.ws.test", "alice--t3code.ws.test", "Alice.ws.test", "xws.test"]
        {
            assert!(workspace_of(bad, "ws.test").is_err(), "{bad}");
        }
        assert!(renew(0, 1000), "none yet");
        assert!(renew(1000 + RENEW_BEFORE - 1, 1000), "it ends soon");
        assert!(!renew(1000 + 3600, 1000));
    }
}
