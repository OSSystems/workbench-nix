//! The front's registry: a record of every workspace, which says who owns
//! it and which node its disk is on. Files, no database:
//!
//! - `<declared>/<ws>.json`, which Nix renders: a declared workspace's
//!   record, where Nix means it to be;
//! - `<stateDir>/registry/<ws>.json`, for every workspace: the node its
//!   disk is on, its address there and a move under way, and for one made
//!   at run time the rest of its record.
//!
//! Each is written atomically, under the registry's lock, which every
//! command that changes it takes.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use vmkit::hostutil;
use workbench_model::fleet::Record;
use workbench_model::is_workspace_name;

use crate::fleet::FrontSettings;

/// The largest record read.
const MAX_RECORD: u64 = 1 << 20;

/// The registry's files.
#[derive(Debug, Clone)]
pub struct Registry {
    /// `<stateDir>/registry`.
    pub dir: PathBuf,
    /// Where Nix renders declared workspaces' records.
    pub declared: PathBuf,
}

impl Registry {
    #[must_use]
    pub fn new(state_root: &Path, declared: &Path) -> Self {
        Registry { dir: state_root.join("registry"), declared: declared.to_owned() }
    }

    #[must_use]
    pub fn of(settings: &FrontSettings) -> Self {
        Registry::new(&settings.state_root(), Path::new(&settings.declared))
    }

    /// One change at a time: a name and an address are never handed out
    /// twice. Its own lock, not self-service's: on a machine with both
    /// roles the node takes that one while the front asks it.
    pub fn lock(&self) -> anyhow::Result<nix::fcntl::Flock<std::fs::File>> {
        crate::self_service::lock_file(&self.dir.join(".lock"))
    }

    fn state_file(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.json"))
    }

    fn declared_file(&self, name: &str) -> PathBuf {
        self.declared.join(format!("{name}.json"))
    }

    /// A workspace's record: a declared one's from Nix, on the node its
    /// disk is on, at its address there, with its move; none for a name
    /// the registry doesn't have. A name both declared and made at run
    /// time is neither's.
    pub fn get(&self, name: &str) -> anyhow::Result<Option<Record>> {
        ensure!(is_workspace_name(name), "invalid workspace name {name:?}");
        let nix = read(&self.declared_file(name))?.map(|mut r| {
            r.declared = true;
            r
        });
        let state = read(&self.state_file(name))?;
        Ok(match (nix, state) {
            (Some(_), Some(s)) if !s.declared => {
                bail!("{name} is both declared in Nix and made at run time; remove one")
            }
            (Some(mut n), Some(s)) => {
                n.node = s.node;
                n.ip = s.ip;
                n.moving = s.moving;
                Some(n)
            }
            (n, s) => n.or(s),
        })
    }

    /// What Nix declares of a workspace, where it means it to be.
    pub fn nix(&self, name: &str) -> anyhow::Result<Option<Record>> {
        ensure!(is_workspace_name(name), "invalid workspace name {name:?}");
        Ok(read(&self.declared_file(name))?.map(|r| Record { declared: true, ..r }))
    }

    /// The registry's own file of a workspace: where its disk is.
    pub fn placed(&self, name: &str) -> anyhow::Result<Option<Record>> {
        ensure!(is_workspace_name(name), "invalid workspace name {name:?}");
        read(&self.state_file(name))
    }

    /// Every record, by name, and the files that don't load, whose names
    /// and addresses nobody can tell.
    #[must_use]
    pub fn all(&self) -> (Vec<Record>, Vec<String>) {
        let mut names = BTreeSet::new();
        for dir in [&self.declared, &self.dir] {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for e in entries.flatten() {
                if let Some(name) = e.file_name().to_str().and_then(|n| n.strip_suffix(".json"))
                    && is_workspace_name(name)
                {
                    names.insert(name.to_owned());
                }
            }
        }
        let (mut ok, mut bad) = (Vec::new(), Vec::new());
        for name in names {
            match self.get(&name) {
                Ok(Some(r)) => ok.push(r),
                Ok(None) => {}
                Err(e) => bad.push(format!("{name}: {e:#}")),
            }
        }
        (ok, bad)
    }

    /// Writes a record: for a declared workspace, only where its disk is
    /// and its move, the rest being Nix's.
    pub fn put(&self, r: &Record) -> anyhow::Result<()> {
        r.validate().map_err(anyhow::Error::msg)?;
        let kept = if r.declared {
            Record {
                name: r.name.clone(),
                node: r.node.clone(),
                ip: r.ip.clone(),
                declared: true,
                moving: r.moving.clone(),
                ..Record::default()
            }
        } else {
            r.clone()
        };
        std::fs::create_dir_all(&self.dir)?;
        hostutil::atomic_write(self.state_file(&r.name), &serde_json::to_vec_pretty(&kept)?, 0o644)?;
        Ok(())
    }

    /// Forgets the registry's record of a workspace; a declared one's
    /// record stays Nix's.
    pub fn remove(&self, name: &str) -> anyhow::Result<()> {
        ensure!(is_workspace_name(name), "invalid workspace name {name:?}");
        match std::fs::remove_file(self.state_file(name)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

fn read(path: &Path) -> anyhow::Result<Option<Record>> {
    let raw = match hostutil::read_bounded(path, MAX_RECORD) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        r => r.with_context(|| path.display().to_string())?,
    };
    let r: Record = serde_json::from_slice(&raw).with_context(|| path.display().to_string())?;
    r.validate().map_err(anyhow::Error::msg).with_context(|| path.display().to_string())?;
    let file = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    ensure!(r.name == file, "{} names workspace {:?}", path.display(), r.name);
    Ok(Some(r))
}

/// Every address the records hold, or a move holds for them.
#[must_use]
pub fn used_addresses(records: &[Record]) -> BTreeSet<String> {
    let mut used = BTreeSet::new();
    for r in records {
        used.insert(r.ip.clone());
        if let Some(m) = &r.moving {
            used.insert(m.ip.clone());
            used.insert(m.from_ip.clone());
        }
    }
    used
}

/// The first address of a node's run-time half no record holds.
pub fn free_address(records: &[Record], subnet: &str) -> anyhow::Result<String> {
    crate::self_service::free_ip(subnet, &used_addresses(records))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use workbench_model::fleet::{Move, Phase};
    use workbench_model::memory::Ceiling;

    pub(crate) fn record(name: &str, node: &str, ip: &str) -> Record {
        Record {
            name: name.into(),
            owner: "alice".into(),
            node: node.into(),
            ip: ip.into(),
            os: "ubuntu".into(),
            ssh_authorized_keys: vec!["ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMbJhk5H0h7O me@laptop".into()],
            max_memory: Ceiling::Size(8),
            disk_gib: 64,
            apps: vec!["t3code".into()],
            ..Record::default()
        }
    }

    fn registry(dir: &Path) -> Registry {
        Registry::new(&dir.join("state"), &dir.join("declared"))
    }

    #[test]
    fn records_and_pending() {
        let dir = tempfile::tempdir().unwrap();
        let reg = registry(dir.path());
        assert_eq!(reg.get("alice").unwrap(), None);
        let mut r = Record { pending: true, ..record("alice", "a", "10.232.0.128") };
        reg.put(&r).unwrap();
        assert_eq!(reg.get("alice").unwrap().unwrap(), r, "a pending record holds its name and address");
        r.pending = false;
        reg.put(&r).unwrap();
        assert!(!reg.get("alice").unwrap().unwrap().pending);
        let (all, bad) = reg.all();
        assert_eq!((all.len(), bad.len()), (1, 0));
        assert_eq!(free_address(&all, "10.232.0").unwrap(), "10.232.0.129");
        reg.remove("alice").unwrap();
        reg.remove("alice").unwrap();
        assert_eq!(reg.get("alice").unwrap(), None);
        assert!(reg.get("../etc/passwd").is_err());
        assert!(reg.put(&Record { node: "../b".into(), ..r }).is_err(), "an invalid record isn't written");
    }

    /// A declared workspace is Nix's, but for where its disk is and its
    /// move, which the registry keeps.
    #[test]
    fn declared_records() {
        let dir = tempfile::tempdir().unwrap();
        let reg = registry(dir.path());
        std::fs::create_dir_all(&reg.declared).unwrap();
        let nix = Record { secrets_dir: "/var/lib/workbench-secrets/bob".into(), ..record("bob", "b", "10.232.1.10") };
        std::fs::write(reg.declared.join("bob.json"), serde_json::to_vec(&nix).unwrap()).unwrap();
        let got = reg.get("bob").unwrap().unwrap();
        assert!(got.declared && got.node == "b" && got.secrets_dir == nix.secrets_dir);
        // Its disk is still on a, where it was before Nix moved it.
        let moving = Move {
            from: "a".into(),
            from_ip: "10.232.0.10".into(),
            to: "b".into(),
            ip: "10.232.1.10".into(),
            phase: Phase::Copy,
            ran: true,
        };
        reg.put(&Record { node: "a".into(), ip: "10.232.0.10".into(), moving: Some(moving.clone()), ..got }).unwrap();
        let kept: Record = serde_json::from_slice(&std::fs::read(reg.dir.join("bob.json")).unwrap()).unwrap();
        assert!(kept.owner.is_empty() && kept.secrets_dir.is_empty(), "the registry keeps only where it is: {kept:?}");
        let got = reg.get("bob").unwrap().unwrap();
        assert_eq!((got.node.as_str(), got.ip.as_str(), got.owner.as_str()), ("a", "10.232.0.10", "alice"));
        assert_eq!(got.moving, Some(moving));
        // A name made at run time too is neither's.
        let made = record("carol", "a", "10.232.0.130");
        reg.put(&made).unwrap();
        std::fs::write(reg.declared.join("carol.json"), serde_json::to_vec(&made).unwrap()).unwrap();
        assert!(reg.get("carol").unwrap_err().to_string().contains("both"));
        std::fs::write(reg.dir.join("dave.json"), b"{").unwrap();
        let (all, bad) = reg.all();
        assert_eq!(all.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["bob"]);
        assert_eq!(bad.len(), 2, "{bad:?}");
        assert_eq!(
            used_addresses(&all),
            ["10.232.0.10", "10.232.1.10"].into_iter().map(String::from).collect(),
            "a move holds both its addresses"
        );
    }

    #[test]
    fn a_record_is_named_after_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let reg = registry(dir.path());
        std::fs::create_dir_all(&reg.dir).unwrap();
        std::fs::write(reg.dir.join("eve.json"), serde_json::to_vec(&record("alice", "a", "10.232.0.128")).unwrap())
            .unwrap();
        assert!(reg.get("eve").is_err());
    }
}
