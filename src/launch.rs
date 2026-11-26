//! A workspace's unit: `run` prepares the disk and the shares, starts one
//! virtiofsd per share and then becomes cloud-hypervisor, so the VM is the
//! unit and not a child of any daemon; `shutdown` presses its power button.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::{PermissionsExt, chown};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use nix::unistd::{Gid, Uid, setgid, setgroups, setuid};
use serde::{Deserialize, Serialize};
use vmkit::ch::{self, Client, Disk, Net, Vm};
use vmkit::hostutil::{self, Account};
use vmkit::memory::GIB;
use vmkit::virtiofs;

use workbench_model::apps::app_url;

use crate::config::{self, Config};

/// The largest closure listing or registration copied to the guest.
const MAX_CLOSURE_FILE: u64 = 64 << 20;
/// The largest secrets file, and all of them together.
const MAX_SECRET: u64 = 16 << 20;
const MAX_SECRETS: u64 = 64 << 20;
/// The largest environment file.
const MAX_ENV_FILE: u64 = 1 << 20;

/// The workspace's unit's main process: it ends by becoming
/// cloud-hypervisor, and only returns on failure.
pub fn run(dirs: &[PathBuf], name: &str) -> anyhow::Result<()> {
    let c = config::load(dirs, name)?;
    boot(&c, None)
}

/// What a start is given rather than read on the host: on a node, what
/// the front answered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Given {
    pub secrets: Secrets,
    /// The guest's SSH host key, and the user CA it trusts; none from a
    /// front without the CAs.
    pub host_key: Option<HostKey>,
    pub user_ca: Option<Vec<u8>>,
}

/// Prepares the disk and the shares of a workspace, starts virtiofsd and
/// becomes cloud-hypervisor. The secrets and the SSH keys are the host's,
/// or those `given`.
pub fn boot(c: &Config, given: Option<&Given>) -> anyhow::Result<()> {
    let c = c.clone();
    let vm_user = hostutil::lookup_account(&c.user).context("VM user")?;
    virtiofs::require_private_mounts()?;
    wait_for_tap(&c.tap)?;
    prepare_state(&c, &vm_user)?;
    for dir in [c.vm_dir(), c.shares_dir()] {
        match fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e).context(dir.display().to_string()),
            _ => {}
        }
    }
    let root = Account { uid: 0, gid: 0, groups: vec![] };
    ensure_dir(Path::new(&c.runtime_dir), 0o711, &root)?;
    if c.socket_group.is_empty() {
        ensure_dir(&c.vm_dir(), 0o700, &vm_user)?;
    } else {
        // Setgid, so the sockets cloud-hypervisor makes are the group's;
        // `share-sockets` then lets the group use them.
        let group = nix::unistd::Group::from_name(&c.socket_group)?
            .with_context(|| format!("socket group {:?}", c.socket_group))?;
        let owner = Account { uid: vm_user.uid, gid: group.gid.as_raw(), groups: vec![] };
        ensure_dir(&c.vm_dir(), 0o2750, &owner)?;
    }
    ensure_dir(&c.shares_dir(), 0o700, &root)?;
    let [store, config_share, secrets] = c.shares();
    virtiofs::store_view(&c.closure, &store.dir).context("sharing the closure")?;
    write_config_share(&c, &config_share.dir).context("writing the config share")?;
    match given {
        // Until the modules render a fleet: a host on its own reads its
        // own secrets and signs its own host keys.
        None => {
            write_secrets_share(&c, &secrets.dir).context("writing the secrets share")?;
            // Without them people reach the guest the way they did before:
            // the VM starts all the same.
            if let Err(e) = write_ssh_keys(&c, &config_share.dir, &secrets.dir) {
                eprintln!("the guest's SSH host key and user CA: {e:#}");
            }
        }
        Some(g) => {
            write_secrets(&secrets.dir, &g.secrets).context("writing the secrets share")?;
            if let Some(ca) = &g.user_ca {
                write_user_ca(&config_share.dir, &c.owner, ca).context("sharing the user CA")?;
            }
            if let Some(key) = &g.host_key {
                write_host_key(&secrets.dir, key).context("sharing the SSH host key")?;
            }
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build()?;
    for share in c.shares() {
        let socket = c.share_socket(&share);
        // Not this process's child for long: cloud-hypervisor inherits it.
        Command::new(&c.virtiofsd)
            .args(virtiofsd_args(&socket, &share.dir))
            .stdin(Stdio::null())
            .stdout(io::stderr())
            .spawn()
            .with_context(|| format!("starting virtiofsd for {}", share.tag))?;
        runtime.block_on(hostutil::wait_for_socket(&socket, Duration::from_secs(10)))?;
        chown(&socket, Some(vm_user.uid), Some(vm_user.gid))?;
    }
    drop(runtime);
    // What the VM boots with, for as long as it runs; the web UI reads it.
    hostutil::atomic_write(c.booted(), &serde_json::to_vec(&c.boot())?, 0o644)?;
    let host_cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let argv = cloud_hypervisor_argv(&c, u32::try_from(host_cpus).unwrap_or(u32::MAX));
    eprintln!("starting workspace {} for {} at {}", c.name, c.owner, c.ip);
    let groups: Vec<Gid> = vm_user.groups.iter().map(|&g| Gid::from_raw(g)).collect();
    setgroups(&groups)?;
    setgid(Gid::from_raw(vm_user.gid))?;
    setuid(Uid::from_raw(vm_user.uid))?;
    let err =
        Command::new(&argv[0]).args(&argv[1..]).current_dir(c.vm_dir()).env_clear().env("PATH", "/var/empty").exec();
    Err(err).context("exec cloud-hypervisor")
}

/// Waits for the tap: systemd-networkd makes the tap of a workspace made
/// at run time once its files are in, which may be just before the start.
fn wait_for_tap(tap: &str) -> anyhow::Result<()> {
    let dev = Path::new("/sys/class/net").join(tap);
    for _ in 0..150 {
        if dev.exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    fs::metadata(&dev).with_context(|| format!("tap {tap}"))?;
    Ok(())
}

/// Makes the state directory and the disk, as the VM user's. Others may
/// stat the disk, for its size, but not list or read it.
pub(crate) fn prepare_state(c: &Config, vm_user: &Account) -> anyhow::Result<()> {
    prepare_state_dir(c, vm_user)?;
    prepare_disk(c, vm_user).context("preparing disk")
}

/// Makes the state directory alone, for a disk that comes from elsewhere.
pub(crate) fn prepare_state_dir(c: &Config, vm_user: &Account) -> anyhow::Result<()> {
    ensure_dir(Path::new(&c.state_dir), 0o711, vm_user)
}

/// Makes a directory with `mode`, owned by `owner`.
fn ensure_dir(dir: &Path, mode: u32, owner: &Account) -> anyhow::Result<()> {
    fs::create_dir_all(dir).with_context(|| dir.display().to_string())?;
    chown(dir, Some(owner.uid), Some(owner.gid))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Makes a directory with `mode`, owned by whoever runs this: root in the
/// unit.
fn make_dir(dir: &Path, mode: u32) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(mode))
}

fn write_read_only(path: &Path, data: &[u8]) -> io::Result<()> {
    fs::write(path, data)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o444))
}

/// Makes the persistent disk from the OS image on first start and grows it
/// to the configured size; it never shrinks it. The guest grows its root
/// partition to match at boot.
fn prepare_disk(c: &Config, vm_user: &Account) -> anyhow::Result<()> {
    let disk = c.disk();
    let size = c.disk_gib * GIB;
    if disk.try_exists()? {
        grow_file(&disk, size)?;
    } else {
        let tmp = disk.with_extension("raw.new");
        let _ = fs::remove_file(&tmp);
        let made = (|| {
            let out = Command::new(&c.qemu_img)
                .args(["convert", "-f", &c.image_format, "-O", "raw", &c.image])
                .arg(&tmp)
                .stdin(Stdio::null())
                .output()?;
            if !out.status.success() {
                bail!("qemu-img convert: {}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim());
            }
            grow_file(&tmp, size)?;
            Ok(())
        })();
        if made.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        made?;
        fs::rename(&tmp, &disk)?;
        eprintln!("created {} from {}", disk.display(), c.image);
    }
    chown(&disk, Some(vm_user.uid), Some(vm_user.gid))?;
    fs::set_permissions(&disk, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Grows a file to `size`, sparsely; a larger file is left as it is.
pub(crate) fn grow_file(path: &Path, size: i64) -> io::Result<()> {
    let size = u64::try_from(size).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "negative size"))?;
    let f = OpenOptions::new().write(true).open(path)?;
    if f.metadata()?.len() >= size {
        return Ok(());
    }
    f.set_len(size)?;
    f.sync_all()
}

/// `workspace.json` in the config share: what the guest agent and a NixOS
/// guest set themselves up from at each boot. It is a contract with
/// guests: see `docs/contracts.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuestConfig {
    pub name: String,
    pub owner: String,
    pub url: String,
    pub ip: String,
    pub gateway: String,
    pub nameservers: Vec<String>,
    /// The first port of an app, or 0: what guests of earlier releases had.
    pub app_port: i64,
    pub control_port: i64,
    pub agent_port: i64,
    /// The apps the agent runs.
    #[serde(default)]
    pub apps: Vec<GuestApp>,
}

/// An app in `workspace.json`: its bundle is `config/apps/<id>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestApp {
    pub id: String,
    pub name: String,
    /// Its port and its address, which a sign-in link must be on; neither
    /// for an app without an address, which is reached from a terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Its `serve` starts what it runs and ends; its unit stays active.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub oneshot: bool,
}

impl GuestConfig {
    #[must_use]
    pub fn of(c: &Config) -> Self {
        GuestConfig {
            name: c.name.clone(),
            owner: c.owner.clone(),
            url: c.url.clone(),
            ip: c.ip.clone(),
            gateway: c.gateway.clone(),
            nameservers: c.nameservers.clone(),
            app_port: c.apps.iter().find_map(|a| a.port).map_or(0, i64::from),
            control_port: c.control_port,
            agent_port: c.agent_port,
            apps: c
                .apps
                .iter()
                .map(|a| GuestApp {
                    id: a.id.clone(),
                    name: a.name.clone(),
                    port: a.port,
                    url: a.port.map(|_| app_url(&c.url, &a.id)),
                    oneshot: a.oneshot,
                })
                .collect(),
        }
    }

    #[must_use]
    pub fn app(&self, id: &str) -> Option<&GuestApp> {
        self.apps.iter().find(|a| a.id == id)
    }
}

/// Writes what the guest starts its services from: the packages, the
/// agent and the apps in the closure, the closure's paths and their registration, the
/// owner's SSH keys, the non-secret environment and `workspace.json`.
pub(crate) fn write_config_share(c: &Config, dir: &Path) -> anyhow::Result<()> {
    make_dir(dir, 0o755)?;
    std::os::unix::fs::symlink(&c.packages, dir.join("app"))?;
    if !c.agent.is_empty() {
        std::os::unix::fs::symlink(&c.agent, dir.join("agent"))?;
    }
    make_dir(&dir.join("apps"), 0o755)?;
    for app in &c.apps {
        std::os::unix::fs::symlink(&app.bundle, dir.join("apps").join(&app.id))?;
    }
    for (name, src) in [("store-paths", &c.closure), ("registration", &c.registration)] {
        let raw = hostutil::read_bounded(src, MAX_CLOSURE_FILE).with_context(|| src.clone())?;
        write_read_only(&dir.join(name), &raw)?;
    }
    let keys = dir.join("authorized_keys");
    make_dir(&keys, 0o755)?;
    let lines = c.ssh_authorized_keys.iter().fold(String::new(), |mut out, k| {
        let _ = writeln!(out, "{k}");
        out
    });
    write_read_only(&keys.join(&c.owner), lines.as_bytes())?;
    write_read_only(&dir.join("environment"), environment_file(&c.environment).as_bytes())?;
    let raw = serde_json::to_vec_pretty(&GuestConfig::of(c))?;
    write_read_only(&dir.join("workspace.json"), &raw)?;
    Ok(())
}

/// Renders variables for systemd's `EnvironmentFile=`, quoted so no value
/// is reinterpreted.
pub(crate) fn environment_file(env: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (k, v) in env {
        let v = v.replace('\\', r"\\").replace('"', "\\\"");
        let _ = writeln!(out, "{k}=\"{v}\"");
    }
    out
}

/// Copies the workspace's secrets directory into this run's share and
/// appends the environment files to its environment file, which the
/// guest's app service reads. Nothing of it is in the store.
pub(crate) fn write_secrets_share(c: &Config, dir: &Path) -> anyhow::Result<()> {
    let secrets = collect_secrets(Path::new(&c.secrets_dir), &c.environment_files)?;
    write_secrets(dir, &secrets)
}

/// What a secrets share holds: a copy of the secrets directory, but its
/// own `environment` and `ssh-host`, and the secret environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Secrets {
    /// Its directories, relative to it, each after its parent. Names are
    /// the file system's, UTF-8 or not.
    pub dirs: Vec<PathBuf>,
    /// Its files, relative to it, and what each holds.
    pub files: Vec<(PathBuf, Vec<u8>)>,
    /// The secrets directory's environment file followed by every
    /// environment file, each starting on a line of its own; empty for
    /// none.
    pub environment: Vec<u8>,
}

/// Reads a secrets directory, which may be missing, and the environment
/// files, which may not.
pub fn collect_secrets(src: &Path, environment_files: &[String]) -> anyhow::Result<Secrets> {
    let mut out = Secrets::default();
    let mut total = 0;
    if src.exists() {
        collect_dir(src, Path::new(""), &mut out, &mut total)?;
    }
    out.environment = match hostutil::read_bounded(src.join("environment"), MAX_ENV_FILE) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).context("secrets environment"),
    };
    for path in environment_files {
        let raw = hostutil::read_bounded(path, MAX_ENV_FILE).with_context(|| path.clone())?;
        out.environment.push(b'\n');
        out.environment.extend(raw);
    }
    Ok(out)
}

/// Writes a secrets share: directories anyone may list, files anyone in
/// the guest may read and no one write.
pub fn write_secrets(dir: &Path, secrets: &Secrets) -> anyhow::Result<()> {
    make_dir(dir, 0o755)?;
    for d in &secrets.dirs {
        ensure!(is_relative_path(d), "a secret directory {} outside the share", d.display());
        let to = dir.join(d);
        fs::create_dir(&to).with_context(|| to.display().to_string())?;
        fs::set_permissions(&to, fs::Permissions::from_mode(0o755))?;
    }
    for (name, data) in &secrets.files {
        ensure!(is_relative_path(name), "a secret file {} outside the share", name.display());
        write_read_only(&dir.join(name), data).with_context(|| name.display().to_string())?;
    }
    if !secrets.environment.is_empty() {
        write_read_only(&dir.join("environment"), &secrets.environment)?;
    }
    Ok(())
}

/// A relative path of named components, which stays below the directory
/// it is joined to.
fn is_relative_path(p: &Path) -> bool {
    p.components().next().is_some() && p.components().all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// The guest's SSH host key and its certificate, in the secrets share.
pub const SSH_HOST_DIR: &str = "ssh-host";
/// The guest's host key, one per workspace, kept by the host.
const SSH_HOST_KEY: &str = "ssh_host_ed25519_key";
/// How long a host certificate is good for: it is signed again at every
/// start.
const HOST_CERT_VALIDITY: &str = "-5m:+520w";

/// Shares the guest's SSH host key, which the host makes once and signs at
/// every start with its host CA for the workspace's address, in the
/// secrets share at `ssh-host/`; and the user CA's public key, which the
/// guest trusts for its owner, in the config share at `ssh/user-ca.pub`,
/// with the one principal it takes for them, their name, at
/// `ssh/principals/<owner>`: a certificate signs in as the owner, never
/// as another account. Nothing for a config from before the CAs.
pub(crate) fn write_ssh_keys(c: &Config, config_dir: &Path, secrets_dir: &Path) -> anyhow::Result<()> {
    if c.ssh.is_empty() {
        return Ok(());
    }
    let user_ca = hostutil::read_bounded(&c.ssh.user_ca, MAX_ENV_FILE).with_context(|| c.ssh.user_ca.clone())?;
    write_user_ca(config_dir, &c.owner, &user_ca)?;
    let key = sign_host_key(&c.ssh, &c.name, c.host_name())?;
    write_host_key(secrets_dir, &key)
}

/// A guest's SSH host key, its public key and a certificate for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostKey {
    pub private: Vec<u8>,
    pub public: Vec<u8>,
    pub certificate: Vec<u8>,
}

/// The host key of workspace `name`, made the first time and kept in the
/// CAs' `hostKeys`, with a certificate for `host_name` the host CA signs
/// now.
pub fn sign_host_key(ssh: &config::SshCa, name: &str, host_name: &str) -> anyhow::Result<HostKey> {
    ensure!(!ssh.is_empty(), "no SSH certificate authorities");
    let keys = Path::new(&ssh.host_keys).join(name);
    make_dir(&keys, 0o700)?;
    let key = keys.join(SSH_HOST_KEY);
    let keygen = |args: &[&str]| -> anyhow::Result<()> {
        let out = Command::new(&ssh.keygen).args(args).stdin(Stdio::null()).output().context("ssh-keygen")?;
        ensure!(out.status.success(), "ssh-keygen: {}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim());
        Ok(())
    };
    let key_arg = key.to_string_lossy().into_owned();
    if !key.exists() {
        keygen(&["-q", "-t", "ed25519", "-N", "", "-C", host_name, "-f", &key_arg])?;
    }
    let (public, cert) = (keys.join(format!("{SSH_HOST_KEY}.pub")), keys.join(format!("{SSH_HOST_KEY}-cert.pub")));
    let _ = fs::remove_file(&cert);
    let id = format!("{name} host");
    let public_arg = public.to_string_lossy().into_owned();
    keygen(&["-q", "-s", &ssh.host_ca, "-I", &id, "-h", "-n", host_name, "-V", HOST_CERT_VALIDITY, &public_arg])?;
    Ok(HostKey {
        private: hostutil::read_bounded(&key, MAX_ENV_FILE)?,
        public: hostutil::read_bounded(&public, MAX_ENV_FILE)?,
        certificate: hostutil::read_bounded(&cert, MAX_ENV_FILE)?,
    })
}

/// The user CA's public key, which the guest's sshd trusts, and the one
/// principal it takes for the owner, their name, in the config share.
pub fn write_user_ca(config_dir: &Path, owner: &str, user_ca: &[u8]) -> anyhow::Result<()> {
    make_dir(&config_dir.join("ssh"), 0o755)?;
    write_read_only(&config_dir.join("ssh/user-ca.pub"), user_ca)?;
    make_dir(&config_dir.join("ssh/principals"), 0o755)?;
    write_read_only(&config_dir.join("ssh/principals").join(owner), format!("{owner}\n").as_bytes())?;
    Ok(())
}

/// The guest's host key, its public key and its certificate, in the
/// secrets share's `ssh-host/`.
pub fn write_host_key(secrets_dir: &Path, key: &HostKey) -> anyhow::Result<()> {
    let share = secrets_dir.join(SSH_HOST_DIR);
    make_dir(&share, 0o755)?;
    // Root's in the guest: sshd refuses a host key others may read.
    fs::write(share.join(SSH_HOST_KEY), &key.private)?;
    fs::set_permissions(share.join(SSH_HOST_KEY), fs::Permissions::from_mode(0o400))?;
    write_read_only(&share.join(format!("{SSH_HOST_KEY}.pub")), &key.public)?;
    write_read_only(&share.join(format!("{SSH_HOST_KEY}-cert.pub")), &key.certificate)?;
    Ok(())
}

/// Reads one directory of a secrets directory, `rel` below it.
fn collect_dir(src: &Path, rel: &Path, out: &mut Secrets, total: &mut u64) -> anyhow::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(src)?.collect::<io::Result<_>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        // The host writes these itself.
        if rel.as_os_str().is_empty() && (name == "environment" || name == SSH_HOST_DIR) {
            continue;
        }
        let from = entry.path();
        let path = rel.join(&name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            out.dirs.push(path.clone());
            collect_dir(&from, &path, out, total)?;
        } else if kind.is_file() {
            let raw = hostutil::read_bounded(&from, MAX_SECRET).with_context(|| from.display().to_string())?;
            *total += raw.len() as u64;
            if *total > MAX_SECRETS {
                bail!("secrets directory larger than 64 MiB");
            }
            out.files.push((path, raw));
        }
    }
    Ok(())
}

fn virtiofsd_args(socket: &Path, dir: &Path) -> Vec<String> {
    let mut args = virtiofs::args(&socket.to_string_lossy(), &dir.to_string_lossy());
    args.push("--sandbox=namespace".into());
    args
}

/// Boots the workspace from firmware with its disk, the cloud-init seed of
/// an Ubuntu guest, its tap, vsock, the shares and a serial console socket.
/// Memory starts at the boot size and grows through virtio-mem.
pub(crate) fn cloud_hypervisor_argv(c: &Config, host_cpus: u32) -> Vec<String> {
    let cpus = u32::try_from(c.cpus).unwrap_or(0);
    let path = |p: PathBuf| p.to_string_lossy().into_owned();
    let mut disks = vec![Disk { path: path(c.disk()), ..Disk::default() }];
    if !c.seed.is_empty() {
        disks.push(Disk { path: c.seed.clone(), read_only: true, ..Disk::default() });
    }
    Vm {
        binary: c.cloud_hypervisor.clone(),
        api_socket: path(c.api_socket()),
        firmware: c.firmware.clone(),
        cpus: if cpus == 0 || cpus > host_cpus { host_cpus } else { cpus },
        memory_mib: c.boot_mem_mib,
        hotplug_mib: (c.max_mem_mib() - c.boot_mem_mib).max(0),
        disks,
        nets: vec![Net { tap: c.tap.clone(), mac: c.mac.clone() }],
        vsock_cid: 3,
        vsock_socket: path(c.vsock()),
        serial: format!("socket={}", c.serial().display()),
        shares: c.shares().iter().map(|s| ch::Share { tag: s.tag.into(), socket: path(c.share_socket(s)) }).collect(),
        ..Vm::default()
    }
    .args()
}

/// The unit's `ExecStartPost`: lets the socket group use the VM's API,
/// vsock and serial sockets once cloud-hypervisor has made them,
/// owner-only. Without a socket group it does nothing.
pub async fn share_sockets(dirs: &[PathBuf], name: &str, timeout: Duration) -> anyhow::Result<()> {
    let c = config::load(dirs, name)?;
    if c.socket_group.is_empty() {
        return Ok(());
    }
    for socket in [c.api_socket(), c.vsock(), c.serial()] {
        hostutil::wait_for_socket(&socket, timeout).await?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o660))?;
    }
    Ok(())
}

/// The unit's `ExecStop`: presses the VM's power button and waits for the
/// guest to power off, so a stop is a clean shutdown; systemd kills what is
/// left after.
pub async fn shutdown(dirs: &[PathBuf], name: &str, timeout: Duration) -> anyhow::Result<()> {
    let c = config::load(dirs, name)?;
    let client = Client::new(c.api_socket());
    let wait = async {
        if client.power_button().await.is_err() {
            return;
        }
        while client.info().await.is_ok() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    if tokio::time::timeout(timeout, wait).await.is_err() {
        eprintln!("{name} did not power off within {timeout:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::test_config;
    use workbench_model::apps::BootApp;

    fn flag_values(argv: &[String], flag: &str) -> Vec<String> {
        let mut values = vec![];
        for (i, a) in argv.iter().enumerate() {
            if a == flag {
                values.extend(argv[i + 1..].iter().take_while(|v| !v.starts_with("--")).cloned());
            }
        }
        values
    }

    #[test]
    fn cloud_hypervisor_args() {
        let mut c = test_config();
        let argv = cloud_hypervisor_argv(&c, 16);
        assert_eq!(argv[0], c.cloud_hypervisor);
        let want: &[(&str, &[&str])] = &[
            ("--api-socket", &["path=/run/workbench/alice-dev/vm/api.sock"]),
            ("--firmware", &["/nix/store/x-ovmf/FV/CLOUDHV.fd"]),
            ("--cpus", &["boot=16,max=16,nested=off"]),
            ("--memory", &["size=2048M,hotplug_method=virtio-mem,hotplug_size=14336M,shared=on"]),
            ("--balloon", &["free_page_reporting=on"]),
            (
                "--disk",
                &[
                    "path=/var/lib/workbench/alice-dev/disk.raw,image_type=raw",
                    "path=/nix/store/x-seed.img,readonly=on,image_type=raw",
                ],
            ),
            ("--net", &["tap=wb0ae8000a,mac=02:00:0a:e8:00:0a"]),
            ("--vsock", &["cid=3,socket=/run/workbench/alice-dev/vm/vsock.sock"]),
            ("--serial", &["socket=/run/workbench/alice-dev/vm/serial.sock"]),
            ("--console", &["off"]),
            (
                "--fs",
                &[
                    "tag=store,socket=/run/workbench/alice-dev/vm/fs-store.sock",
                    "tag=config,socket=/run/workbench/alice-dev/vm/fs-config.sock",
                    "tag=secrets,socket=/run/workbench/alice-dev/vm/fs-secrets.sock",
                ],
            ),
        ];
        for (flag, values) in want {
            assert_eq!(flag_values(&argv, flag), *values, "{flag}");
        }
        assert!(!argv.iter().any(|a| a == "--kernel"), "a firmware guest got a kernel");
        c.os = "nixos".into();
        c.seed = String::new();
        assert_eq!(flag_values(&cloud_hypervisor_argv(&c, 16), "--disk"), want[5].1[..1], "a NixOS guest's disks");
    }

    #[test]
    fn cloud_hypervisor_sizing() {
        let mut c = test_config();
        c.cpus = 4;
        assert_eq!(flag_values(&cloud_hypervisor_argv(&c, 16), "--cpus"), ["boot=4,max=4,nested=off"]);
        c.cpus = 64;
        assert_eq!(
            flag_values(&cloud_hypervisor_argv(&c, 16), "--cpus"),
            ["boot=16,max=16,nested=off"],
            "more vCPUs than host CPUs"
        );
        c.max_mem = config::Ceiling::Size(c.boot_mem_mib);
        assert_eq!(flag_values(&cloud_hypervisor_argv(&c, 16), "--memory"), ["size=2048M,shared=on"]);
        c.max_mem = config::Ceiling::Auto;
        let hotplug = config::host_mem_mib().unwrap() - 2048;
        assert_eq!(
            flag_values(&cloud_hypervisor_argv(&c, 16), "--memory"),
            [format!("size=2048M,hotplug_method=virtio-mem,hotplug_size={hotplug}M,shared=on")],
            "auto grows to the host's memory"
        );
    }

    #[test]
    fn environment_file_quotes() {
        let env = [("B".to_owned(), r#"say "hi" \o/"#.to_owned()), ("A".to_owned(), "$HOME stays literal".to_owned())];
        assert_eq!(environment_file(&env.into()), "A=\"$HOME stays literal\"\nB=\"say \\\"hi\\\" \\\\o/\"\n");
    }

    #[test]
    fn config_share() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("config");
        let mut c = test_config();
        c.closure = tmp.path().join("store-paths").to_string_lossy().into();
        c.registration = tmp.path().join("registration").to_string_lossy().into();
        fs::write(&c.closure, "/nix/store/x-app\n").unwrap();
        fs::write(&c.registration, "/nix/store/x-app\nsha256:0\n0\n\n0\n").unwrap();
        write_config_share(&c, &dir).unwrap();
        assert_eq!(fs::read_link(dir.join("app")).unwrap(), Path::new(&c.packages));
        assert_eq!(fs::read_link(dir.join("agent")).unwrap(), Path::new(&c.agent));
        // A config from before the agent was apart: the packages have it.
        let old = Config { agent: String::new(), ..c.clone() };
        assert!(!serde_json::to_string(&old).unwrap().contains("agent\""));
        write_config_share(&old, &tmp.path().join("old")).unwrap();
        assert!(!tmp.path().join("old/agent").exists());
        assert_eq!(fs::read_link(dir.join("apps/t3code")).unwrap(), Path::new(&c.apps[0].bundle));
        assert_eq!(fs::read_to_string(dir.join("environment")).unwrap(), "LLM_URL=\"https://llm.example\"\n");
        let gc: GuestConfig = serde_json::from_slice(&fs::read(dir.join("workspace.json")).unwrap()).unwrap();
        assert_eq!(gc, GuestConfig::of(&c));
        assert_eq!(gc.agent_port, 5002);
        assert_eq!((gc.app_port, gc.apps[0].url.as_deref()), (3773, Some("https://alice-dev--t3code.ws.test")));
        for (name, src) in [("store-paths", &c.closure), ("registration", &c.registration)] {
            let (got, want) = (fs::read(dir.join(name)).unwrap(), fs::read(src).unwrap());
            assert!(!want.is_empty() && got == want, "{name} is not a copy of {src}");
        }
        assert_eq!(fs::read_to_string(dir.join("authorized_keys/alice")).unwrap(), "ssh-ed25519 AAAA alice@laptop\n");
    }

    /// `workspace.json` keeps the field names guests of earlier releases
    /// read.
    #[test]
    fn guest_config_fields() {
        let v = serde_json::to_value(GuestConfig::of(&test_config())).unwrap();
        let mut keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["agentPort", "appPort", "apps", "controlPort", "gateway", "ip", "name", "nameservers", "owner", "url"]
        );
        // An app without an address has neither port nor url.
        let mut c = test_config();
        c.apps.insert(0, BootApp { id: "herdr".into(), port: None, ..c.apps[0].clone() });
        let gc = GuestConfig::of(&c);
        assert_eq!(gc.app_port, 3773, "the first app with a port");
        assert_eq!(serde_json::to_value(&gc.apps[0]).unwrap(), serde_json::json!({"id": "herdr", "name": "T3 Code"}));
    }

    #[test]
    fn secrets_share() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = test_config();
        let secrets = tmp.path().join("secrets");
        c.secrets_dir = secrets.to_string_lossy().into();
        fs::create_dir_all(secrets.join("git")).unwrap();
        fs::write(secrets.join("environment"), "ANTHROPIC_API_KEY=sk-1").unwrap();
        fs::write(secrets.join("git/credentials"), "https://u:p@git").unwrap();
        let extra = tmp.path().join("extra.env");
        fs::write(&extra, "OPENAI_API_KEY=sk-2\n").unwrap();
        c.environment_files = vec![extra.to_string_lossy().into()];
        let share = tmp.path().join("share");
        write_secrets_share(&c, &share).unwrap();
        // A secrets file without a final newline must not swallow the next
        // variable.
        assert_eq!(
            fs::read_to_string(share.join("environment")).unwrap(),
            "ANTHROPIC_API_KEY=sk-1\nOPENAI_API_KEY=sk-2\n"
        );
        assert_eq!(fs::read_to_string(share.join("git/credentials")).unwrap(), "https://u:p@git");
        let mode = fs::metadata(share.join("git/credentials")).unwrap().permissions().mode();
        assert_eq!(mode & 0o222, 0, "secret copy is writable");

        c.secrets_dir = tmp.path().join("missing").to_string_lossy().into();
        c.environment_files = vec![];
        write_secrets_share(&c, &tmp.path().join("empty")).expect("a missing secrets directory leaves an empty share");
        c.environment_files = vec![tmp.path().join("nope.env").to_string_lossy().into()];
        assert!(write_secrets_share(&c, &tmp.path().join("broken")).is_err(), "a missing environment file was ignored");
    }

    /// A host on its own copies a secret whatever its name, UTF-8 or not.
    #[test]
    fn secrets_of_any_name() {
        use std::os::unix::ffi::OsStrExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let mut c = test_config();
        let secrets = tmp.path().join("secrets");
        let odd = std::ffi::OsStr::from_bytes(b"caf\xe9.key");
        fs::create_dir_all(secrets.join(odd)).unwrap();
        fs::write(secrets.join(odd).join(odd), "k").unwrap();
        c.secrets_dir = secrets.to_string_lossy().into();
        c.environment_files = vec![];
        let share = tmp.path().join("share");
        write_secrets_share(&c, &share).unwrap();
        assert_eq!(fs::read(share.join(odd).join(odd)).unwrap(), b"k");
        let outside = Secrets { files: vec![("../x".into(), vec![])], ..Secrets::default() };
        assert!(write_secrets(&tmp.path().join("other"), &outside).is_err(), "a secret outside the share");
    }

    /// The guest gets a host key of its own, the same at every start, and a
    /// certificate for its address from the host CA; it trusts the user
    /// CA. A config from before the CAs shares neither.
    #[test]
    fn ssh_host_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let ca = tmp.path().join("ca");
        fs::create_dir(&ca).unwrap();
        let keygen = |args: &[&str]| {
            let out = Command::new("ssh-keygen").args(args).output().expect("ssh-keygen on the PATH");
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        for name in ["host-ca", "user-ca"] {
            keygen(&["-q", "-t", "ed25519", "-N", "", "-f", &ca.join(name).to_string_lossy()]);
        }
        let mut c = test_config();
        c.ssh.keygen = "ssh-keygen".into();
        c.ssh.host_ca = ca.join("host-ca").to_string_lossy().into();
        c.ssh.user_ca = ca.join("user-ca.pub").to_string_lossy().into();
        c.ssh.host_keys = tmp.path().join("hosts").to_string_lossy().into();
        let share = |run: &str| {
            let (config, secrets) = (tmp.path().join(run).join("config"), tmp.path().join(run).join("secrets"));
            make_dir(&secrets, 0o755).unwrap();
            make_dir(&config, 0o755).unwrap();
            write_ssh_keys(&c, &config, &secrets).unwrap();
            (config, secrets.join(SSH_HOST_DIR))
        };
        let (config, host) = share("1");
        assert_eq!(fs::read(config.join("ssh/user-ca.pub")).unwrap(), fs::read(ca.join("user-ca.pub")).unwrap());
        let principals: Vec<_> =
            fs::read_dir(config.join("ssh/principals")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(principals, ["alice"], "a certificate signs in as the owner only");
        assert_eq!(fs::read_to_string(config.join("ssh/principals/alice")).unwrap(), "alice\n");
        let mode = fs::metadata(host.join(SSH_HOST_KEY)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o400, "sshd takes only a key others can't read");
        let cert = keygen(&["-L", "-f", &host.join(format!("{SSH_HOST_KEY}-cert.pub")).to_string_lossy()]);
        assert!(cert.contains("host certificate") && cert.contains("alice-dev.ws.test"), "{cert}");
        let (_, again) = share("2");
        let public = |dir: &Path| fs::read(dir.join(format!("{SSH_HOST_KEY}.pub"))).unwrap();
        assert_eq!(public(&host), public(&again), "the same key at every start");
        let old = Config { ssh: crate::config::SshCa::default(), ..c.clone() };
        let empty = tmp.path().join("old");
        write_ssh_keys(&old, &empty, &empty).unwrap();
        assert!(!empty.exists(), "a config from before the CAs");
    }

    #[test]
    fn grow_file_never_shrinks() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("disk.raw");
        fs::write(&path, vec![0; 4096]).unwrap();
        grow_file(&path, 1 << 20).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 1 << 20);
        grow_file(&path, 4096).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 1 << 20, "shrunk");
    }
}
