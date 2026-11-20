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

use anyhow::{Context, bail};
use nix::unistd::{Gid, Uid, setgid, setgroups, setuid};
use serde::{Deserialize, Serialize};
use vmkit::ch::{self, Client, Disk, Net, Vm};
use vmkit::hostutil::{self, Account};
use vmkit::memory::GIB;
use vmkit::virtiofs;

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
    let vm_user = hostutil::lookup_account(&c.user).context("VM user")?;
    virtiofs::require_private_mounts()?;
    fs::metadata(Path::new("/sys/class/net").join(&c.tap)).with_context(|| format!("tap {}", c.tap))?;
    // Others may stat the disk, for its size, but not list or read it.
    ensure_dir(Path::new(&c.state_dir), 0o711, &vm_user)?;
    prepare_disk(&c, &vm_user).context("preparing disk")?;
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
    write_secrets_share(&c, &secrets.dir).context("writing the secrets share")?;
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
    fs::write(c.started_app(), format!("{}\n", c.app))?;
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
    pub app_port: i64,
    pub control_port: i64,
    pub agent_port: i64,
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
            app_port: c.app_port,
            control_port: c.control_port,
            agent_port: c.agent_port,
        }
    }
}

/// Writes what the guest starts its services from: the app in the closure,
/// the closure's paths and their registration, the owner's SSH keys, the
/// non-secret environment and `workspace.json`.
pub(crate) fn write_config_share(c: &Config, dir: &Path) -> anyhow::Result<()> {
    make_dir(dir, 0o755)?;
    std::os::unix::fs::symlink(&c.app, dir.join("app"))?;
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
    make_dir(dir, 0o755)?;
    let src = Path::new(&c.secrets_dir);
    let mut total = 0;
    if src.exists() {
        copy_secrets(src, dir, true, &mut total)?;
    }
    // The environment is the secrets directory's environment file followed
    // by every environment file, each starting on a line of its own.
    let mut env = match hostutil::read_bounded(src.join("environment"), MAX_ENV_FILE) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).context("secrets environment"),
    };
    for path in &c.environment_files {
        let raw = hostutil::read_bounded(path, MAX_ENV_FILE).with_context(|| path.clone())?;
        env.push(b'\n');
        env.extend(raw);
    }
    if !env.is_empty() {
        write_read_only(&dir.join("environment"), &env)?;
    }
    Ok(())
}

fn copy_secrets(src: &Path, dst: &Path, top: bool, total: &mut u64) -> anyhow::Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if top && name == "environment" {
            continue;
        }
        let (from, to) = (entry.path(), dst.join(&name));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            fs::create_dir(&to)?;
            fs::set_permissions(&to, fs::Permissions::from_mode(0o755))?;
            copy_secrets(&from, &to, false, total)?;
        } else if kind.is_file() {
            let raw = hostutil::read_bounded(&from, MAX_SECRET).with_context(|| from.display().to_string())?;
            *total += raw.len() as u64;
            if *total > MAX_SECRETS {
                bail!("secrets directory larger than 64 MiB");
            }
            write_read_only(&to, &raw)?;
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
        hotplug_mib: (c.max_mem_mib - c.boot_mem_mib).max(0),
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
        c.max_mem_mib = c.boot_mem_mib;
        assert_eq!(flag_values(&cloud_hypervisor_argv(&c, 16), "--memory"), ["size=2048M,shared=on"]);
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
        assert_eq!(fs::read_link(dir.join("app")).unwrap(), Path::new(&c.app));
        assert_eq!(fs::read_to_string(dir.join("environment")).unwrap(), "LLM_URL=\"https://llm.example\"\n");
        let gc: GuestConfig = serde_json::from_slice(&fs::read(dir.join("workspace.json")).unwrap()).unwrap();
        assert_eq!(gc, GuestConfig::of(&c));
        assert_eq!(gc.agent_port, 5002);
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
            ["agentPort", "appPort", "controlPort", "gateway", "ip", "name", "nameservers", "owner", "url"]
        );
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
