//! Apps: what runs in a workspace and is opened from a browser, each at an
//! address of its own. A workspace has none, one or several, and none is
//! its main one.
//!
//! A VM keeps what it booted with for as long as it runs. The host's config
//! says what the next start boots, a [`Boot`]; the record the last start
//! left says what the VM runs, another. Their difference, [`changes`], is
//! everything that waits for a restart, and the only place that is decided:
//! an app added or removed, a newer version, newer packages. What the UI
//! shows of an app, its [`Stage`] and whether it opens, follows from it.

use serde::{Deserialize, Serialize};

/// The longest app id.
const MAX_ID: usize = 16;

/// Whether `id` names an app: a lowercase letter, then lowercase letters
/// and digits, at most 16. No dash, so `<workspace>--<app>` reads one way.
#[must_use]
pub fn is_app_id(id: &str) -> bool {
    let b = id.as_bytes();
    (1..=MAX_ID).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// An app's address: the workspace's, with `--<id>` after its name, so
/// `https://alice.ws.example` and `t3code` give
/// `https://alice--t3code.ws.example`. Every app is an origin of its own.
#[must_use]
pub fn app_url(ws_url: &str, id: &str) -> String {
    let (scheme, rest) = ws_url.split_once("://").unwrap_or(("https", ws_url));
    match rest.split_once('.') {
        Some((name, domain)) => format!("{scheme}://{name}--{id}.{domain}"),
        None => format!("{scheme}://{rest}--{id}"),
    }
}

/// An app as the host offers it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInfo {
    pub id: String,
    /// What people call it.
    pub name: String,
}

/// What a VM boots: the workspace's packages, which its agent comes with,
/// and its apps, each at a version.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Boot {
    /// The packages' bundle.
    pub packages: String,
    pub apps: Vec<BootApp>,
}

/// One app a VM boots.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootApp {
    pub id: String,
    pub name: String,
    /// The port it listens on in the guest.
    pub port: u16,
    /// Its bundle: a new one is a new version.
    pub bundle: String,
}

impl Boot {
    #[must_use]
    pub fn app(&self, id: &str) -> Option<&BootApp> {
        self.apps.iter().find(|a| a.id == id)
    }
}

/// One thing the next start changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Change {
    /// The app comes with it.
    AppAdded { app: String },
    /// The app goes.
    AppRemoved { app: String },
    /// A newer version of the app comes.
    AppUpdated { app: String },
    /// Newer packages come. A newer agent, the host's, comes with every
    /// start too, but changes nothing anyone sees.
    Packages,
}

impl Change {
    /// The app it is about, if any.
    #[must_use]
    pub fn app(&self) -> Option<&str> {
        match self {
            Change::AppAdded { app } | Change::AppRemoved { app } | Change::AppUpdated { app } => Some(app),
            Change::Packages => None,
        }
    }
}

/// What starting `next` changes in a VM that runs `running`: the apps of
/// `next` that come or are updated, in its order, then those that go, then
/// the packages. Nothing when no VM runs: the next start boots `next`
/// whatever it is.
#[must_use]
pub fn changes(running: Option<&Boot>, next: &Boot) -> Vec<Change> {
    let Some(running) = running else { return vec![] };
    let mut out = Vec::new();
    for app in &next.apps {
        match running.app(&app.id) {
            None => out.push(Change::AppAdded { app: app.id.clone() }),
            Some(r) if r.bundle != app.bundle || r.port != app.port => {
                out.push(Change::AppUpdated { app: app.id.clone() });
            }
            Some(_) => {}
        }
    }
    for app in &running.apps {
        if next.app(&app.id).is_none() {
            out.push(Change::AppRemoved { app: app.id.clone() });
        }
    }
    if running.packages != next.packages {
        out.push(Change::Packages);
    }
    out
}

/// Where an app stands between the VM that runs and the next start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// The next start keeps it as it is; so is every app of a VM that
    /// doesn't run.
    Ready,
    /// The next start brings it; the VM that runs doesn't have it.
    Coming,
    /// It runs until the next start, which leaves it out.
    Going,
    /// It runs, and the next start brings a newer version.
    Updating,
}

impl Stage {
    /// The stage `changes` give the app `id`.
    #[must_use]
    pub fn of(id: &str, changes: &[Change]) -> Self {
        changes
            .iter()
            .find_map(|c| match c {
                Change::AppAdded { app } if app == id => Some(Stage::Coming),
                Change::AppRemoved { app } if app == id => Some(Stage::Going),
                Change::AppUpdated { app } if app == id => Some(Stage::Updating),
                _ => None,
            })
            .unwrap_or(Stage::Ready)
    }

    /// Whether the VM that runs has the app.
    #[must_use]
    pub fn runs(self) -> bool {
        self != Stage::Coming
    }
}

/// One app of a workspace, as people see it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct App {
    pub id: String,
    pub name: String,
    /// Where it is opened.
    pub url: String,
    /// The guest makes links that sign a browser in to it; known while
    /// the agent answers.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub open_link: bool,
}

/// Why an app of a workspace doesn't open now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OpenBlocker {
    /// The workspace has no such app.
    Unknown,
    Stopped,
    /// The VM, or its agent, isn't up yet.
    Starting,
    /// It comes with the next start.
    Coming,
}

/// A one-time link to open an app on another device, and its QR code: one
/// SVG path of unit squares on a grid of `size`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceLink {
    pub link: String,
    pub size: u32,
    pub qr: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, bundle: &str) -> BootApp {
        BootApp { id: id.into(), name: id.to_uppercase(), port: 3773, bundle: bundle.into() }
    }

    fn boot(packages: &str, apps: &[BootApp]) -> Boot {
        Boot { packages: packages.into(), apps: apps.to_vec() }
    }

    #[test]
    fn ids_and_addresses() {
        for ok in ["t3code", "a", "herdr2", &"a".repeat(16)] {
            assert!(is_app_id(ok), "{ok}");
        }
        for bad in ["", "T3", "t3-code", "2fa", "a.b", &"a".repeat(17)] {
            assert!(!is_app_id(bad), "{bad}");
        }
        assert_eq!(app_url("https://alice.ws.test", "t3code"), "https://alice--t3code.ws.test");
        assert_eq!(app_url("http://alice-dev.wb.example.com", "x"), "http://alice-dev--x.wb.example.com");
    }

    #[test]
    fn what_the_next_start_changes() {
        let t3 = app("t3code", "/nix/store/a-t3code");
        let next = boot("/nix/store/p1", std::slice::from_ref(&t3));
        assert!(changes(None, &next).is_empty(), "no VM runs: nothing waits");
        assert!(changes(Some(&next), &next).is_empty(), "the VM runs what the config says");
        let bare = boot("/nix/store/p1", &[]);
        assert_eq!(changes(Some(&bare), &next), [Change::AppAdded { app: "t3code".into() }]);
        assert_eq!(changes(Some(&next), &bare), [Change::AppRemoved { app: "t3code".into() }]);
        let newer = boot("/nix/store/p2", &[app("t3code", "/nix/store/b-t3code"), app("herdr", "/nix/store/h")]);
        assert_eq!(
            changes(Some(&next), &newer),
            [Change::AppUpdated { app: "t3code".into() }, Change::AppAdded { app: "herdr".into() }, Change::Packages]
        );
        let moved = boot("/nix/store/p1", &[BootApp { port: 8080, ..t3 }]);
        assert_eq!(changes(Some(&next), &moved), [Change::AppUpdated { app: "t3code".into() }], "a new port too");
    }

    #[test]
    fn stages() {
        let c = [
            Change::AppAdded { app: "a".into() },
            Change::AppRemoved { app: "r".into() },
            Change::AppUpdated { app: "u".into() },
            Change::Packages,
        ];
        assert_eq!(
            ["a", "r", "u", "kept"].map(|id| Stage::of(id, &c)),
            [Stage::Coming, Stage::Going, Stage::Updating, Stage::Ready]
        );
        assert!(!Stage::Coming.runs() && Stage::Going.runs());
    }

    /// Changes are in the status JSON the UI and `workbench status --json`
    /// give.
    #[test]
    fn change_json() {
        let c = [Change::AppAdded { app: "t3code".into() }, Change::Packages];
        assert_eq!(serde_json::to_string(&c).unwrap(), r#"[{"kind":"appAdded","app":"t3code"},{"kind":"packages"}]"#);
    }
}
