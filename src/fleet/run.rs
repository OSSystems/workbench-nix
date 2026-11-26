//! `workbench run <ws>` on a node: asks the front for what the start
//! needs, has the workspace's config, writes the shares with the secrets
//! from the answer in the runtime directory, a tmpfs, keeps the config it
//! started with, and becomes cloud-hypervisor. It fails while the front
//! can't be reached or refuses, and systemd starts it again.

use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use workbench_model::fleet::Record;

use super::NodeSettings;
use super::client::FrontClient;
use super::contract::StartAnswer;
use crate::config::{self, Config};
use crate::launch;
use crate::self_service::{self, Template};

/// The unit's main process on a node: it ends by becoming
/// cloud-hypervisor, and only returns on failure.
pub fn run(settings: &NodeSettings, name: &str) -> anyhow::Result<()> {
    // No runtime threads left behind once it is cloud-hypervisor.
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let front = FrontClient { endpoint: settings.front.clone() };
    let answer = rt.block_on(front.start(name)).map_err(|e| anyhow::anyhow!("the front: {e}"))?;
    drop(rt);
    let c = config_for(settings, name, &answer)?;
    launch::boot(&c, Some(&answer.given()))
}

/// The config a workspace starts with: a declared one's is Nix's, and one
/// made at run time's is the node's template with the record the front
/// gave, which the node keeps.
pub fn config_for(settings: &NodeSettings, name: &str, answer: &StartAnswer) -> anyhow::Result<Config> {
    let dirs = settings.config_dirs();
    let Some(r) = &answer.record else {
        let c = config::load(&dirs, name)?;
        ensure!(!c.self_service, "the front has {name} declared, and this node made it at run time");
        return Ok(c);
    };
    ensure!(r.name == name, "the front answered for {:?}", r.name);
    let t = Template::load(Path::new(&settings.template))?;
    kept_config(&t, &dirs, r)
}

/// Renders a record on the config the node kept of it, and keeps the new
/// one, for `workbench` on the node.
pub fn kept_config(t: &Template, dirs: &[PathBuf], r: &Record) -> anyhow::Result<Config> {
    let paths = t.paths();
    // Also refuses a name both declared here and made at run time.
    let kept = config::load(dirs, &r.name)?;
    ensure!(kept.self_service, "{} is declared on this node, and the front made it at run time", r.name);
    let c = self_service::render_kept(t, r, &kept).with_context(|| format!("{}'s config", r.name))?;
    if c != kept {
        self_service::write_config(&paths, &c)?;
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::registry::tests::record;
    use crate::self_service::tests::template;
    use workbench_model::memory::Ceiling;

    /// A start renders the front's record on the node's own: the record's
    /// apps and memory, the node's VM user, address and image, which a
    /// node keeps for the next.
    #[test]
    fn a_start_renders_the_record_on_what_the_node_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = template();
        t.state_dir = dir.path().display().to_string();
        t.apps.push(workbench_model::apps::BootApp { id: "herdr".into(), port: None, ..t.apps[0].clone() });
        let r = Record {
            name: "samuel-silva-data".into(),
            owner: "samuel.silva".into(),
            ..record("x", "a", "10.232.0.130")
        };
        let made = self_service::render(&t, &r, 60_600).unwrap();
        self_service::write_config(&t.paths(), &made).unwrap();
        let dirs = [dir.path().join("etc"), t.paths().configs()];
        assert_eq!(kept_config(&t, &dirs, &r).unwrap(), made, "nothing changed");
        let more = Record { apps: vec!["t3code".into(), "herdr".into()], max_memory: Ceiling::Size(16), ..r.clone() };
        let c = kept_config(&t, &dirs, &more).unwrap();
        assert_eq!(c.apps.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["t3code", "herdr"]);
        assert_eq!((c.max_mem, c.uid, c.ip.as_str()), (Ceiling::Size(16_384), 60_600, "10.232.0.130"));
        assert_eq!(config::load(&dirs, &r.name).unwrap(), c, "the node keeps the config it started with");
        let elsewhere = Record { ip: "10.232.0.131".into(), ..r.clone() };
        assert!(kept_config(&t, &dirs, &elsewhere).is_err(), "an address the node didn't make it at");
    }
}
