//! Workspaces people make themselves: the name rule, the SSH keys they
//! paste, the host's limits and quotas, and the requests the web UI hands
//! to the host, to make one or change its apps. Both check a request with
//! the same code.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::apps::AppInfo;
use crate::is_workspace_name;
use crate::memory::Ceiling;

/// The smallest disk a workspace made at run time gets.
pub const MIN_DISK_GIB: i64 = 16;
/// The most SSH keys a request carries.
pub const MAX_SSH_KEYS: usize = 16;
/// The longest SSH key line.
const MAX_SSH_KEY: usize = 8192;

/// The key types a pasted SSH key may have.
const SSH_KEY_TYPES: [&str; 7] = [
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

/// Names the host keeps for its own directories in its state directory,
/// where a workspace's would be: `<stateDir>/<name>`.
pub const RESERVED: [&str; 5] = ["closed", "configs", "registry", "self-service", "ssh"];

/// Whether a workspace may not have `name`, which the host keeps.
#[must_use]
pub fn is_reserved(name: &str) -> bool {
    RESERVED.contains(&name)
}

/// A uid as the start of workspace names: `.` and `_` become `-`, so
/// `samuel.silva` owns `samuel-silva` and `samuel-silva-<suffix>`.
#[must_use]
pub fn slug(uid: &str) -> String {
    uid.replace(['.', '_'], "-")
}

/// Whether `owner` may name a workspace `name`: their slug, or their slug,
/// a dash and a suffix, and a workspace name either way.
pub fn check_name(owner: &str, name: &str) -> Result<(), String> {
    let slug = slug(owner);
    let theirs =
        name == slug || name.strip_prefix(&slug).and_then(|r| r.strip_prefix('-')).is_some_and(|s| !s.is_empty());
    if !theirs {
        return Err(format!("The name must be {slug} or start with {slug}-."));
    }
    if !is_workspace_name(name) {
        return Err(
            "The name must be lowercase letters, digits and dashes, at most 28 characters, starting with a letter."
                .into(),
        );
    }
    if is_reserved(name) {
        return Err(format!("{name} is a name the host keeps for itself: pick another."));
    }
    Ok(())
}

/// The SSH keys in pasted text, one per line, `<type> <key> [comment]`;
/// blank lines are skipped.
pub fn parse_ssh_keys(text: &str) -> Result<Vec<String>, String> {
    let mut keys = Vec::new();
    for (i, line) in text.lines().map(str::trim).enumerate() {
        if line.is_empty() {
            continue;
        }
        let n = i + 1;
        let mut parts = line.split_ascii_whitespace();
        let (Some(kind), Some(blob)) = (parts.next(), parts.next()) else {
            return Err(format!("SSH key on line {n}: expected <type> <key> [comment]."));
        };
        if !SSH_KEY_TYPES.contains(&kind) {
            return Err(format!("SSH key on line {n}: {kind:?} isn't a key type the host takes."));
        }
        let base64 =
            blob.len() >= 16 && blob.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'='));
        if !base64 || line.len() > MAX_SSH_KEY || line.chars().any(char::is_control) {
            return Err(format!("SSH key on line {n} isn't a public key."));
        }
        let comment: Vec<&str> = parts.collect();
        keys.push(if comment.is_empty() {
            format!("{kind} {blob}")
        } else {
            format!("{kind} {blob} {}", comment.join(" "))
        });
    }
    if keys.len() > MAX_SSH_KEYS {
        return Err(format!("At most {MAX_SSH_KEYS} SSH keys."));
    }
    Ok(keys)
}

/// What the host lets people make, from the Nix configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    /// The guest systems offered.
    pub os: Vec<String>,
    /// What every VM boots with.
    #[serde(rename = "bootMemMiB")]
    pub boot_mem_mib: i64,
    /// The most memory a VM may grow to, or `auto`: the host's.
    #[serde(rename = "maxMemoryGiB")]
    pub max_memory: Ceiling,
    /// The host's memory, in whole GiB, for a most of `auto`: the host
    /// reads it, the Nix configuration doesn't say.
    #[serde(rename = "hostMemoryGiB", default)]
    pub host_memory_gib: i64,
    #[serde(rename = "maxDiskGiB")]
    pub max_disk_gib: i64,
    /// How many workspaces a person may make, unless an admin set theirs.
    pub quota: u32,
    /// The apps offered, in the order the host gives them.
    #[serde(default)]
    pub apps: Vec<AppInfo>,
}

impl Limits {
    /// The least a VM may be allowed to grow to: its boot memory.
    #[must_use]
    pub fn min_memory_gib(&self) -> i64 {
        (self.boot_mem_mib + 1023) / 1024
    }

    /// The most memory a VM may be allowed to grow to, in GiB: the host's
    /// for `auto`.
    #[must_use]
    pub fn max_memory_gib(&self) -> i64 {
        self.max_memory.or_host(self.host_memory_gib)
    }

    /// Whether a VM may grow to `auto`, the host's memory: only where the
    /// most is the host's too.
    #[must_use]
    pub fn memory_auto(&self) -> bool {
        self.max_memory == Ceiling::Auto
    }

    /// Checks the most memory a VM may grow to: from its boot memory to
    /// the host's most, or `auto` where the host allows it.
    pub fn check_memory(&self, max: Ceiling) -> Result<(), String> {
        let (lo, hi) = (self.min_memory_gib(), self.max_memory_gib());
        match max {
            Ceiling::Auto if self.memory_auto() => Ok(()),
            Ceiling::Auto => Err(format!("Memory must be from {lo} to {hi} GiB: this host has no auto.")),
            Ceiling::Size(gib) if (lo..=hi).contains(&gib) => Ok(()),
            Ceiling::Size(_) => Err(format!("Memory must be from {lo} to {hi} GiB.")),
        }
    }

    /// Checks apps someone picked: offered ones, each once.
    pub fn check_apps(&self, apps: &[String]) -> Result<(), String> {
        for (i, id) in apps.iter().enumerate() {
            if !self.apps.iter().any(|a| a.id == *id) {
                return Err(format!("{id:?} isn't an app this host offers."));
            }
            if apps[..i].contains(id) {
                return Err(format!("{id:?} is picked twice."));
            }
        }
        Ok(())
    }
}

/// A person's quota: what an admin set for them, or the host's.
#[must_use]
pub fn quota_of(quotas: &BTreeMap<String, u32>, default: u32, user: &str) -> u32 {
    quotas.get(user).copied().unwrap_or(default)
}

/// A workspace someone asks for: what the web UI writes and the host's
/// `workbench self-service create` makes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub name: String,
    pub owner: String,
    pub os: String,
    pub ssh_authorized_keys: Vec<String>,
    #[serde(rename = "maxMemoryGiB")]
    pub max_memory: Ceiling,
    #[serde(rename = "diskGiB")]
    pub disk_gib: i64,
    /// The apps it has, by id.
    #[serde(default)]
    pub apps: Vec<String>,
    /// The node an admin picked for it, on a front with nodes; empty for
    /// the one with the most free memory.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub node: String,
}

impl Request {
    /// Checks the name rule and the limits; the quota and whether the
    /// name is free need the host.
    pub fn check(&self, limits: &Limits) -> Result<(), String> {
        check_name(&self.owner, &self.name)?;
        if !limits.os.contains(&self.os) {
            return Err(format!("{:?} isn't a system this host offers.", self.os));
        }
        let keys = parse_ssh_keys(&self.ssh_authorized_keys.join("\n"))?;
        if keys != self.ssh_authorized_keys {
            return Err("SSH keys must be one per entry.".into());
        }
        limits.check_memory(self.max_memory)?;
        if !(MIN_DISK_GIB..=limits.max_disk_gib).contains(&self.disk_gib) {
            return Err(format!("Disk must be from {MIN_DISK_GIB} to {} GiB.", limits.max_disk_gib));
        }
        limits.check_apps(&self.apps)
    }
}

/// The apps someone wants a workspace made at run time to have: what the
/// web UI writes and `workbench self-service apps` puts in its config. The
/// VM boots with them at its next start.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppsRequest {
    pub apps: Vec<String>,
}

/// The most memory an admin wants a workspace made at run time to grow to:
/// what the web UI writes and `workbench self-service memory` puts in its
/// config. The VM grows to it after its next start.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryRequest {
    #[serde(rename = "maxMemoryGiB")]
    pub max_memory: Ceiling,
}

/// How a request ended, for the web UI to say.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

/// What the New workspace page shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewView {
    pub limits: Limits,
    /// The viewer's slug, which names start with.
    pub slug: String,
    /// Workspaces made at run time the viewer owns, and their quota.
    pub owned: u32,
    pub quota: u32,
}

/// A person on the host page's quotas.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaRow {
    pub user: String,
    pub owned: u32,
    pub quota: u32,
    /// Whether an admin set it, rather than the host's default.
    pub own: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMbJhk5H0h7Oi79LSHLWfuffv6uFcuXtm77kewxrwQsD";

    #[test]
    fn names_start_with_the_owners_slug() {
        assert_eq!(slug("samuel.silva"), "samuel-silva");
        assert!(check_name("samuel.silva", "samuel-silva").is_ok());
        assert!(check_name("samuel.silva", "samuel-silva-data").is_ok());
        assert!(check_name("samuel.silva", "samuel.silva").is_err(), "a dot never makes a host name");
        assert!(check_name("samuel.silva", "samuel-silva-").is_err());
        assert!(check_name("samuel.silva", "samuel-silvas").is_err());
        assert!(check_name("bob", "alice").is_err());
        assert!(check_name("ana_paula", "ana-paula").is_ok(), "an underscore becomes a dash too");
        assert!(check_name("ana_paula", "ana_paula").is_err(), "still a DNS label");
        assert!(check_name("alice", "alice-a-name-much-too-long-here").is_err());
        // The host's own directories are where a workspace's would be.
        for kept in RESERVED {
            assert!(check_name(kept, kept).unwrap_err().contains("keeps for itself"), "{kept}");
        }
        assert!(check_name("ssh", "ssh-keys").is_ok());
    }

    #[test]
    fn ssh_keys() {
        assert_eq!(parse_ssh_keys(&format!("\n {KEY}  me@laptop \n\n")).unwrap(), [format!("{KEY} me@laptop")]);
        assert_eq!(parse_ssh_keys(KEY).unwrap(), [KEY]);
        assert!(parse_ssh_keys("ssh-dss AAAAB3NzaC1kc3MAAACBAP").is_err());
        assert!(parse_ssh_keys("ssh-ed25519").is_err());
        assert!(parse_ssh_keys("command=\"x\" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA").is_err(), "no options");
        assert!(parse_ssh_keys("ssh-ed25519 not*base64*at*all*!").is_err());
        assert!(parse_ssh_keys(&format!("{KEY}\n").repeat(MAX_SSH_KEYS + 1)).is_err());
        assert!(parse_ssh_keys("").unwrap().is_empty());
    }

    #[test]
    fn requests_within_the_limits() {
        let limits = Limits {
            os: vec!["ubuntu".into()],
            boot_mem_mib: 2048,
            max_memory: Ceiling::Size(16),
            host_memory_gib: 0,
            max_disk_gib: 128,
            quota: 1,
            apps: vec![AppInfo { id: "t3code".into(), name: "T3 Code".into(), ..AppInfo::default() }],
        };
        let ok = Request {
            name: "alice".into(),
            owner: "alice".into(),
            os: "ubuntu".into(),
            ssh_authorized_keys: vec![KEY.into()],
            max_memory: Ceiling::Size(8),
            disk_gib: 64,
            apps: vec!["t3code".into()],
            node: String::new(),
        };
        assert_eq!(ok.check(&limits), Ok(()));
        // The host's memory, only where the host's is the most.
        let auto = Request { max_memory: Ceiling::Auto, ..ok.clone() };
        assert!(auto.check(&limits).unwrap_err().contains("no auto"));
        let hosts = Limits { max_memory: Ceiling::Auto, host_memory_gib: 64, ..limits.clone() };
        assert_eq!((auto.check(&hosts), ok.check(&hosts), hosts.max_memory_gib()), (Ok(()), Ok(()), 64));
        assert!(Request { max_memory: Ceiling::Size(65), ..ok.clone() }.check(&hosts).is_err());
        assert_eq!(Request { apps: vec![], ..ok.clone() }.check(&limits), Ok(()), "no app at all");
        for bad in [
            Request { os: "nixos".into(), ..ok.clone() },
            Request { max_memory: Ceiling::Size(1), ..ok.clone() },
            Request { max_memory: Ceiling::Size(17), ..ok.clone() },
            Request { disk_gib: 8, ..ok.clone() },
            Request { disk_gib: 129, ..ok.clone() },
            Request { name: "bob".into(), ..ok.clone() },
            Request { ssh_authorized_keys: vec![format!("{KEY}\n{KEY}")], ..ok.clone() },
            Request { apps: vec!["herdr".into()], ..ok.clone() },
            Request { apps: vec!["t3code".into(), "t3code".into()], ..ok.clone() },
        ] {
            assert!(bad.check(&limits).is_err(), "{bad:?}");
        }
        let quotas = BTreeMap::from([("alice".to_owned(), 3)]);
        assert_eq!((quota_of(&quotas, 1, "alice"), quota_of(&quotas, 1, "bob")), (3, 1));
    }

    /// A field the Nix modules misspell is refused, not dropped.
    #[test]
    fn limits_and_apps_refuse_unknown_fields() {
        let limits =
            serde_json::json!({"os": ["ubuntu"], "bootMemMiB": 2048, "maxMemoryGiB": 16, "maxDiskGiB": 64, "quota": 1});
        assert!(serde_json::from_value::<Limits>(limits.clone()).is_ok());
        let mut typo = limits;
        typo["maxMemroyGiB"] = 8.into();
        assert!(serde_json::from_value::<Limits>(typo).is_err());
        let app = serde_json::json!({"id": "hello", "name": "Hello", "port": 8080, "bundle": "/nix/store/x", "oneshot": true});
        assert!(serde_json::from_value::<crate::apps::BootApp>(app.clone()).is_ok());
        let mut typo = app;
        typo["oneshott"] = true.into();
        assert!(serde_json::from_value::<crate::apps::BootApp>(typo).is_err());
    }
}
