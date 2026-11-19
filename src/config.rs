//! A workspace's config, as the host module renders it to
//! `/etc/workbench/<name>.json`, and where its files and sockets live.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use vmkit::hostutil;
pub use workbench_model::{is_user_name, is_workspace_name};

/// Searched in order for `<workspace>.json`: the declarative configs Nix
/// renders, then configs made at run time.
pub const DEFAULT_CONFIG_DIRS: [&str; 2] = ["/etc/workbench", "/var/lib/workbench/configs"];

/// The largest config read.
const MAX_CONFIG: u64 = 1 << 20;

/// The largest disk, 1 PiB, and the most memory, 16 TiB: sizes in bytes
/// stay far from overflowing.
const MAX_DISK_GIB: i64 = 1 << 20;
const MAX_MEM_MIB: i64 = 1 << 24;

/// The longest path a Unix socket may have.
const MAX_SOCKET_PATH: usize = 107;

/// One workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Config {
    pub name: String,
    pub owner: String,
    /// The host user cloud-hypervisor runs as.
    pub user: String,
    pub url: String,
    /// Keeps the disk across restarts.
    pub state_dir: String,
    /// Holds the sockets and shares of one run.
    pub runtime_dir: String,

    pub cloud_hypervisor: String,
    pub virtiofsd: String,
    pub qemu_img: String,
    pub firmware: String,
    /// The guest's: `ubuntu`, set up by cloud-init from `seed`, its `NoCloud`
    /// disk, or `nixos`, set up by `workbench.nixosModules.guest` from the
    /// config share.
    pub os: String,
    /// The OS image a new disk is made from.
    pub image: String,
    pub image_format: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub seed: String,
    #[serde(rename = "diskGiB")]
    pub disk_gib: i64,

    pub cpus: i64,
    #[serde(rename = "bootMemMiB")]
    pub boot_mem_mib: i64,
    #[serde(rename = "maxMemMiB")]
    pub max_mem_mib: i64,

    pub tap: String,
    pub mac: String,
    pub ip: String,
    pub gateway: String,
    pub nameservers: Vec<String>,
    pub app_port: i64,

    pub ssh_authorized_keys: Vec<String>,

    /// The store paths the guest is shared.
    pub closure: String,
    /// Their validity, for a guest that copies them into a store of its own.
    pub registration: String,
    /// What the guest starts from.
    pub app: String,

    pub environment: BTreeMap<String, String>,
    pub environment_files: Vec<String>,
    pub secrets_dir: String,

    pub control_port: i64,
    pub agent_port: i64,

    /// When set, may use the VM's sockets: the web UI's group.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub socket_group: String,

    /// Made at run time by its owner, not declared in Nix.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub self_service: bool,
    /// The VM user's id, for one made at run time: its user is a userdb
    /// record the host writes with it.
    #[serde(skip_serializing_if = "is_zero")]
    pub uid: u32,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn is_mac(mac: &str) -> bool {
    let parts: Vec<&str> = mac.split(':').collect();
    parts.len() == 6
        && parts.iter().all(|p| p.len() == 2 && p.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
}

pub(crate) fn is_env_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'_')
}

/// An absolute path below `/` with no empty, `.` or `..` component and no
/// trailing slash, and without the characters cloud-hypervisor's arguments
/// split on.
fn is_clean_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else { return false };
    !path.contains([',', '\n']) && rest.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

fn port(p: i64) -> bool {
    (1..=65535).contains(&p)
}

impl Config {
    /// Checks everything the host relies on.
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(is_workspace_name(&self.name), "invalid workspace name {:?}", self.name);
        ensure!(is_user_name(&self.owner) && self.owner != "root", "invalid owner {:?}", self.owner);
        ensure!(is_user_name(&self.user) && self.user != "root", "invalid VM user {:?}", self.user);
        for (field, path) in [
            ("stateDir", &self.state_dir),
            ("runtimeDir", &self.runtime_dir),
            ("cloudHypervisor", &self.cloud_hypervisor),
            ("virtiofsd", &self.virtiofsd),
            ("qemuImg", &self.qemu_img),
            ("firmware", &self.firmware),
            ("image", &self.image),
            ("closure", &self.closure),
            ("registration", &self.registration),
            ("app", &self.app),
            ("secretsDir", &self.secrets_dir),
        ] {
            ensure!(is_clean_path(path), "{field} must be a clean absolute path without commas");
        }
        match self.os.as_str() {
            "ubuntu" => {
                ensure!(
                    is_clean_path(&self.seed),
                    "an Ubuntu guest needs a seed: a clean absolute path without commas"
                );
            }
            "nixos" => ensure!(self.seed.is_empty(), "a NixOS guest takes no cloud-init seed"),
            os => bail!("unknown guest OS {os:?}"),
        }
        for f in &self.environment_files {
            ensure!(f.starts_with('/'), "environment file {f:?} must be absolute");
        }
        ensure!(matches!(self.image_format.as_str(), "qcow2" | "raw"), "unknown image format {:?}", self.image_format);
        ensure!(
            (1..=MAX_DISK_GIB).contains(&self.disk_gib) && (0..=i64::from(u32::MAX)).contains(&self.cpus),
            "diskGiB must be positive and at most {MAX_DISK_GIB}, and cpus nonnegative"
        );
        ensure!(
            self.boot_mem_mib > 0
                && self.boot_mem_mib % 128 == 0
                && self.max_mem_mib >= self.boot_mem_mib
                && self.max_mem_mib % 128 == 0
                && self.max_mem_mib <= MAX_MEM_MIB,
            "bootMemMiB and maxMemMiB must be multiples of 128 with bootMemMiB <= maxMemMiB <= {MAX_MEM_MIB}"
        );
        ensure!(
            !self.tap.is_empty() && self.tap.len() <= 15 && !self.tap.contains(['/', ' ', '\t', '\n']),
            "invalid tap {:?}",
            self.tap
        );
        ensure!(is_mac(&self.mac), "invalid MAC {:?}", self.mac);
        for ip in [&self.ip, &self.gateway].into_iter().chain(&self.nameservers) {
            ensure!(ip.parse::<Ipv4Addr>().is_ok(), "invalid IPv4 address {ip:?}");
        }
        for key in &self.ssh_authorized_keys {
            ensure!(!key.is_empty() && !key.contains(['\n', '\r', '\0']), "an SSH key must be one nonempty line");
        }
        ensure!(port(self.app_port) && port(self.control_port) && port(self.agent_port), "invalid port");
        ensure!(self.control_port != self.agent_port, "controlPort and agentPort must differ");
        ensure!(
            self.socket_group.is_empty() || is_user_name(&self.socket_group),
            "invalid socket group {:?}",
            self.socket_group
        );
        for (k, v) in &self.environment {
            ensure!(is_env_name(k) && !v.contains(['\n', '\0']), "invalid environment variable {k:?}");
        }
        // The serial console socket is the longest path in the VM directory.
        ensure!(
            self.vm_dir().join("vsock.sock_65535").as_os_str().len() <= MAX_SOCKET_PATH,
            "runtimeDir too long for its sockets"
        );
        Ok(())
    }

    /// The persistent disk.
    #[must_use]
    pub fn disk(&self) -> PathBuf {
        Path::new(&self.state_dir).join("disk.raw")
    }

    /// cloud-hypervisor's directory: its sockets, made anew by every start.
    #[must_use]
    pub fn vm_dir(&self) -> PathBuf {
        Path::new(&self.runtime_dir).join("vm")
    }

    /// Where the shares of one run are made.
    #[must_use]
    pub fn shares_dir(&self) -> PathBuf {
        Path::new(&self.runtime_dir).join("shares")
    }

    #[must_use]
    pub fn api_socket(&self) -> PathBuf {
        self.vm_dir().join("api.sock")
    }

    #[must_use]
    pub fn vsock(&self) -> PathBuf {
        self.vm_dir().join("vsock.sock")
    }

    #[must_use]
    pub fn serial(&self) -> PathBuf {
        self.vm_dir().join("serial.sock")
    }

    /// Records the app the running VM started with, to tell when the host
    /// has a newer one.
    #[must_use]
    pub fn started_app(&self) -> PathBuf {
        Path::new(&self.runtime_dir).join("app")
    }

    /// The host side of the guest's control port.
    #[must_use]
    pub fn control_socket(&self) -> PathBuf {
        vmkit::ch::guest_port_socket(&self.vsock().to_string_lossy(), self.control_port_u32()).into()
    }

    /// The control port; [`Config::validate`] keeps it in range.
    #[must_use]
    pub fn control_port_u32(&self) -> u32 {
        u32::try_from(self.control_port).unwrap_or(0)
    }

    /// The agent port; [`Config::validate`] keeps it in range.
    #[must_use]
    pub fn agent_port_u32(&self) -> u32 {
        u32::try_from(self.agent_port).unwrap_or(0)
    }

    /// What the guest mounts read-only over virtiofs: its closure, the
    /// workspace's settings and its secrets.
    #[must_use]
    pub fn shares(&self) -> [Share; 3] {
        let dir = self.shares_dir();
        ["store", "config", "secrets"].map(|tag| Share { tag, dir: dir.join(tag) })
    }

    /// virtiofsd's socket for a share.
    #[must_use]
    pub fn share_socket(&self, share: &Share) -> PathBuf {
        self.vm_dir().join(format!("fs-{}.sock", share.tag))
    }
}

/// One host directory the guest mounts over virtiofs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    pub tag: &'static str,
    pub dir: PathBuf,
}

/// Parses and validates a config.
pub fn parse(raw: &[u8]) -> anyhow::Result<Config> {
    let c: Config = serde_json::from_slice(raw).context("workspace config")?;
    c.validate()?;
    Ok(c)
}

/// Finds a workspace's config in the first directory that has it.
pub fn load(dirs: &[PathBuf], name: &str) -> anyhow::Result<Config> {
    ensure!(is_workspace_name(name), "invalid workspace name {name:?}");
    for dir in dirs {
        let path = dir.join(format!("{name}.json"));
        let raw = match hostutil::read_bounded(&path, MAX_CONFIG) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            r => r.with_context(|| path.display().to_string())?,
        };
        let c = parse(&raw).with_context(|| path.display().to_string())?;
        ensure!(c.name == name, "{} names workspace {:?}", path.display(), c.name);
        // A workspace made at run time whose name the Nix configuration
        // took later: neither starts on the other's disk.
        let later = dirs.iter().skip_while(|d| *d != dir).skip(1);
        if let Some(other) = later.into_iter().find(|d| d.join(format!("{name}.json")).exists()) {
            bail!("{name} is in both {} and {}; remove one", dir.display(), other.display());
        }
        return Ok(c);
    }
    let dirs: Vec<_> = dirs.iter().map(|d| d.display().to_string()).collect();
    bail!("no workspace {name:?} in {}", dirs.join(", "))
}

/// The workspaces the config directories declare, sorted.
#[must_use]
pub fn configured(dirs: &[PathBuf]) -> Vec<String> {
    let mut names = BTreeSet::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for e in entries.flatten() {
            if let Some(name) = e.file_name().to_str().and_then(|n| n.strip_suffix(".json"))
                && is_workspace_name(name)
            {
                names.insert(name.to_owned());
            }
        }
    }
    names.into_iter().collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn test_config() -> Config {
        Config {
            name: "alice-dev".into(),
            owner: "alice".into(),
            user: "wb-alice-dev".into(),
            url: "https://alice-dev.ws.test".into(),
            state_dir: "/var/lib/workbench/alice-dev".into(),
            runtime_dir: "/run/workbench/alice-dev".into(),
            cloud_hypervisor: "/nix/store/x-ch/bin/cloud-hypervisor".into(),
            virtiofsd: "/nix/store/x-virtiofsd/bin/virtiofsd".into(),
            qemu_img: "/nix/store/x-qemu/bin/qemu-img".into(),
            firmware: "/nix/store/x-ovmf/FV/CLOUDHV.fd".into(),
            os: "ubuntu".into(),
            image: "/nix/store/x-ubuntu.img".into(),
            image_format: "qcow2".into(),
            seed: "/nix/store/x-seed.img".into(),
            disk_gib: 64,
            cpus: 0,
            boot_mem_mib: 2048,
            max_mem_mib: 16384,
            tap: "wb0ae8000a".into(),
            mac: "02:00:0a:e8:00:0a".into(),
            ip: "10.232.0.10".into(),
            gateway: "10.232.0.1".into(),
            nameservers: vec!["1.1.1.1".into()],
            app_port: 3773,
            ssh_authorized_keys: vec!["ssh-ed25519 AAAA alice@laptop".into()],
            closure: "/nix/store/x-closure/store-paths".into(),
            registration: "/nix/store/x-closure/registration".into(),
            app: "/nix/store/x-app".into(),
            environment: [("LLM_URL".to_owned(), "https://llm.example".to_owned())].into(),
            environment_files: vec![],
            secrets_dir: "/var/lib/workbench-secrets/alice-dev".into(),
            control_port: 5001,
            agent_port: 5002,
            socket_group: String::new(),
            self_service: false,
            uid: 0,
        }
    }

    #[test]
    fn validation() {
        type Change = fn(&mut Config);
        test_config().validate().unwrap();
        let mut nixos = test_config();
        nixos.os = "nixos".into();
        nixos.seed = String::new();
        nixos.validate().unwrap();
        let changes: &[(&str, Change)] = &[
            ("uppercase name", |c| c.name = "Alice".into()),
            ("name too long", |c| c.name = "a".repeat(29)),
            ("trailing dash", |c| c.name = "alice-".into()),
            ("root owner", |c| c.owner = "root".into()),
            ("owner with slash", |c| c.owner = "a/b".into()),
            ("root VM user", |c| c.user = "root".into()),
            ("relative state dir", |c| c.state_dir = "var/lib/x".into()),
            ("comma in disk path", |c| c.state_dir = "/var/lib/a,b".into()),
            ("unclean path", |c| c.seed = "/nix/store/../etc/shadow".into()),
            ("trailing slash", |c| c.image = "/nix/store/x/".into()),
            ("root as state dir", |c| c.state_dir = "/".into()),
            ("huge disk", |c| c.disk_gib = 9_000_000_000),
            ("huge memory", |c| c.max_mem_mib = 1 << 40),
            ("unknown format", |c| c.image_format = "vmdk".into()),
            ("boot over max", |c| c.boot_mem_mib = 32768),
            ("boot not aligned", |c| c.boot_mem_mib = 1000),
            ("long tap", |c| c.tap = "wb0123456789abcd".into()),
            ("bad mac", |c| c.mac = "02:00:0a".into()),
            ("ipv6", |c| c.ip = "fd00::1".into()),
            ("bad gateway", |c| c.gateway = "host".into()),
            ("bad nameserver", |c| c.nameservers = vec!["dns.example".into()]),
            ("two keys in one", |c| c.ssh_authorized_keys = vec!["ssh-ed25519 A\nssh-ed25519 B".into()]),
            ("unknown os", |c| c.os = "arch".into()),
            ("ubuntu without seed", |c| c.seed = String::new()),
            ("nixos with seed", |c| c.os = "nixos".into()),
            ("same vsock ports", |c| c.agent_port = c.control_port),
            ("bad socket group", |c| c.socket_group = "wheel users".into()),
            ("port out of range", |c| c.app_port = 70000),
            ("env newline", |c| c.environment = [("A".into(), "x\nB=y".into())].into()),
            ("env name", |c| c.environment = [("A-B".into(), "x".into())].into()),
            ("relative env file", |c| c.environment_files = vec!["keys.env".into()]),
            ("long runtime dir", |c| c.runtime_dir = format!("/run/{}", "x".repeat(100))),
        ];
        for (name, change) in changes {
            let mut c = test_config();
            change(&mut c);
            assert!(c.validate().is_err(), "{name}: accepted");
        }
    }

    #[test]
    fn parse_rejects_unknown_fields() {
        let raw = serde_json::to_vec(&test_config()).unwrap();
        parse(&raw).unwrap();
        let mut m: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        m["sudoers"] = "ALL".into();
        assert!(parse(&serde_json::to_vec(&m).unwrap()).is_err(), "an unknown field was accepted");
    }

    /// The module renders every field; the names are camelCase with the
    /// units capitalised as Nix writes them.
    #[test]
    fn field_names() {
        let v = serde_json::to_value(test_config()).unwrap();
        for key in ["diskGiB", "bootMemMiB", "maxMemMiB", "sshAuthorizedKeys", "environmentFiles", "qemuImg"] {
            assert!(v.get(key).is_some(), "{key} missing from {v}");
        }
    }

    /// A later directory only adds workspaces, which is where self-service
    /// configs go; a name in two directories loads from neither.
    #[test]
    fn config_directories_precedence() {
        let (etc, lib) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let write = |dir: &Path, c: &Config| {
            std::fs::write(dir.join(format!("{}.json", c.name)), serde_json::to_vec(c).unwrap()).unwrap();
        };
        let a = test_config();
        write(etc.path(), &a);
        let mut shadow = a.clone();
        shadow.owner = "mallory".into();
        write(lib.path(), &shadow);
        let mut b = test_config();
        b.name = "bob".into();
        b.owner = "bob".into();
        write(lib.path(), &b);
        std::fs::write(lib.path().join("notes.txt"), "").unwrap();
        std::fs::write(lib.path().join("Bad.json"), "").unwrap();
        let dirs = [etc.path().to_owned(), lib.path().to_owned()];
        assert_eq!(configured(&dirs), ["alice-dev", "bob"]);
        let both = load(&dirs, "alice-dev").unwrap_err().to_string();
        assert!(both.contains("in both"), "{both}");
        std::fs::remove_file(lib.path().join("alice-dev.json")).unwrap();
        assert_eq!(load(&dirs, "alice-dev").unwrap().owner, "alice");
        load(&dirs, "bob").unwrap();
        assert!(load(&dirs, "../etc/passwd").is_err(), "loaded a path outside the config directories");
        let mut renamed = test_config();
        renamed.name = "carol".into();
        std::fs::write(lib.path().join("dave.json"), serde_json::to_vec(&renamed).unwrap()).unwrap();
        assert!(load(&dirs, "dave").is_err(), "a config was loaded under another workspace's name");
    }
}
