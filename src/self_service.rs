//! Workspaces made at run time: `workbench self-service create` makes one
//! from a request the web UI wrote, `destroy` takes it away, `setup` puts
//! back at boot what `/run` lost, and `reconcile` stops the workspaces of
//! owners who left. They run as root; the web UI only writes requests and
//! starts their units.
//!
//! A workspace made at run time is what the Nix module would declare: a
//! config in `<stateDir>/configs`, the VM's user, as a userdb record, its
//! tap, as systemd-networkd files, and for Ubuntu its cloud-init seed. The
//! host's [`Template`] holds everything that isn't the workspace's own.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use vmkit::hostutil::{self, Account};
use workbench_model::self_service::{Limits, Outcome, Request, quota_of};
use workbench_model::{is_user_name, is_workspace_name};

use crate::config::{self, Config};
use crate::{launch, state};

/// The host's template, which the Nix module writes.
pub const TEMPLATE: &str = "/etc/workbench-self-service.json";
/// Where the VMs' user records go; `/run` is empty at boot, so `setup` puts
/// them back.
pub const USERDB: &str = "/run/userdb";
/// Where their taps' systemd-networkd files go.
pub const NETWORK: &str = "/run/systemd/network";

/// The VM users of workspaces made at run time: between systemd-homed's
/// users and the dynamic ones.
const UIDS: std::ops::RangeInclusive<u32> = 60_600..=60_999;
/// The largest request, quotas or command output read.
const MAX_FILE: u64 = 1 << 20;
/// How long the host's commands may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// What every workspace made at run time shares, from the Nix
/// configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Template {
    pub limits: Limits,
    /// Workspaces are `<scheme>://<name>.<domain>`.
    pub domain: String,
    pub scheme: String,
    pub state_dir: String,
    /// Each workspace's secrets directory is a directory of this.
    pub secrets_root: String,
    /// The first three octets of the workspaces' /24; the host is `.1`.
    pub subnet: String,
    pub nameservers: Vec<String>,
    pub app_port: i64,
    pub control_port: i64,
    pub agent_port: i64,
    pub cpus: i64,
    pub cloud_hypervisor: String,
    pub virtiofsd: String,
    pub qemu_img: String,
    pub firmware: String,
    pub closure: String,
    pub registration: String,
    pub app: String,
    pub environment: BTreeMap<String, String>,
    pub environment_files: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub socket_group: String,
    /// By guest system: the image a new disk is made from.
    pub images: BTreeMap<String, Image>,
    /// For Ubuntu seeds.
    pub mkfs_vfat: String,
    pub mcopy: String,
    /// Prints a workspace's secret environment for its owner.
    pub environment_command: String,
    /// Exits 0 while an owner may keep their workspaces; empty asks no one.
    #[serde(default)]
    pub owner_command: String,
}

/// A guest system's image.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Image {
    pub image: String,
    pub image_format: String,
    /// Ubuntu's cloud-init user data, with `@name@` and `@owner@` for the
    /// workspace's; empty for a guest that needs no seed.
    #[serde(default)]
    pub user_data: String,
}

impl Template {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = hostutil::read_bounded(path, MAX_FILE).with_context(|| path.display().to_string())?;
        serde_json::from_slice(&raw).with_context(|| path.display().to_string())
    }

    #[must_use]
    pub fn paths(&self) -> Paths {
        Paths(PathBuf::from(&self.state_dir))
    }
}

/// Where workspaces made at run time keep their files.
#[derive(Debug, Clone)]
pub struct Paths(pub PathBuf);

impl Paths {
    /// Their configs, which the host reads after `/etc/workbench`.
    #[must_use]
    pub fn configs(&self) -> PathBuf {
        self.0.join("configs")
    }

    /// Requests and quotas, which the web UI writes, each in a directory of
    /// its own, and the outcomes, which root writes for the UI to read.
    /// The directory itself is root's, so the UI can't swap any of them.
    #[must_use]
    pub fn dir(&self) -> PathBuf {
        self.0.join("self-service")
    }

    #[must_use]
    pub fn request(&self, name: &str) -> PathBuf {
        self.dir().join("requests").join(format!("{name}.json"))
    }

    #[must_use]
    pub fn outcome(&self, name: &str) -> PathBuf {
        self.dir().join("results").join(format!("{name}.json"))
    }

    /// The config of a workspace being made or taken away, root's: if the
    /// process dies half way, what it names is cleaned up under the lock.
    #[must_use]
    pub fn pending(&self, name: &str) -> PathBuf {
        self.dir().join("pending").join(format!("{name}.json"))
    }

    #[must_use]
    pub fn quotas(&self) -> PathBuf {
        self.dir().join("quotas").join("quotas.json")
    }

    /// Ubuntu seeds, root's.
    #[must_use]
    pub fn seed(&self, name: &str) -> PathBuf {
        self.0.join(".seeds").join(format!("{name}.img"))
    }

    /// One request at a time, so a quota holds and an address or a user id
    /// is never handed out while it is being taken away.
    fn lock(&self) -> anyhow::Result<nix::fcntl::Flock<fs::File>> {
        let path = self.0.join(".seeds").join(".lock");
        fs::create_dir_all(path.parent().context("lock")?)?;
        let file = fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
        Ok(nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive).map_err(|(_, e)| e)?)
    }
}

/// Reads a file the web UI wrote, without following a link it put there.
fn read_theirs(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt;
    let f = fs::OpenOptions::new().read(true).custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits()).open(path)?;
    let mut data = Vec::new();
    f.take(MAX_FILE + 1).read_to_end(&mut data)?;
    if data.len() as u64 > MAX_FILE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "file too large"));
    }
    Ok(data)
}

/// Marks a workspace whose owner left; names starting with a dot are never
/// workspaces.
#[must_use]
pub fn left_marker(state_root: &Path, name: &str) -> PathBuf {
    state_root.join(".left").join(name)
}

/// The people's quotas an admin set, by uid.
pub fn read_quotas(path: &Path) -> anyhow::Result<BTreeMap<String, u32>> {
    match read_theirs(path) {
        Ok(raw) => serde_json::from_slice(&raw).with_context(|| path.display().to_string()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e).with_context(|| path.display().to_string()),
    }
}

/// Every config file in `dirs`, each on its own: one a directory before
/// shadows still holds its address and VM user. Then the files that don't
/// load, whose address and user nobody can tell.
fn every_config(dirs: &[PathBuf]) -> (Vec<Config>, Vec<String>) {
    let (mut ok, mut bad) = (Vec::new(), Vec::new());
    for dir in dirs {
        let dir = std::slice::from_ref(dir);
        for name in config::configured(dir) {
            match config::load(dir, &name) {
                Ok(c) => ok.push(c),
                Err(e) => bad.push(format!("{}: {e:#}", dir[0].join(format!("{name}.json")).display())),
            }
        }
    }
    (ok, bad)
}

/// The configs of workspaces made at run time that load.
#[must_use]
pub fn made(paths: &Paths) -> Vec<Config> {
    every_config(&[paths.configs()]).0.into_iter().filter(|c| c.self_service).collect()
}

/// How many workspaces made at run time `owner` has.
#[must_use]
pub fn owned(made: &[Config], owner: &str) -> u32 {
    u32::try_from(made.iter().filter(|c| c.owner == owner).count()).unwrap_or(u32::MAX)
}

/// The addresses of workspaces made at run time: the upper half of the
/// subnet, which the Nix module keeps declared workspaces out of.
pub const MADE_IPS: std::ops::RangeInclusive<u8> = 128..=254;

/// The first address of the run-time half of the subnet no config has.
pub fn free_ip(subnet: &str, used: &BTreeSet<String>) -> anyhow::Result<String> {
    MADE_IPS
        .map(|n| format!("{subnet}.{n}"))
        .find(|ip| !used.contains(ip))
        .context("no address left in the workspaces' subnet")
}

/// The first VM uid no config and no user has.
pub fn free_uid(used: &BTreeSet<u32>, taken: impl Fn(u32) -> bool) -> anyhow::Result<u32> {
    UIDS.clone().find(|uid| !used.contains(uid) && !taken(*uid)).context("no VM user id left")
}

/// The tap and MAC of an address, as the Nix module makes them.
#[must_use]
pub fn tap_and_mac(ip: Ipv4Addr) -> (String, String) {
    let o = ip.octets();
    let tap = format!("wb{:02x}{:02x}{:02x}{:02x}", o[0], o[1], o[2], o[3]);
    let mac = format!("02:00:{:02x}:{:02x}:{:02x}:{:02x}", o[0], o[1], o[2], o[3]);
    (tap, mac)
}

/// The config a request makes, at `ip` with the VM user `uid`.
pub fn config_of(t: &Template, r: &Request, ip: &str, uid: u32) -> anyhow::Result<Config> {
    let image = t.images.get(&r.os).with_context(|| format!("no image for {:?}", r.os))?;
    let paths = t.paths();
    let addr: Ipv4Addr = ip.parse().context("address")?;
    let (tap, mac) = tap_and_mac(addr);
    let c = Config {
        name: r.name.clone(),
        owner: r.owner.clone(),
        user: format!("wb-{}", r.name),
        url: format!("{}://{}.{}", t.scheme, r.name, t.domain),
        state_dir: format!("{}/{}", t.state_dir, r.name),
        runtime_dir: format!("/run/workbench/{}", r.name),
        cloud_hypervisor: t.cloud_hypervisor.clone(),
        virtiofsd: t.virtiofsd.clone(),
        qemu_img: t.qemu_img.clone(),
        firmware: t.firmware.clone(),
        os: r.os.clone(),
        image: image.image.clone(),
        image_format: image.image_format.clone(),
        seed: if image.user_data.is_empty() { String::new() } else { paths.seed(&r.name).display().to_string() },
        disk_gib: r.disk_gib,
        cpus: t.cpus,
        boot_mem_mib: t.limits.boot_mem_mib,
        max_mem_mib: r.max_memory_gib * 1024,
        tap,
        mac,
        ip: ip.to_owned(),
        gateway: format!("{}.1", t.subnet),
        nameservers: t.nameservers.clone(),
        app_port: t.app_port,
        ssh_authorized_keys: r.ssh_authorized_keys.clone(),
        closure: t.closure.clone(),
        registration: t.registration.clone(),
        app: t.app.clone(),
        environment: t.environment.clone(),
        environment_files: t.environment_files.clone(),
        secrets_dir: format!("{}/{}", t.secrets_root, r.name),
        control_port: t.control_port,
        agent_port: t.agent_port,
        socket_group: t.socket_group.clone(),
        self_service: true,
        uid,
    };
    c.validate()?;
    Ok(c)
}

/// The VM user's userdb record: a locked system user whose primary group
/// is `kvm`, like the Nix module's.
#[must_use]
pub fn user_record(c: &Config, kvm_gid: u32) -> String {
    serde_json::json!({
        "userName": c.user,
        "uid": c.uid,
        "gid": kvm_gid,
        "realName": format!("cloud-hypervisor of workspace {}", c.name),
        "homeDirectory": "/var/empty",
        "shell": "/run/current-system/sw/bin/nologin",
        "disposition": "system",
        "locked": true,
    })
    .to_string()
}

/// The tap's systemd-networkd files: the netdev, then the network.
#[must_use]
pub fn network_files(c: &Config) -> (String, String) {
    let netdev = format!(
        "[NetDev]\nName={tap}\nKind=tap\n\n[Tap]\nUser={user}\nGroup=kvm\nVNetHeader=true\n",
        tap = c.tap,
        user = c.user
    );
    let network = format!(
        "[Match]\nName={tap}\n\n[Network]\nAddress={gw}/32\nConfigureWithoutCarrier=yes\nIgnoreCarrierLoss=yes\n\
         LinkLocalAddressing=no\nIPv6AcceptRA=no\n\n[Route]\nDestination={ip}/32\n\n[Link]\nRequiredForOnline=no\n",
        tap = c.tap,
        gw = c.gateway,
        ip = c.ip
    );
    (netdev, network)
}

/// An Ubuntu seed's three files: user data, network config, meta data.
pub fn seed_files(user_data: &str, c: &Config, instance: &str) -> anyhow::Result<[(String, String); 3]> {
    let filled = user_data.replace("@name@", &c.name).replace("@owner@", &c.owner);
    let body = filled.strip_prefix("#cloud-config\n").context("user data must start with #cloud-config")?;
    let mut user: serde_json::Value = serde_json::from_str(body).context("user data")?;
    let first = user.pointer_mut("/users/0").context("user data has no user")?;
    first["ssh_authorized_keys"] = serde_json::json!(c.ssh_authorized_keys);
    let network = serde_json::json!({
        "version": 2,
        "ethernets": { "uplink": {
            "match": { "macaddress": c.mac },
            "addresses": [format!("{}/32", c.ip)],
            "routes": [{ "to": "0.0.0.0/0", "via": c.gateway, "on-link": true }],
            "nameservers": { "addresses": c.nameservers },
        }},
    });
    let meta = serde_json::json!({ "instance-id": format!("{}-{instance}", c.name), "local-hostname": c.name });
    Ok([
        ("user-data".into(), format!("#cloud-config\n{user}")),
        ("network-config".into(), network.to_string()),
        ("meta-data".into(), meta.to_string()),
    ])
}

/// Checks what a credential command printed: `KEY=value` lines, comments
/// and blanks, and at least one variable.
pub fn check_environment(out: &str) -> Result<(), String> {
    let mut any = false;
    for line in out.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.split_once('=').is_some_and(|(k, _)| config::is_env_name(k)) {
            return Err("the credential command printed something that isn't KEY=value".into());
        }
        any = true;
    }
    if any { Ok(()) } else { Err("the credential command printed nothing".into()) }
}

/// What the owner command says of someone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// Exit 0: they may keep their workspaces.
    Stays,
    /// Exit 1: they left.
    Left,
    /// Anything else, such as the directory being down: nothing is decided.
    Unknown,
}

impl Owner {
    #[must_use]
    pub fn of(code: Option<i32>) -> Self {
        match code {
            Some(0) => Owner::Stays,
            Some(1) => Owner::Left,
            _ => Owner::Unknown,
        }
    }
}

/// Asks the owner command about `owner`.
async fn owner_status(t: &Template, owner: &str) -> Owner {
    match ask(&t.owner_command, owner).await {
        Ok((code, _)) => Owner::of(code),
        Err(e) => {
            eprintln!("{}: {e:#}", t.owner_command);
            Owner::Unknown
        }
    }
}

/// Runs one of the host's commands with `owner` as its argument: its exit
/// code, none when a signal ended it, and what it printed.
async fn ask(cmd: &str, owner: &str) -> anyhow::Result<(Option<i32>, String)> {
    let run = Command::new(cmd).arg(owner).stdin(Stdio::null()).stderr(Stdio::inherit()).kill_on_drop(true).output();
    let out = tokio::time::timeout(COMMAND_TIMEOUT, run).await.with_context(|| format!("{cmd} took too long"))??;
    let stdout = String::from_utf8_lossy(&out.stdout[..out.stdout.len().min(1 << 20)]).into_owned();
    Ok((out.status.code(), stdout))
}

/// A refusal people are shown, as opposed to the host failing.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

fn refuse(msg: impl Into<String>) -> anyhow::Error {
    Refused(msg.into()).into()
}

/// Everything a workspace made at run time has on the host, but its config.
fn host_files(paths: &Paths, c: &Config) -> Vec<PathBuf> {
    let root = &paths.0;
    let seed = paths.seed(&c.name);
    vec![
        PathBuf::from(&c.state_dir),
        PathBuf::from(&c.secrets_dir),
        seed.with_extension("d"),
        seed.with_extension("new"),
        seed,
        state::stop_marker(root, &c.name),
        left_marker(root, &c.name),
        Path::new(USERDB).join(format!("{}.user", c.user)),
        Path::new(USERDB).join(format!("{}.user", c.uid)),
        Path::new(NETWORK).join(format!("40-{}.netdev", c.tap)),
        Path::new(NETWORK).join(format!("40-{}.network", c.tap)),
    ]
}

/// Removes what a workspace has on the host, its tap included.
async fn remove_host_files(paths: &Paths, c: &Config) -> anyhow::Result<()> {
    for p in host_files(paths, c) {
        remove(&p).with_context(|| p.display().to_string())?;
    }
    let _ = run_tool("networkctl", &["delete", &c.tap]).await;
    run_tool("networkctl", &["reload"]).await
}

fn write_pending(paths: &Paths, c: &Config) -> anyhow::Result<()> {
    let file = paths.pending(&c.name);
    let dir = file.parent().context("pending")?;
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    hostutil::atomic_write(&file, &serde_json::to_vec(c)?, 0o600)?;
    Ok(())
}

/// Under the lock: finishes what a create or a destroy that died half way
/// left. A pending workspace with a config is whole, and one without is
/// cleaned up.
async fn finish_pending(paths: &Paths) -> anyhow::Result<()> {
    let dir = paths.dir().join("pending");
    let Ok(entries) = fs::read_dir(&dir) else { return Ok(()) };
    let mut failed = Vec::new();
    for e in entries.flatten() {
        let file = e.path();
        let parsed = fs::read(&file).map_err(anyhow::Error::from).and_then(|raw| config::parse(&raw));
        let done = match parsed {
            Ok(c) if paths.configs().join(format!("{}.json", c.name)).exists() => Ok(()),
            Ok(c) => remove_host_files(paths, &c).await,
            Err(e) => Err(e),
        };
        match done {
            Ok(()) => remove(&file)?,
            Err(e) => failed.push(format!("{}: {e:#}", file.display())),
        }
    }
    ensure!(failed.is_empty(), "{}", failed.join("; "));
    Ok(())
}

fn kvm_gid() -> anyhow::Result<u32> {
    Ok(nix::unistd::Group::from_name("kvm")?.context("no group kvm")?.gid.as_raw())
}

/// Writes a workspace's userdb record and networkd files.
fn write_host_files(c: &Config, kvm: u32) -> anyhow::Result<()> {
    fs::create_dir_all(USERDB)?;
    let record = Path::new(USERDB).join(format!("{}.user", c.user));
    hostutil::atomic_write(&record, user_record(c, kvm).as_bytes(), 0o644)?;
    let by_uid = Path::new(USERDB).join(format!("{}.user", c.uid));
    remove(&by_uid)?;
    std::os::unix::fs::symlink(format!("{}.user", c.user), &by_uid)?;
    fs::create_dir_all(NETWORK)?;
    let (netdev, network) = network_files(c);
    for (ext, body) in [("netdev", netdev), ("network", network)] {
        let file = Path::new(NETWORK).join(format!("40-{}.{ext}", c.tap));
        hostutil::atomic_write(&file, body.as_bytes(), 0o644)?;
    }
    Ok(())
}

async fn run_tool(tool: &str, args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new(tool).args(args).kill_on_drop(true).output().await.with_context(|| tool.to_owned())?;
    ensure!(out.status.success(), "{tool} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    Ok(())
}

/// Builds an Ubuntu seed as the Nix module does: a FAT image labelled
/// `CIDATA`.
async fn write_seed(t: &Template, user_data: &str, c: &Config, at: &Path) -> anyhow::Result<()> {
    let instance = jiff::Timestamp::now().as_second().to_string();
    let files = seed_files(user_data, c, &instance)?;
    let dir = at.with_extension("d");
    let img = at.with_extension("new");
    let _ = remove(&dir);
    fs::create_dir_all(&dir)?;
    let made = async {
        let mut paths = Vec::new();
        for (name, body) in &files {
            let p = dir.join(name);
            fs::write(&p, body)?;
            paths.push(p.display().to_string());
        }
        remove(&img)?;
        let img_s = img.display().to_string();
        run_tool(&t.mkfs_vfat, &["-n", "CIDATA", "-i", "12345678", "-C", &img_s, "1024"]).await?;
        let mut args = vec!["-m", "-i", img_s.as_str()];
        args.extend(paths.iter().map(String::as_str));
        args.push("::");
        run_tool(&t.mcopy, &args).await?;
        fs::set_permissions(&img, fs::Permissions::from_mode(0o644))?;
        fs::rename(&img, at)?;
        anyhow::Ok(())
    }
    .await;
    let _ = remove(&dir);
    let _ = remove(&img);
    made
}

/// `workbench self-service create <ws>`: makes the workspace the web UI
/// asked for, and writes how it went for the UI to say.
pub async fn create(dirs: &[PathBuf], t: &Template, name: &str) -> anyhow::Result<()> {
    ensure!(is_workspace_name(name), "invalid workspace name {name:?}");
    let paths = t.paths();
    remove(&paths.outcome(name))?;
    let result = make(dirs, t, &paths, name).await;
    let outcome = match &result {
        Ok(()) => Outcome { ok: true, error: String::new() },
        Err(e) => Outcome {
            ok: false,
            error: e
                .downcast_ref::<Refused>()
                .map_or_else(|| format!("The host couldn't make it: {e:#}"), |r| r.0.clone()),
        },
    };
    write_outcome(&paths, name, &outcome)?;
    let _ = fs::remove_file(paths.request(name));
    result
}

fn write_outcome(paths: &Paths, name: &str, outcome: &Outcome) -> anyhow::Result<()> {
    let out = paths.outcome(name);
    fs::create_dir_all(out.parent().context("outcome")?)?;
    hostutil::atomic_write(&out, &serde_json::to_vec(outcome)?, 0o644)?;
    Ok(())
}

async fn make(dirs: &[PathBuf], t: &Template, paths: &Paths, name: &str) -> anyhow::Result<()> {
    let raw = read_theirs(&paths.request(name)).context("the request")?;
    let r: Request = serde_json::from_slice(&raw).context("the request")?;
    ensure!(r.name == name, "the request names {:?}", r.name);
    ensure!(is_user_name(&r.owner) && r.owner != "root", "invalid owner {:?}", r.owner);
    r.check(&t.limits).map_err(refuse)?;

    let lock = paths.lock()?;
    finish_pending(paths).await?;

    // Fails closed: a config that doesn't load may hold any address.
    let (all, unreadable) = every_config(dirs);
    ensure!(unreadable.is_empty(), "configs the host can't read: {}", unreadable.join("; "));
    // A secrets directory left by an earlier workspace of the name would
    // reach the new one's guest.
    let leftover = [Path::new(&t.state_dir).join(name), Path::new(&t.secrets_root).join(name), paths.seed(name)];
    if all.iter().any(|c| c.name == name) || leftover.iter().any(|p| p.exists()) {
        return Err(refuse(format!("{name} is taken: pick another name.")));
    }
    let quota = quota_of(&read_quotas(&paths.quotas())?, t.limits.quota, &r.owner);
    let made: Vec<Config> = all.iter().filter(|c| c.self_service).cloned().collect();
    if owned(&made, &r.owner) >= quota {
        return Err(refuse(format!("You have all the workspaces your quota allows ({quota}). Ask an admin for more.")));
    }
    if !t.owner_command.is_empty() {
        match owner_status(t, &r.owner).await {
            Owner::Stays => {}
            Owner::Left => return Err(refuse("You may not make workspaces here. Ask an admin.")),
            Owner::Unknown => return Err(refuse("The host couldn't check who you are just now. Try again later.")),
        }
    }
    let (code, env) = ask(&t.environment_command, &r.owner).await?;
    if code != Some(0) || check_environment(&env).is_err() {
        return Err(refuse("Your workspace has no credential for its app yet. Ask an admin."));
    }

    let ip = free_ip(&t.subnet, &all.iter().map(|c| c.ip.clone()).collect())?;
    let uids: BTreeSet<u32> = all.iter().map(|c| c.uid).collect();
    let uid = free_uid(&uids, |u| nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(u)).ok().flatten().is_some())?;
    let c = config_of(t, &r, &ip, uid)?;

    // Markers an earlier workspace of the name left would keep this one
    // stopped.
    let root = Path::new(&t.state_dir);
    remove(&state::stop_marker(root, name))?;
    remove(&left_marker(root, name))?;
    write_pending(paths, &c)?;
    let made = async {
        write_host_files(&c, kvm_gid()?)?;
        run_tool("networkctl", &["reload"]).await?;
        if !c.seed.is_empty() {
            let at = PathBuf::from(&c.seed);
            // The VM's user reaches its seed by its name, and lists nothing.
            let dir = at.parent().context("seed")?;
            fs::create_dir_all(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o711))?;
            write_seed(t, &t.images[&c.os].user_data, &c, &at).await?;
        }
        let secrets = PathBuf::from(&c.secrets_dir);
        fs::create_dir_all(&secrets)?;
        fs::set_permissions(&secrets, fs::Permissions::from_mode(0o700))?;
        hostutil::atomic_write(secrets.join("environment"), env.as_bytes(), 0o600)?;
        let vm_user = wait_for_user(&c.user).await?;
        launch::prepare_state(&c, &vm_user)?;
        fs::create_dir_all(paths.configs())?;
        let file = paths.configs().join(format!("{name}.json"));
        hostutil::atomic_write(&file, &serde_json::to_vec_pretty(&c)?, 0o644)?;
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = made {
        // Without a config, the pending record has it cleaned up; if that
        // fails too, the next request under the lock tries again.
        if let Err(again) = finish_pending(paths).await {
            eprintln!("{name}: cleaning up after a failed create: {again:#}");
        }
        return Err(e);
    }
    remove(&paths.pending(name))?;
    drop(lock);
    run_tool("systemctl", &["--no-block", "start", &state::unit_name(name)]).await
}

/// The VM user, once the user database has its record.
async fn wait_for_user(user: &str) -> anyhow::Result<Account> {
    for _ in 0..50 {
        if let Ok(a) = hostutil::lookup_account(user) {
            return Ok(a);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    hostutil::lookup_account(user).with_context(|| format!("the user database has no {user}"))
}

/// `workbench self-service destroy <ws>`: stops a workspace made at run
/// time and deletes its disk, its config and the rest of it, saying how it
/// went for the web UI.
pub async fn destroy(t: &Template, name: &str) -> anyhow::Result<()> {
    ensure!(is_workspace_name(name), "invalid workspace name {name:?}");
    let paths = t.paths();
    remove(&paths.outcome(name))?;
    let result = take_away(&paths, name).await;
    if let Err(e) = &result {
        let error =
            e.downcast_ref::<Refused>().map_or_else(|| format!("The host couldn't destroy it: {e:#}"), |r| r.0.clone());
        write_outcome(&paths, name, &Outcome { ok: false, error })?;
    }
    result
}

async fn take_away(paths: &Paths, name: &str) -> anyhow::Result<()> {
    let _lock = paths.lock()?;
    finish_pending(paths).await?;
    let Some(c) = config::load(&[paths.configs()], name).ok().filter(|c| c.self_service) else {
        bail!(Refused(format!("{name} wasn't made at run time: only the Nix configuration removes it.")));
    };
    let unit = state::unit_name(name);
    run_tool("systemctl", &["stop", &unit]).await?;
    ensure!(!hostutil::unit_running(&unit).await.unwrap_or(true), "{unit} is still running; nothing deleted");
    // The config first, so nothing starts the VM again; the pending record
    // has whatever is left cleaned up if this dies half way.
    write_pending(paths, &c)?;
    remove(&paths.configs().join(format!("{name}.json")))?;
    remove(&paths.outcome(name))?;
    finish_pending(paths).await
}

fn remove(p: &Path) -> io::Result<()> {
    let r = if p.is_dir() && !p.is_symlink() { fs::remove_dir_all(p) } else { fs::remove_file(p) };
    match r {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// `workbench self-service setup`, at boot: the user records and network
/// files of every workspace made at run time again, then the ones neither
/// stopped nor left start.
pub async fn setup(t: &Template) -> anyhow::Result<()> {
    let paths = t.paths();
    let _lock = paths.lock()?;
    // One workspace failing leaves the others to come up.
    let mut failed = Vec::new();
    if let Err(e) = finish_pending(&paths).await {
        failed.push(format!("{e:#}"));
    }
    failed.extend(every_config(&[paths.configs()]).1);
    let made = made(&paths);
    let kvm = kvm_gid()?;
    let mut ready = Vec::new();
    for c in &made {
        match write_host_files(c, kvm) {
            Ok(()) => ready.push(c),
            Err(e) => failed.push(format!("{}: {e:#}", c.name)),
        }
    }
    run_tool("networkctl", &["reload"]).await?;
    let root = Path::new(&t.state_dir);
    for c in ready {
        if !state::stop_marker(root, &c.name).exists()
            && !left_marker(root, &c.name).exists()
            && let Err(e) = run_tool("systemctl", &["--no-block", "start", &state::unit_name(&c.name)]).await
        {
            failed.push(format!("{}: {e:#}", c.name));
        }
    }
    ensure!(failed.is_empty(), "{}", failed.join("; "));
    Ok(())
}

/// `workbench self-service reconcile`: stops and marks the workspaces of
/// owners the owner command no longer keeps, and unmarks those of owners
/// who came back. Disks stay.
pub async fn reconcile(t: &Template) -> anyhow::Result<()> {
    if t.owner_command.is_empty() {
        return Ok(());
    }
    let root = Path::new(&t.state_dir);
    let paths = t.paths();
    // Asked without the lock, which a slow directory would hold up.
    let owners: BTreeSet<String> = made(&paths).into_iter().map(|c| c.owner).collect();
    let mut said: BTreeMap<String, Owner> = BTreeMap::new();
    for owner in owners {
        let o = owner_status(t, &owner).await;
        said.insert(owner, o);
    }
    // Then under it, so a workspace destroyed meanwhile isn't marked again.
    let _lock = paths.lock()?;
    let mut failed = Vec::new();
    for c in made(&paths) {
        let owner = said.get(&c.owner).copied().unwrap_or(Owner::Unknown);
        if let Err(e) = settle(root, &c, owner).await {
            failed.push(format!("{}: {e:#}", c.name));
        }
    }
    ensure!(failed.is_empty(), "{}", failed.join("; "));
    Ok(())
}

/// Brings one workspace in line with what the owner command said: one
/// whose owner left is marked, and stopped whenever it runs, at every
/// pass, whoever started it. The mark, not `workbench stop`'s, keeps it
/// stopped across boots, so one its owner hadn't stopped starts again
/// when they come back.
async fn settle(root: &Path, c: &Config, owner: Owner) -> anyhow::Result<()> {
    let marker = left_marker(root, &c.name);
    let unit = state::unit_name(&c.name);
    match owner {
        Owner::Unknown => eprintln!("{}: couldn't tell whether {} stays; left as it is", c.name, c.owner),
        Owner::Stays => {
            if marker.exists() {
                remove(&marker)?;
                eprintln!("{}: {} is back", c.name, c.owner);
                if !state::stop_marker(root, &c.name).exists() {
                    run_tool("systemctl", &["--no-block", "start", &unit]).await?;
                }
            }
        }
        Owner::Left => {
            if !marker.exists() {
                fs::create_dir_all(marker.parent().context("marker")?)?;
                fs::write(&marker, "")?;
                eprintln!("{}: {} left; stopping it, disk kept", c.name, c.owner);
            }
            if hostutil::unit_running(&unit).await.unwrap_or(true) {
                run_tool("systemctl", &["--no-block", "stop", &unit]).await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn template() -> Template {
        Template {
            limits: Limits {
                os: vec!["ubuntu".into(), "nixos".into()],
                boot_mem_mib: 2048,
                max_memory_gib: 16,
                max_disk_gib: 128,
                quota: 1,
            },
            domain: "ws.test".into(),
            scheme: "https".into(),
            state_dir: "/var/lib/workbench".into(),
            secrets_root: "/var/lib/workbench-secrets".into(),
            subnet: "10.232.0".into(),
            nameservers: vec!["1.1.1.1".into()],
            app_port: 3773,
            control_port: 5001,
            agent_port: 5002,
            cpus: 0,
            cloud_hypervisor: "/nix/store/x-ch/bin/cloud-hypervisor".into(),
            virtiofsd: "/nix/store/x-virtiofsd/bin/virtiofsd".into(),
            qemu_img: "/nix/store/x-qemu/bin/qemu-img".into(),
            firmware: "/nix/store/x-ovmf/FV/CLOUDHV.fd".into(),
            closure: "/nix/store/x-closure/store-paths".into(),
            registration: "/nix/store/x-closure/registration".into(),
            app: "/nix/store/x-app".into(),
            environment: BTreeMap::new(),
            environment_files: vec![],
            socket_group: "workbench".into(),
            images: BTreeMap::from([
                (
                    "ubuntu".into(),
                    Image {
                        image: "/nix/store/x-ubuntu.img".into(),
                        image_format: "qcow2".into(),
                        user_data: "#cloud-config\n{\"hostname\":\"@name@\",\"users\":[{\"name\":\"@owner@\",\
                                    \"ssh_authorized_keys\":[]}],\"write_files\":[{\"content\":\"User=@owner@\"}]}"
                            .into(),
                    },
                ),
                (
                    "nixos".into(),
                    Image {
                        image: "/nix/store/x-nixos.qcow2".into(),
                        image_format: "qcow2".into(),
                        user_data: String::new(),
                    },
                ),
            ]),
            mkfs_vfat: "/nix/store/x-dosfstools/bin/mkfs.vfat".into(),
            mcopy: "/nix/store/x-mtools/bin/mcopy".into(),
            environment_command: "/run/current-system/sw/bin/env-of".into(),
            owner_command: String::new(),
        }
    }

    fn request(os: &str) -> Request {
        Request {
            name: "samuel-silva-data".into(),
            owner: "samuel.silva".into(),
            os: os.into(),
            ssh_authorized_keys: vec!["ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMbJhk5H0h7O me@laptop".into()],
            max_memory_gib: 8,
            disk_gib: 64,
        }
    }

    #[test]
    fn a_request_makes_a_config_like_the_modules() {
        let t = template();
        let c = config_of(&t, &request("ubuntu"), "10.232.0.11", 60_600).unwrap();
        assert_eq!((c.tap.as_str(), c.mac.as_str()), ("wb0ae8000b", "02:00:0a:e8:00:0b"));
        assert_eq!(c.url, "https://samuel-silva-data.ws.test");
        assert_eq!(c.user, "wb-samuel-silva-data");
        assert_eq!(c.seed, "/var/lib/workbench/.seeds/samuel-silva-data.img");
        assert_eq!((c.max_mem_mib, c.boot_mem_mib, c.disk_gib), (8192, 2048, 64));
        assert!(c.self_service && c.uid == 60_600);
        let nixos = config_of(&t, &request("nixos"), "10.232.0.12", 60_601).unwrap();
        assert!(nixos.seed.is_empty(), "NixOS takes no seed");
    }

    #[test]
    fn addresses_and_users() {
        let used = BTreeSet::from(["10.232.0.2".to_owned(), "10.232.0.128".to_owned(), "10.232.0.129".to_owned()]);
        assert_eq!(free_ip("10.232.0", &used).unwrap(), "10.232.0.130", "the upper half, which Nix leaves alone");
        let full: BTreeSet<String> = (128..=254).map(|n| format!("10.232.0.{n}")).collect();
        assert!(free_ip("10.232.0", &full).is_err());
        assert_eq!(free_uid(&BTreeSet::from([60_600]), |u| u == 60_601).unwrap(), 60_602);
    }

    /// Every config file counts for addresses and users: one a directory
    /// before shadows, and one that doesn't load, which stops a create.
    #[test]
    fn every_config_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let (nix, made) = (tmp.path().join("etc"), tmp.path().join("made"));
        let c = config_of(&template(), &request("ubuntu"), "10.232.0.130", 60_600).unwrap();
        let declared = Config { ip: "10.232.0.10".into(), self_service: false, ..c.clone() };
        for (dir, c) in [(&nix, &declared), (&made, &c)] {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join(format!("{}.json", c.name)), serde_json::to_vec(c).unwrap()).unwrap();
        }
        let (ok, bad) = every_config(&[nix.clone(), made.clone()]);
        assert_eq!(ok.iter().map(|c| c.ip.as_str()).collect::<Vec<_>>(), ["10.232.0.10", "10.232.0.130"]);
        assert!(bad.is_empty());
        fs::write(made.join("broken.json"), b"{").unwrap();
        let (ok, bad) = every_config(&[nix, made]);
        assert_eq!((ok.len(), bad.len()), (2, 1), "{bad:?}");
    }

    #[test]
    fn host_files() {
        let c = config_of(&template(), &request("ubuntu"), "10.232.0.11", 60_600).unwrap();
        let record: serde_json::Value = serde_json::from_str(&user_record(&c, 302)).unwrap();
        assert_eq!(record["userName"], "wb-samuel-silva-data");
        assert_eq!((record["uid"].as_u64(), record["gid"].as_u64()), (Some(60_600), Some(302)));
        let (netdev, network) = network_files(&c);
        assert!(netdev.contains("Name=wb0ae8000b\nKind=tap") && netdev.contains("User=wb-samuel-silva-data"));
        assert!(network.contains("Address=10.232.0.1/32") && network.contains("Destination=10.232.0.11/32"));
    }

    #[test]
    fn seeds() {
        let t = template();
        let c = config_of(&t, &request("ubuntu"), "10.232.0.11", 60_600).unwrap();
        let [(_, user), (_, network), (_, meta)] = seed_files(&t.images["ubuntu"].user_data, &c, "1").unwrap();
        let user: serde_json::Value = serde_json::from_str(user.strip_prefix("#cloud-config\n").unwrap()).unwrap();
        assert_eq!(user["hostname"], "samuel-silva-data");
        assert_eq!(user["users"][0]["name"], "samuel.silva");
        assert_eq!(user["users"][0]["ssh_authorized_keys"][0], c.ssh_authorized_keys[0]);
        assert_eq!(user["write_files"][0]["content"], "User=samuel.silva");
        let network: serde_json::Value = serde_json::from_str(&network).unwrap();
        assert_eq!(network["ethernets"]["uplink"]["match"]["macaddress"], "02:00:0a:e8:00:0b");
        assert!(meta.contains("samuel-silva-data-1"));
    }

    #[test]
    fn only_exit_1_means_left() {
        assert_eq!(Owner::of(Some(0)), Owner::Stays);
        assert_eq!(Owner::of(Some(1)), Owner::Left);
        assert_eq!(Owner::of(Some(2)), Owner::Unknown, "a directory that is down decides nothing");
        assert_eq!(Owner::of(None), Owner::Unknown);
    }

    #[test]
    fn credentials_must_be_variables() {
        assert!(check_environment("API_KEY=abc\n").is_ok());
        assert!(check_environment("# a comment\n\nA=1\nB_2=x=y\n").is_ok());
        assert!(check_environment("").is_err());
        assert!(check_environment("\n# only a comment\n").is_err());
        assert!(check_environment("not a variable").is_err());
        assert!(check_environment("1A=x").is_err());
    }

    #[test]
    fn quotas_count_what_owners_made() {
        let t = template();
        let mut made = vec![config_of(&t, &request("ubuntu"), "10.232.0.11", 60_600).unwrap()];
        made.push(Config { owner: "bob".into(), ..made[0].clone() });
        assert_eq!((owned(&made, "samuel.silva"), owned(&made, "bob"), owned(&made, "carol")), (1, 1, 0));
    }
}
