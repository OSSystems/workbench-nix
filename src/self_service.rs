//! Workspaces made at run time: `workbench self-service create` makes one
//! from a request the web UI wrote, `apps` changes its apps, `destroy`
//! takes it away, `setup` puts back at boot what `/run` lost and brings
//! every config up to the host's template, and `reconcile` stops the
//! workspaces of owners who left. They run as root; the web UI only writes
//! requests and starts their units.
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
use workbench_model::apps::{AppInfo, BootApp};
use workbench_model::fleet::{NodeRefusal, Record};
use workbench_model::memory::Ceiling;
use workbench_model::self_service::{AppsRequest, Limits, MemoryRequest, Outcome, Request, quota_of};
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
    pub control_port: i64,
    pub agent_port: i64,
    pub cpus: i64,
    pub cloud_hypervisor: String,
    pub virtiofsd: String,
    pub qemu_img: String,
    pub firmware: String,
    pub closure: String,
    pub registration: String,
    /// The packages every such workspace has.
    pub packages: String,
    /// The agent.
    #[serde(default)]
    pub agent: String,
    /// The apps offered, in order: the closure has every one of them.
    pub apps: Vec<BootApp>,
    /// Those a new workspace has unless someone says otherwise.
    #[serde(default)]
    pub default_apps: Vec<String>,
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
    /// The host's SSH certificate authorities.
    #[serde(default)]
    pub ssh: config::SshCa,
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
    /// Reads the template; its limits offer its apps, and know the host's
    /// memory when their most is `auto`.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = hostutil::read_bounded(path, MAX_FILE).with_context(|| path.display().to_string())?;
        let mut t: Template = serde_json::from_slice(&raw).with_context(|| path.display().to_string())?;
        if t.limits.memory_auto() {
            t.limits.host_memory_gib = config::host_mem_mib()? / 1024;
        }
        t.limits.apps = t
            .apps
            .iter()
            .map(|a| AppInfo {
                id: a.id.clone(),
                name: a.name.clone(),
                terminal: a.port.is_none(),
                default: t.default_apps.contains(&a.id),
            })
            .collect();
        Ok(t)
    }

    /// The offered apps `ids` name, in that order.
    pub fn apps_of(&self, ids: &[String]) -> anyhow::Result<Vec<BootApp>> {
        ids.iter()
            .map(|id| self.apps.iter().find(|a| a.id == *id).cloned().with_context(|| format!("no app {id:?} offered")))
            .collect()
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

    /// A request to change a workspace's apps, and how it went.
    #[must_use]
    pub fn apps_request(&self, name: &str) -> PathBuf {
        self.dir().join("requests").join(format!("{name}.apps.json"))
    }

    #[must_use]
    pub fn apps_outcome(&self, name: &str) -> PathBuf {
        self.dir().join("results").join(format!("{name}.apps.json"))
    }

    /// A request to change the most memory a workspace grows to, and how
    /// it went.
    #[must_use]
    pub fn memory_request(&self, name: &str) -> PathBuf {
        self.dir().join("requests").join(format!("{name}.memory.json"))
    }

    #[must_use]
    pub fn memory_outcome(&self, name: &str) -> PathBuf {
        self.dir().join("results").join(format!("{name}.memory.json"))
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
    pub(crate) fn lock(&self) -> anyhow::Result<nix::fcntl::Flock<fs::File>> {
        lock_file(&self.0.join(".seeds").join(".lock"))
    }

    /// A request to move a workspace to another node, and how it went.
    #[must_use]
    pub fn move_request(&self, name: &str) -> PathBuf {
        self.dir().join("requests").join(format!("{name}.move.json"))
    }

    #[must_use]
    pub fn move_outcome(&self, name: &str) -> PathBuf {
        self.dir().join("results").join(format!("{name}.move.json"))
    }
}

/// Takes an exclusive lock on `path`, made if missing, until dropped.
pub(crate) fn lock_file(path: &Path) -> anyhow::Result<nix::fcntl::Flock<fs::File>> {
    fs::create_dir_all(path.parent().context("lock")?)?;
    let file = fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
    Ok(nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive).map_err(|(_, e)| e)?)
}

/// Reads a file the web UI wrote, without following a link it put there.
pub(crate) fn read_theirs(path: &Path) -> io::Result<Vec<u8>> {
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
pub(crate) fn every_config(dirs: &[PathBuf]) -> (Vec<Config>, Vec<String>) {
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
    let c = unchecked_config_of(t, r, ip, uid)?;
    c.validate()?;
    Ok(c)
}

fn unchecked_config_of(t: &Template, r: &Request, ip: &str, uid: u32) -> anyhow::Result<Config> {
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
        max_mem: r.max_memory.map(|gib| gib * 1024),
        tap,
        mac,
        ip: ip.to_owned(),
        gateway: format!("{}.1", t.subnet),
        nameservers: t.nameservers.clone(),
        ssh_authorized_keys: r.ssh_authorized_keys.clone(),
        closure: t.closure.clone(),
        registration: t.registration.clone(),
        packages: t.packages.clone(),
        agent: t.agent.clone(),
        apps: t.apps_of(&r.apps)?,
        legacy_app_port: 0,
        environment: t.environment.clone(),
        environment_files: t.environment_files.clone(),
        secrets_dir: format!("{}/{}", t.secrets_root, r.name),
        control_port: t.control_port,
        agent_port: t.agent_port,
        socket_group: t.socket_group.clone(),
        self_service: true,
        uid,
        ssh: t.ssh.clone(),
    };
    Ok(c)
}

/// A workspace's config made again from the template as it is now: its
/// own settings, address and VM user stay, the host's paths are the new
/// ones. What it was given stays too, whatever the host offers now: its
/// system, even one no longer offered, with its seed as it is, and its
/// memory. Apps the host no longer offers go. One from before apps gets
/// the apps a new workspace has, which then was the one.
pub fn refreshed(t: &Template, c: &Config) -> anyhow::Result<Config> {
    let mut apps: Vec<String> =
        c.apps.iter().filter(|a| t.apps.iter().any(|o| o.id == a.id)).map(|a| a.id.clone()).collect();
    if c.apps.is_empty() && c.legacy_app_port != 0 {
        apps = t.apps.iter().filter(|a| t.default_apps.contains(&a.id)).map(|a| a.id.clone()).collect();
    }
    let r = Request {
        name: c.name.clone(),
        owner: c.owner.clone(),
        os: c.os.clone(),
        ssh_authorized_keys: c.ssh_authorized_keys.clone(),
        max_memory: c.max_mem.map(|mib| mib / 1024),
        disk_gib: c.disk_gib,
        apps,
        node: String::new(),
    };
    let mut t = t.clone();
    let offered = t.images.contains_key(&c.os);
    if !offered {
        t.images.insert(
            c.os.clone(),
            Image { image: c.image.clone(), image_format: c.image_format.clone(), user_data: String::new() },
        );
    }
    t.limits.boot_mem_mib = t.limits.boot_mem_mib.min(c.max_mem_mib());
    let mut fresh = unchecked_config_of(&t, &r, &c.ip, c.uid)?;
    // Not what the template says now: what the disk was made from.
    fresh.image.clone_from(&c.image);
    fresh.image_format.clone_from(&c.image_format);
    if !offered {
        fresh.seed.clone_from(&c.seed);
    }
    fresh.validate()?;
    Ok(fresh)
}

/// The request a record stands for: the one that made it, or would have.
#[must_use]
pub fn request_of(r: &Record) -> Request {
    Request {
        name: r.name.clone(),
        owner: r.owner.clone(),
        os: r.os.clone(),
        ssh_authorized_keys: r.ssh_authorized_keys.clone(),
        max_memory: r.max_memory,
        disk_gib: r.disk_gib,
        apps: r.apps.clone(),
        node: String::new(),
    }
}

/// The record of a workspace a request makes on `node` at `ip`.
#[must_use]
pub fn record_of(req: &Request, node: &str, ip: &str) -> Record {
    Record {
        name: req.name.clone(),
        owner: req.owner.clone(),
        node: node.to_owned(),
        ip: ip.to_owned(),
        os: req.os.clone(),
        ssh_authorized_keys: req.ssh_authorized_keys.clone(),
        max_memory: req.max_memory,
        disk_gib: req.disk_gib,
        apps: req.apps.clone(),
        ..Record::default()
    }
}

/// The record of a config made at run time on a host on its own, which
/// becomes `node` of a fleet.
#[must_use]
pub fn record_of_config(c: &Config, node: &str) -> Record {
    Record {
        name: c.name.clone(),
        owner: c.owner.clone(),
        node: node.to_owned(),
        ip: c.ip.clone(),
        os: c.os.clone(),
        ssh_authorized_keys: c.ssh_authorized_keys.clone(),
        max_memory: c.max_mem.map(|mib| mib / 1024),
        disk_gib: c.disk_gib,
        apps: c.apps.iter().map(|a| a.id.clone()).collect(),
        ..Record::default()
    }
}

/// The config a node's template makes of a record, with the node's own
/// VM user `uid`: what [`config_of`] makes of the record's request.
pub fn render(t: &Template, r: &Record, uid: u32) -> anyhow::Result<Config> {
    config_of(t, &request_of(r), &r.ip, uid)
}

/// The config a node starts a workspace made at run time with: the
/// record, on what the node kept of it, its VM user, its address and the
/// image its disk came from, made again from the template as it is now.
pub fn render_kept(t: &Template, r: &Record, kept: &Config) -> anyhow::Result<Config> {
    ensure!(kept.self_service && kept.name == r.name, "{} wasn't made at run time on this node", r.name);
    ensure!(kept.ip == r.ip, "{} is at {} on this node, and {} in the registry", r.name, kept.ip, r.ip);
    let mut c = kept.clone();
    c.owner.clone_from(&r.owner);
    c.ssh_authorized_keys.clone_from(&r.ssh_authorized_keys);
    c.max_mem = r.max_memory.map(|gib| gib * 1024);
    c.disk_gib = r.disk_gib;
    c.apps = r.apps.iter().filter_map(|id| t.apps.iter().find(|a| a.id == *id).cloned()).collect();
    c.legacy_app_port = 0;
    refreshed(t, &c)
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
    owner_said(&t.owner_command, owner).await
}

/// Asks an owner command about `owner`.
pub(crate) async fn owner_said(cmd: &str, owner: &str) -> Owner {
    match ask(cmd, owner).await {
        Ok((code, _)) => Owner::of(code),
        Err(e) => {
            eprintln!("{cmd}: {e:#}");
            Owner::Unknown
        }
    }
}

/// Runs one of the host's commands with `owner` as its argument: its exit
/// code, none when a signal ended it, and what it printed.
pub(crate) async fn ask(cmd: &str, owner: &str) -> anyhow::Result<(Option<i32>, String)> {
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

pub(crate) fn refuse(msg: impl Into<String>) -> anyhow::Error {
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
        seed.with_extension("instance"),
        seed,
        state::stop_marker(root, &c.name),
        left_marker(root, &c.name),
        Path::new(USERDB).join(format!("{}.user", c.user)),
        Path::new(USERDB).join(format!("{}.user", c.uid)),
        Path::new(NETWORK).join(format!("40-{}.netdev", c.tap)),
        Path::new(NETWORK).join(format!("40-{}.network", c.tap)),
    ]
    .into_iter()
    .chain(c.host_key_dir())
    .collect()
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
pub(crate) async fn finish_pending(paths: &Paths) -> anyhow::Result<()> {
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

pub(crate) fn kvm_gid() -> anyhow::Result<u32> {
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

/// Whether a unit runs or is about to; an error counts as running, so
/// nothing is deleted under a VM.
pub(crate) async fn unit_running(unit: &str) -> bool {
    #[cfg(test)]
    if let Some(answer) = tools::stubbed("systemctl", &["is-active", unit]) {
        return answer.is_ok();
    }
    hostutil::unit_running(unit).await.unwrap_or(true)
}

/// A stand-in for the host's tools, for this thread's test: what it is
/// asked, and what it answers.
#[cfg(test)]
pub(crate) mod tools {
    use std::cell::RefCell;

    type Stub = Box<dyn Fn(&str, &[&str]) -> anyhow::Result<()>>;

    thread_local! {
        static STUB: RefCell<Option<Stub>> = const { RefCell::new(None) };
    }

    /// Answers every tool this thread runs with `stub` until it ends.
    pub(crate) fn stub(stub: impl Fn(&str, &[&str]) -> anyhow::Result<()> + 'static) {
        STUB.with(|s| *s.borrow_mut() = Some(Box::new(stub)));
    }

    pub(crate) fn stubbed(tool: &str, args: &[&str]) -> Option<anyhow::Result<()>> {
        STUB.with(|s| s.borrow().as_ref().map(|f| f(tool, args)))
    }
}

pub(crate) async fn run_tool(tool: &str, args: &[&str]) -> anyhow::Result<()> {
    #[cfg(test)]
    if let Some(answer) = tools::stubbed(tool, args) {
        return answer;
    }
    let out = Command::new(tool).args(args).kill_on_drop(true).output().await.with_context(|| tool.to_owned())?;
    ensure!(out.status.success(), "{tool} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    Ok(())
}

/// A seed's instance id, from what it says, as the Nix module makes it:
/// cloud-init sets the guest up again, its network included, when it
/// changes, and only then.
pub fn seed_instance(user_data: &str, c: &Config) -> anyhow::Result<String> {
    use sha2::Digest as _;
    let [(_, user), (_, network), _] = seed_files(user_data, c, "")?;
    let digest = sha2::Sha256::digest(format!("{user}{network}").as_bytes());
    Ok(digest.iter().take(6).fold(String::new(), |mut s, b| {
        let _ = std::fmt::Write::write_fmt(&mut s, format_args!("{b:02x}"));
        s
    }))
}

/// Writes a workspace's seed again when what it says changed, such as the
/// nameservers or the SSH keys: its guest takes it at its next boot.
async fn refresh_seed(t: &Template, c: &Config) -> anyhow::Result<()> {
    let Some(image) = t.images.get(&c.os).filter(|i| !i.user_data.is_empty()) else { return Ok(()) };
    if c.seed.is_empty() {
        return Ok(());
    }
    let at = PathBuf::from(&c.seed);
    let instance = seed_instance(&image.user_data, c)?;
    let known = at.with_extension("instance");
    if at.exists() && fs::read_to_string(&known).is_ok_and(|k| k.trim() == instance) {
        return Ok(());
    }
    write_seed(t, &image.user_data, c, &at).await
}

/// Builds an Ubuntu seed as the Nix module does: a FAT image labelled
/// `CIDATA`, and records its instance id next to it.
async fn write_seed(t: &Template, user_data: &str, c: &Config, at: &Path) -> anyhow::Result<()> {
    let instance = seed_instance(user_data, c)?;
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
        hostutil::atomic_write(at.with_extension("instance"), instance.as_bytes(), 0o600)?;
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
    let failed = "The host couldn't make it";
    reporting(&paths.outcome(name), Some(&paths.request(name)), failed, false, make(dirs, t, &paths, name)).await
}

/// Runs what a request asks for, and writes how it went for the web UI:
/// always, or `only_errors`; then the request goes.
pub(crate) async fn reporting(
    outcome: &Path,
    request: Option<&Path>,
    failed: &str,
    only_errors: bool,
    work: impl std::future::Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<()> {
    remove(outcome)?;
    let result = work.await;
    if result.is_err() || !only_errors {
        write_outcome_at(outcome, &outcome_of(&result, failed))?;
    }
    if let Some(r) = request {
        let _ = fs::remove_file(r);
    }
    result
}

/// The request the web UI wrote to make `name`, for an owner it may be.
pub(crate) fn read_request(paths: &Paths, name: &str) -> anyhow::Result<Request> {
    let raw = read_theirs(&paths.request(name)).context("the request")?;
    let r: Request = serde_json::from_slice(&raw).context("the request")?;
    ensure!(r.name == name, "the request names {:?}", r.name);
    ensure!(is_user_name(&r.owner) && r.owner != "root", "invalid owner {:?}", r.owner);
    Ok(r)
}

/// Refuses a name taken by a workspace, or by what one left.
pub(crate) fn taken(name: &str) -> anyhow::Error {
    refuse(format!("{name} is taken: pick another name."))
}

/// Refuses one more workspace past an owner's quota.
pub(crate) fn check_quota(owned: u32, quota: u32) -> anyhow::Result<()> {
    if owned >= quota {
        return Err(refuse(format!("You have all the workspaces your quota allows ({quota}). Ask an admin for more.")));
    }
    Ok(())
}

/// What an owner's new workspace needs from the host's commands: that the
/// owner command keeps them, if there is one, and the secret environment
/// the environment command prints for them.
pub(crate) async fn credential(owner_command: &str, environment_command: &str, owner: &str) -> anyhow::Result<String> {
    if !owner_command.is_empty() {
        match owner_said(owner_command, owner).await {
            Owner::Stays => {}
            Owner::Left => return Err(refuse("You may not make workspaces here. Ask an admin.")),
            Owner::Unknown => return Err(refuse("The host couldn't check who you are just now. Try again later.")),
        }
    }
    let (code, env) = ask(environment_command, owner).await?;
    if code != Some(0) || check_environment(&env).is_err() {
        return Err(refuse("Your workspace has no credential for its app yet. Ask an admin."));
    }
    Ok(env)
}

/// How a request went, as its unit wrote it; none when it wrote nothing
/// readable.
#[must_use]
pub fn read_outcome(path: &Path) -> Option<Outcome> {
    let raw = hostutil::read_bounded(path, 1 << 16).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// An outcome written at `since` or after: one from an earlier request
/// says nothing of this one.
#[must_use]
pub fn read_outcome_since(path: &Path, since: std::time::SystemTime) -> Option<Outcome> {
    let at = fs::metadata(path).and_then(|m| m.modified()).ok()?;
    (at >= since).then(|| read_outcome(path)).flatten()
}

pub(crate) fn write_outcome_at(out: &Path, outcome: &Outcome) -> anyhow::Result<()> {
    fs::create_dir_all(out.parent().context("outcome")?)?;
    hostutil::atomic_write(out, &serde_json::to_vec(outcome)?, 0o644)?;
    Ok(())
}

/// What a request's result says, for the web UI.
pub(crate) fn outcome_of(result: &anyhow::Result<()>, failed: &str) -> Outcome {
    match result {
        Ok(()) => Outcome { ok: true, ..Outcome::default() },
        Err(e) => {
            let node = e.chain().find_map(|c| c.downcast_ref::<NodeRefusal>()).cloned();
            let error = match (e.downcast_ref::<Refused>(), e.downcast_ref::<NodeRefusal>()) {
                (Some(r), _) => r.0.clone(),
                (None, Some(n)) if !n.failed() => n.to_string(),
                _ => format!("{failed}: {e:#}"),
            };
            Outcome { ok: false, error, node }
        }
    }
}

/// `workbench self-service apps <ws>`: gives a workspace made at run time
/// the apps the web UI asked for, in its config. The VM boots with them at
/// its next start; whether that is now is the UI's to ask.
pub async fn apps(t: &Template, name: &str) -> anyhow::Result<()> {
    ensure!(is_workspace_name(name), "invalid workspace name {name:?}");
    let paths = t.paths();
    let (outcome, request) = (paths.apps_outcome(name), paths.apps_request(name));
    reporting(&outcome, Some(&request), "The host couldn't change them", false, change_apps(t, &paths, name)).await
}

async fn change_apps(t: &Template, paths: &Paths, name: &str) -> anyhow::Result<()> {
    let raw = read_theirs(&paths.apps_request(name)).context("the request")?;
    let r: AppsRequest = serde_json::from_slice(&raw).context("the request")?;
    t.limits.check_apps(&r.apps).map_err(refuse)?;
    let _lock = paths.lock()?;
    finish_pending(paths).await?;
    let Some(c) = config::load(&[paths.configs()], name).ok().filter(|c| c.self_service) else {
        bail!(Refused(format!("{name} wasn't made at run time: its apps are in the Nix configuration.")));
    };
    let mut next = c;
    next.apps = t.apps_of(&r.apps)?;
    next.validate()?;
    write_config(paths, &next)
}

/// `workbench self-service memory <ws>`: gives a workspace made at run time
/// the most memory an admin asked for in the web UI, in its config. The VM
/// grows to it after its next start; whether that is now is the UI's to
/// ask.
pub async fn memory(t: &Template, name: &str) -> anyhow::Result<()> {
    ensure!(is_workspace_name(name), "invalid workspace name {name:?}");
    let paths = t.paths();
    let (outcome, request) = (paths.memory_outcome(name), paths.memory_request(name));
    reporting(&outcome, Some(&request), "The host couldn't change it", false, change_memory(t, &paths, name)).await
}

async fn change_memory(t: &Template, paths: &Paths, name: &str) -> anyhow::Result<()> {
    let raw = read_theirs(&paths.memory_request(name)).context("the request")?;
    let r: MemoryRequest = serde_json::from_slice(&raw).context("the request")?;
    let _lock = paths.lock()?;
    finish_pending(paths).await?;
    let Some(c) = config::load(&[paths.configs()], name).ok().filter(|c| c.self_service) else {
        bail!(Refused(format!("{name} wasn't made at run time: its memory is in the Nix configuration.")));
    };
    write_config(paths, &with_memory(t, &c, r.max_memory)?)
}

/// A workspace's config growing to `max` GiB at most, or `auto`, within
/// the template's limits. It boots with no more than that.
pub(crate) fn with_memory(t: &Template, c: &Config, max: Ceiling) -> anyhow::Result<Config> {
    t.limits.check_memory(max).map_err(refuse)?;
    let mut next = c.clone();
    next.max_mem = max.map(|gib| gib * 1024);
    next.boot_mem_mib = next.boot_mem_mib.min(next.max_mem_mib());
    next.validate()?;
    Ok(next)
}

/// Writes a workspace's config where the host reads it after the Nix ones.
pub(crate) fn write_config(paths: &Paths, c: &Config) -> anyhow::Result<()> {
    fs::create_dir_all(paths.configs())?;
    let file = paths.configs().join(format!("{}.json", c.name));
    hostutil::atomic_write(&file, &serde_json::to_vec_pretty(c)?, 0o644)?;
    Ok(())
}

async fn make(dirs: &[PathBuf], t: &Template, paths: &Paths, name: &str) -> anyhow::Result<()> {
    let r = read_request(paths, name)?;
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
        return Err(taken(name));
    }
    let quota = quota_of(&read_quotas(&paths.quotas())?, t.limits.quota, &r.owner);
    let made: Vec<Config> = all.iter().filter(|c| c.self_service).cloned().collect();
    check_quota(owned(&made, &r.owner), quota)?;
    let env = credential(&t.owner_command, &t.environment_command, &r.owner).await?;

    let ip = free_ip(&t.subnet, &all.iter().map(|c| c.ip.clone()).collect())?;
    let uids: BTreeSet<u32> = all.iter().map(|c| c.uid).collect();
    let uid = free_uid(&uids, |u| nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(u)).ok().flatten().is_some())?;
    let c = config_of(t, &r, &ip, uid)?;
    build(t, paths, &c, Build { disk: true, stopped: false, secret_environment: Some(&env) }).await?;
    drop(lock);
    run_tool("systemctl", &["--no-block", "start", &state::unit_name(name)]).await
}

/// How [`build`] makes a workspace.
pub(crate) struct Build<'a> {
    /// Its disk, from the image; otherwise only its state directory, for a
    /// disk that comes later.
    pub disk: bool,
    /// Kept stopped by its stop marker, until something starts it.
    pub stopped: bool,
    /// Its secrets directory with this environment, on a host that keeps
    /// it.
    pub secret_environment: Option<&'a str>,
}

/// Under the lock: makes on the host what a config of a workspace made at
/// run time names, then the config. If it fails half way, its pending
/// record has what it made cleaned up.
pub(crate) async fn build(t: &Template, paths: &Paths, c: &Config, how: Build<'_>) -> anyhow::Result<()> {
    // Markers an earlier workspace of the name left would keep this one
    // stopped.
    let root = Path::new(&t.state_dir);
    remove(&state::stop_marker(root, &c.name))?;
    remove(&left_marker(root, &c.name))?;
    write_pending(paths, c)?;
    let made = async {
        if how.stopped {
            state::mark_stopped(root, &c.name)?;
        }
        write_host_files(c, kvm_gid()?)?;
        run_tool("networkctl", &["reload"]).await?;
        if !c.seed.is_empty() {
            let at = PathBuf::from(&c.seed);
            // The VM's user reaches its seed by its name, and lists nothing.
            let dir = at.parent().context("seed")?;
            fs::create_dir_all(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o711))?;
            let image = t.images.get(&c.os).with_context(|| format!("no image for {:?}", c.os))?;
            write_seed(t, &image.user_data, c, &at).await?;
        }
        if let Some(env) = how.secret_environment {
            let secrets = PathBuf::from(&c.secrets_dir);
            fs::create_dir_all(&secrets)?;
            fs::set_permissions(&secrets, fs::Permissions::from_mode(0o700))?;
            hostutil::atomic_write(secrets.join("environment"), env.as_bytes(), 0o600)?;
        }
        let vm_user = wait_for_user(&c.user).await?;
        if how.disk {
            launch::prepare_state(c, &vm_user)?;
        } else {
            launch::prepare_state_dir(c, &vm_user)?;
        }
        write_config(paths, c)?;
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = made {
        // Without a config, the pending record has it cleaned up; if that
        // fails too, the next request under the lock tries again.
        if let Err(again) = finish_pending(paths).await {
            eprintln!("{}: cleaning up after a failed create: {again:#}", c.name);
        }
        return Err(e);
    }
    remove(&paths.pending(&c.name))?;
    Ok(())
}

/// The VM user, once the user database has its record.
pub(crate) async fn wait_for_user(user: &str) -> anyhow::Result<Account> {
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
    reporting(&paths.outcome(name), None, "The host couldn't destroy it", true, take_away(&paths, name)).await
}

pub(crate) async fn take_away(paths: &Paths, name: &str) -> anyhow::Result<()> {
    let _lock = paths.lock()?;
    finish_pending(paths).await?;
    let Some(c) = config::load(&[paths.configs()], name).ok().filter(|c| c.self_service) else {
        bail!(Refused(format!("{name} wasn't made at run time: only the Nix configuration removes it.")));
    };
    let unit = state::unit_name(name);
    run_tool("systemctl", &["stop", &unit]).await?;
    ensure!(!unit_running(&unit).await, "{unit} is still running; nothing deleted");
    // The config first, so nothing starts the VM again; the pending record
    // has whatever is left cleaned up if this dies half way.
    write_pending(paths, &c)?;
    remove(&paths.configs().join(format!("{name}.json")))?;
    remove(&paths.outcome(name))?;
    finish_pending(paths).await
}

pub(crate) fn remove(p: &Path) -> io::Result<()> {
    let r = if p.is_dir() && !p.is_symlink() { fs::remove_dir_all(p) } else { fs::remove_file(p) };
    match r {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// `workbench self-service setup`, at boot and with every new template:
/// each config of a workspace made at run time made again from the
/// template, their user records and network files again, then the ones
/// neither stopped nor left start. A VM that runs keeps what it booted
/// with until its next start.
pub async fn setup(t: &Template) -> anyhow::Result<()> {
    let paths = t.paths();
    let _lock = paths.lock()?;
    // One workspace failing leaves the others to come up.
    let mut failed = Vec::new();
    if let Err(e) = finish_pending(&paths).await {
        failed.push(format!("{e:#}"));
    }
    failed.extend(every_config(&[paths.configs()]).1);
    for c in made(&paths) {
        let fresh = refreshed(t, &c).and_then(|fresh| {
            if fresh != c {
                write_config(&paths, &fresh)?;
            }
            Ok(())
        });
        if let Err(e) = fresh {
            failed.push(format!("{}: keeping its config: {e:#}", c.name));
        }
    }
    let made = made(&paths);
    for c in &made {
        if let Err(e) = refresh_seed(t, c).await {
            failed.push(format!("{}: its seed: {e:#}", c.name));
        }
    }
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
            if unit_running(&unit).await {
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
                max_memory: Ceiling::Size(16),
                host_memory_gib: 0,
                max_disk_gib: 128,
                quota: 1,
                apps: vec![AppInfo { id: "t3code".into(), name: "T3 Code".into(), terminal: false, default: true }],
            },
            domain: "ws.test".into(),
            scheme: "https".into(),
            state_dir: "/var/lib/workbench".into(),
            secrets_root: "/var/lib/workbench-secrets".into(),
            subnet: "10.232.0".into(),
            nameservers: vec!["1.1.1.1".into()],
            control_port: 5001,
            agent_port: 5002,
            cpus: 0,
            cloud_hypervisor: "/nix/store/x-ch/bin/cloud-hypervisor".into(),
            virtiofsd: "/nix/store/x-virtiofsd/bin/virtiofsd".into(),
            qemu_img: "/nix/store/x-qemu/bin/qemu-img".into(),
            firmware: "/nix/store/x-ovmf/FV/CLOUDHV.fd".into(),
            closure: "/nix/store/x-closure/store-paths".into(),
            registration: "/nix/store/x-closure/registration".into(),
            packages: "/nix/store/x-packages".into(),
            agent: "/nix/store/x-agent".into(),
            apps: vec![BootApp {
                id: "t3code".into(),
                name: "T3 Code".into(),
                port: Some(3773),
                bundle: "/nix/store/x-t3code-app".into(),
                ..BootApp::default()
            }],
            default_apps: vec!["t3code".into()],
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
            ssh: crate::config::tests::test_config().ssh,
        }
    }

    fn request(os: &str) -> Request {
        Request {
            name: "samuel-silva-data".into(),
            owner: "samuel.silva".into(),
            os: os.into(),
            ssh_authorized_keys: vec!["ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMbJhk5H0h7O me@laptop".into()],
            max_memory: Ceiling::Size(8),
            disk_gib: 64,
            apps: vec!["t3code".into()],
            node: String::new(),
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
        assert_eq!((c.max_mem, c.boot_mem_mib, c.disk_gib), (Ceiling::Size(8192), 2048, 64));
        assert!(c.self_service && c.uid == 60_600);
        assert_eq!(c.apps, t.apps);
        let bare = config_of(&t, &Request { apps: vec![], ..request("ubuntu") }, "10.232.0.11", 60_600).unwrap();
        assert!(bare.apps.is_empty(), "a workspace with no app");
        let nixos = config_of(&t, &request("nixos"), "10.232.0.12", 60_601).unwrap();
        assert!(nixos.seed.is_empty(), "NixOS takes no seed");
    }

    /// An admin changes the most a workspace grows to within the limits,
    /// which every later deploy keeps.
    #[test]
    fn memory_changes_within_the_limits() {
        let t = template();
        let c = config_of(&t, &request("ubuntu"), "10.232.0.11", 60_600).unwrap();
        let more = with_memory(&t, &c, Ceiling::Size(16)).unwrap();
        assert_eq!((more.max_mem, more.boot_mem_mib), (Ceiling::Size(16_384), 2048));
        assert_eq!(Config { max_mem: c.max_mem, ..more.clone() }, c, "nothing else changes");
        assert_eq!(refreshed(&t, &more).unwrap().max_mem, Ceiling::Size(16_384), "a deploy keeps it");
        for gib in [1, 17] {
            let e = with_memory(&t, &c, Ceiling::Size(gib)).unwrap_err();
            assert_eq!(e.downcast_ref::<Refused>().unwrap().0, "Memory must be from 2 to 16 GiB.", "{gib}");
        }
        let e = with_memory(&t, &c, Ceiling::Auto).unwrap_err();
        assert!(e.downcast_ref::<Refused>().unwrap().0.contains("no auto"), "not on a host whose most is a size");
        // Where the most is the host's, a workspace may follow it, and
        // keeps following it through deploys.
        let mut hosts = t.clone();
        hosts.limits.max_memory = Ceiling::Auto;
        hosts.limits.host_memory_gib = 64;
        let auto = with_memory(&hosts, &c, Ceiling::Auto).unwrap();
        assert_eq!((auto.max_mem, auto.max_mem_mib()), (Ceiling::Auto, config::host_mem_mib().unwrap()));
        assert_eq!(refreshed(&hosts, &auto).unwrap().max_mem, Ceiling::Auto, "a deploy keeps it");
        let made =
            config_of(&hosts, &Request { max_memory: Ceiling::Auto, ..request("ubuntu") }, "10.232.0.12", 60_601);
        assert_eq!(made.unwrap().max_mem, Ceiling::Auto, "and one is made with it");
        assert_eq!(with_memory(&hosts, &auto, Ceiling::Size(16)).unwrap().max_mem, Ceiling::Size(16_384));
    }

    /// A template's most memory of `auto` is the host's, in whole GiB.
    #[test]
    fn template_memory_of_auto() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("template.json");
        let mut v = serde_json::to_value(template()).unwrap();
        v["limits"]["maxMemoryGiB"] = "auto".into();
        fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
        let limits = Template::load(&path).unwrap().limits;
        assert!(limits.memory_auto());
        assert_eq!(limits.max_memory_gib(), config::host_mem_mib().unwrap() / 1024);
    }

    /// Every config is made again from a new template: the host's paths
    /// change, the workspace's own settings don't, and one from before
    /// apps gets the app it had.
    #[test]
    fn configs_follow_the_template() {
        let t = template();
        let c = config_of(&t, &request("ubuntu"), "10.232.0.130", 60_601).unwrap();
        assert_eq!(refreshed(&t, &c).unwrap(), c, "nothing changed, nothing to write");
        let mut agent = template();
        agent.agent = "/nix/store/y-agent".into();
        let fresh = refreshed(&agent, &c).unwrap();
        assert_eq!(fresh.agent, "/nix/store/y-agent");
        assert!(
            workbench_model::apps::changes(Some(&c.boot()), &fresh.boot()).is_empty(),
            "a newer agent waits for nothing"
        );
        let mut newer = template();
        newer.packages = "/nix/store/y-packages".into();
        newer.apps[0].bundle = "/nix/store/y-t3code-app".into();
        newer.images.get_mut("ubuntu").unwrap().image = "/nix/store/y-ubuntu.img".into();
        let fresh = refreshed(&newer, &c).unwrap();
        assert_eq!(
            (fresh.packages.as_str(), fresh.apps[0].bundle.as_str()),
            ("/nix/store/y-packages", "/nix/store/y-t3code-app")
        );
        assert_eq!((fresh.ip.as_str(), fresh.uid, fresh.max_mem), (c.ip.as_str(), c.uid, c.max_mem));
        assert_eq!(fresh.image, c.image, "the image a disk was made from stays");
        let mut old = c.clone();
        old.apps.clear();
        old.legacy_app_port = 3773;
        assert_eq!(refreshed(&t, &old).unwrap().apps, t.apps);
        let mut opt_in = t.clone();
        opt_in.apps.push(BootApp { id: "herdr".into(), port: None, ..t.apps[0].clone() });
        assert_eq!(refreshed(&opt_in, &old).unwrap().apps, t.apps, "and only those a new workspace has");
        // A system no longer offered, or less memory than the host now
        // boots with, keeps what the workspace was given.
        let mut fewer = newer.clone();
        fewer.images.remove("ubuntu");
        fewer.limits.boot_mem_mib = c.max_mem_mib() + 1024;
        let kept = refreshed(&fewer, &c).unwrap();
        assert_eq!(
            (kept.os.as_str(), kept.seed.as_str(), kept.image.as_str()),
            (c.os.as_str(), c.seed.as_str(), c.image.as_str())
        );
        assert_eq!((kept.boot_mem_mib, kept.packages.as_str()), (c.max_mem_mib(), "/nix/store/y-packages"));
        let mut gone = template();
        gone.apps.clear();
        assert!(refreshed(&gone, &c).unwrap().apps.is_empty(), "an app no longer offered goes");
    }

    fn script(dir: &Path, name: &str, out: &str, code: i32) -> String {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\necho '{out}'\nexit {code}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }

    /// A host on its own and a front ask the host's commands the same:
    /// whether the owner stays, then their workspace's credential.
    #[tokio::test]
    async fn a_credential_from_the_host_s_commands() {
        let dir = tempfile::tempdir().unwrap();
        let env = script(dir.path(), "env", "API_KEY=sk-1", 0);
        let refusal = |e: anyhow::Error| e.downcast_ref::<Refused>().map(|r| r.0.clone()).unwrap_or_default();
        assert_eq!(credential("", &env, "alice").await.unwrap().trim(), "API_KEY=sk-1");
        for (owner, credential_command, why) in [
            (script(dir.path(), "left", "", 1), env.clone(), "may not make"),
            (script(dir.path(), "down", "", 2), env.clone(), "couldn't check"),
            (String::new(), script(dir.path(), "none", "", 0), "no credential"),
            (String::new(), script(dir.path(), "fails", "API_KEY=x", 3), "no credential"),
        ] {
            let e = credential(&owner, &credential_command, "alice").await.unwrap_err();
            assert!(refusal(e).contains(why), "{why}");
        }
        assert!(check_quota(1, 1).is_err() && check_quota(0, 1).is_ok());
    }

    /// A node renders a record into the config a host on its own makes of
    /// the same request, and back.
    #[test]
    fn a_record_renders_as_today() {
        let t = template();
        let mut hosts = t.clone();
        hosts.limits.max_memory = Ceiling::Auto;
        let auto = Request { max_memory: Ceiling::Auto, ..request("ubuntu") };
        for (t, req) in [(&t, request("ubuntu")), (&t, request("nixos")), (&hosts, auto)] {
            let record = record_of(&req, "a", "10.232.0.130");
            let today = config_of(t, &req, "10.232.0.130", 60_600).unwrap();
            assert_eq!(render(t, &record, 60_600).unwrap(), today, "{}", req.os);
            assert_eq!(request_of(&record), req);
            assert_eq!(record_of_config(&today, "a"), record);
            assert_eq!(render_kept(t, &record, &today).unwrap(), today, "a start changes nothing");
        }
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
        let id = seed_instance(&t.images["ubuntu"].user_data, &c).unwrap();
        assert_eq!((id.len(), seed_instance(&t.images["ubuntu"].user_data, &c).unwrap()), (12, id.clone()));
        let other = Config { nameservers: vec!["192.168.150.13".into()], ..c };
        assert_ne!(
            seed_instance(&t.images["ubuntu"].user_data, &other).unwrap(),
            id,
            "new nameservers, a new instance"
        );
    }

    #[test]
    fn only_exit_1_means_left() {
        assert_eq!(Owner::of(Some(0)), Owner::Stays);
        assert_eq!(Owner::of(Some(1)), Owner::Left);
        assert_eq!(Owner::of(Some(2)), Owner::Unknown, "a directory that is down decides nothing");
        assert_eq!(Owner::of(None), Owner::Unknown);
    }

    /// An outcome carries the node that refused, whose name only admins
    /// read; one the node failed says what failed.
    #[test]
    fn outcomes_say_which_node() {
        let down = NodeRefusal::Unreachable { node: "a".into() };
        let o = outcome_of(&Err(down.clone().into()), "The front couldn't make it");
        assert_eq!(
            (o.error.as_str(), o.node.as_ref()),
            ("Its node, a, doesn't answer just now. Try again later.", Some(&down))
        );
        let failed = NodeRefusal::Failed { node: "a".into(), error: "boom".into() };
        let o =
            outcome_of(&Err(anyhow::Error::from(failed.clone()).context("making it")), "The front couldn't make it");
        assert_eq!(o.error, "The front couldn't make it: making it: node a: boom");
        assert_eq!(o.node, Some(failed));
        assert_eq!(outcome_of(&Err(refuse("no")), "x"), Outcome { ok: false, error: "no".into(), node: None });
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
