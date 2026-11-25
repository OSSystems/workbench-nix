//! What people do with workspaces of their own: the New workspace page,
//! creating, changing their apps, destroying, and the admins' quotas and memory. The UI has no privilege:
//! it writes the request, then starts the unit that makes or destroys the
//! workspace as root, and reads how it went.

use std::collections::BTreeMap;

use axum::http::StatusCode;
use leptos::prelude::ServerFnError;
use vmkit::hostutil;
use workbench::self_service::read_quotas;
use workbench_model::self_service::{
    AppsRequest, MemoryRequest, NewView, Outcome, QuotaRow, Request, parse_ssh_keys, quota_of, slug,
};
use workbench_model::{Quotas, State, Verb, Viewer, Workspace, is_user_name};

use super::{AppState, SelfService, encode, refuse, see_other, status};

/// The most a quota may be.
const MAX_QUOTA: u32 = 100;
/// The most of the pasted keys, encoded, a refused form gets back.
const MAX_KEYS_BACK: usize = 2048;

/// What the New workspace form posts.
#[derive(Debug, Clone, Default)]
pub struct CreateForm {
    pub name: String,
    pub os: String,
    pub ssh_keys: String,
    pub memory_gib: String,
    pub disk_gib: String,
    pub apps: Vec<String>,
}

impl CreateForm {
    /// The form again, after a refusal. The keys come back only while the
    /// address stays well within what nginx takes in a header.
    fn back(&self, error: &str) -> String {
        let keys = encode(&self.ssh_keys);
        let keys = if keys.len() <= MAX_KEYS_BACK { keys } else { String::new() };
        let apps: Vec<&str> = self.apps.iter().take(16).map(String::as_str).collect();
        format!(
            "/new?error={}&name={}&os={}&memory={}&disk={}&apps={}&keys={keys}",
            encode(error),
            encode(&self.name.chars().take(64).collect::<String>()),
            encode(&self.os.chars().take(16).collect::<String>()),
            encode(&self.memory_gib.chars().take(8).collect::<String>()),
            encode(&self.disk_gib.chars().take(8).collect::<String>()),
            encode(&apps.join(",").chars().take(256).collect::<String>()),
        )
    }
}

/// How many workspaces made at run time `user` owns.
fn owned(all: &[Workspace], user: &str) -> u32 {
    u32::try_from(all.iter().filter(|w| w.configured && w.self_service && w.owner == user).count()).unwrap_or(u32::MAX)
}

impl AppState {
    fn self_service(&self) -> Result<&SelfService, ServerFnError> {
        self.opts
            .self_service
            .as_ref()
            .ok_or_else(|| refuse("this host doesn't let people make workspaces", StatusCode::NOT_FOUND))
    }

    /// What the New workspace page shows `viewer`.
    pub async fn new_view(&self, viewer: &Viewer) -> Result<NewView, ServerFnError> {
        let ss = self.self_service()?;
        if !viewer.creates {
            return Err(refuse("you may not make workspaces here", StatusCode::FORBIDDEN));
        }
        let all = self.all().await.map_err(super::failed)?;
        let quotas = read_quotas(&ss.paths.quotas()).map_err(super::failed)?;
        Ok(NewView {
            limits: ss.limits.clone(),
            slug: slug(&viewer.name),
            owned: owned(&all, &viewer.name),
            quota: quota_of(&quotas, ss.limits.quota, &viewer.name),
        })
    }

    /// The `create` server function: checks the form, writes the request
    /// and starts the unit that makes the workspace; then the list with it
    /// selected, or the form with why not.
    pub async fn create_action(&self, viewer: &Viewer, form: CreateForm) -> Result<(), ServerFnError> {
        let ss = self.self_service()?;
        if !viewer.creates {
            return Err(refuse("you may not make workspaces here", StatusCode::FORBIDDEN));
        }
        // The status first: a form's redirect replaces it with a 303.
        let refused = |msg: &str| {
            status(StatusCode::UNPROCESSABLE_ENTITY);
            see_other(&form.back(msg));
            Err(ServerFnError::new(msg))
        };
        let keys = match parse_ssh_keys(&form.ssh_keys) {
            Ok(k) => k,
            Err(e) => return refused(&e),
        };
        let (Ok(memory), Ok(disk)) = (form.memory_gib.trim().parse(), form.disk_gib.trim().parse()) else {
            return refused("Memory and disk must be whole numbers of GiB.");
        };
        let r = Request {
            name: form.name.trim().to_owned(),
            owner: viewer.name.clone(),
            os: form.os.clone(),
            ssh_authorized_keys: keys,
            max_memory_gib: memory,
            disk_gib: disk,
            apps: form.apps.clone(),
        };
        if let Err(e) = r.check(&ss.limits) {
            return refused(&e);
        }
        let all = self.all().await.map_err(super::failed)?;
        if all.iter().any(|w| w.name == r.name) {
            return refused(&format!("{} is taken: pick another name.", r.name));
        }
        let quotas = read_quotas(&ss.paths.quotas()).map_err(super::failed)?;
        let quota = quota_of(&quotas, ss.limits.quota, &viewer.name);
        if owned(&all, &viewer.name) >= quota {
            return refused(&format!(
                "You have all the workspaces your quota allows ({quota}). Ask an admin for more."
            ));
        }
        let Some(_making) = Making::start(self, &r.name) else {
            return refused(&format!("{} is being made just now: pick another name.", r.name));
        };
        let request = ss.paths.request(&r.name);
        let outcome = ss.paths.outcome(&r.name);
        let written = (|| {
            std::fs::create_dir_all(request.parent().unwrap_or(&ss.paths.dir()))?;
            hostutil::atomic_write(&request, &serde_json::to_vec(&r)?, 0o640)?;
            anyhow::Ok(())
        })();
        if let Err(e) = written {
            return refused(&format!("The host couldn't take the request: {e:#}"));
        }
        let ran = self.backend.run_unit(&format!("workbench-create@{}.service", r.name)).await;
        self.forget().await;
        match read_outcome(&outcome) {
            Some(o) if o.ok => {
                see_other(&format!("/?w={}", r.name));
                Ok(())
            }
            Some(o) => refused(&o.error),
            None => refused(&ran.err().map_or_else(
                || "The host said nothing of it.".to_owned(),
                |e| format!("The host hasn't finished it, or couldn't: {e:#}. If it shows up in the list, it's yours."),
            )),
        }
    }

    /// The `apps` server function: gives a workspace made at run time the
    /// apps picked, through the unit that changes its config as root, and
    /// with `restart` restarts it into them. Then the list with it
    /// selected, or the apps page with why not.
    pub async fn apps_action(
        &self,
        viewer: &Viewer,
        name: &str,
        apps: Vec<String>,
        restart: bool,
    ) -> Result<(), ServerFnError> {
        let ss = self.self_service()?;
        let ws = self.find(viewer, name).await?;
        if !ws.self_service {
            return Err(refuse("only the Nix configuration changes this workspace's apps", StatusCode::FORBIDDEN));
        }
        let back = format!("/w/{name}/apps");
        let refused = |msg: &str, code: StatusCode| {
            status(code);
            see_other(&format!("{back}?error={}", encode(msg)));
            Err(ServerFnError::new(msg))
        };
        if let Err(e) = ss.limits.check_apps(&apps) {
            return refused(&e, StatusCode::UNPROCESSABLE_ENTITY);
        }
        let change = Change {
            what: "apps",
            busy: format!("{name}'s apps are being changed just now: try again."),
            request: ss.paths.apps_request(name),
            outcome: ss.paths.apps_outcome(name),
            unit: format!("workbench-apps@{name}.service"),
        };
        match self.change(ss, &ws, &change, &AppsRequest { apps }, restart).await {
            Ok(()) => {
                see_other(&format!("/?w={name}"));
                Ok(())
            }
            Err((msg, code)) => refused(&msg, code),
        }
    }

    /// The `memory` server function, for admins: gives a workspace made at
    /// run time the most memory it grows to, through the unit that changes
    /// its config as root, and with `restart` restarts it into it. Then
    /// the list with it selected, or the memory page with why not.
    pub async fn memory_action(
        &self,
        viewer: &Viewer,
        name: &str,
        memory_gib: &str,
        restart: bool,
    ) -> Result<(), ServerFnError> {
        let ss = self.self_service()?;
        if !viewer.admin {
            return Err(refuse("a workspace's memory is for admins", StatusCode::FORBIDDEN));
        }
        let ws = self.find(viewer, name).await?;
        if !ws.self_service {
            return Err(refuse("only the Nix configuration changes this workspace's memory", StatusCode::FORBIDDEN));
        }
        let refused = |msg: &str, code: StatusCode| {
            status(code);
            see_other(&format!("/w/{name}/memory?error={}", encode(msg)));
            Err(ServerFnError::new(msg))
        };
        let Ok(gib) = memory_gib.trim().parse::<i64>() else {
            return refused("Memory must be a whole number of GiB.", StatusCode::UNPROCESSABLE_ENTITY);
        };
        if let Err(e) = ss.limits.check_memory(gib) {
            return refused(&e, StatusCode::UNPROCESSABLE_ENTITY);
        }
        let change = Change {
            what: "memory",
            busy: format!("{name}'s memory is being changed just now: try again."),
            request: ss.paths.memory_request(name),
            outcome: ss.paths.memory_outcome(name),
            unit: format!("workbench-memory@{name}.service"),
        };
        match self.change(ss, &ws, &change, &MemoryRequest { max_memory_gib: gib }, restart).await {
            Ok(()) => {
                see_other(&format!("/?w={name}"));
                Ok(())
            }
            Err((msg, code)) => refused(&msg, code),
        }
    }

    /// Writes a change's request, runs its unit and reads how it went;
    /// with `restart`, restarts a running workspace into it. Why not, and
    /// the status that says it, otherwise.
    async fn change(
        &self,
        ss: &SelfService,
        ws: &Workspace,
        change: &Change,
        request: &impl serde::Serialize,
        restart: bool,
    ) -> Result<(), (String, StatusCode)> {
        let name = &ws.name;
        // Another request would join this one's run, which takes only one.
        // Held until the unit is done, even if the browser goes away.
        let Some(changing) = Making::start(self, &format!("{name}/{}", change.what)) else {
            return Err((change.busy.clone(), StatusCode::CONFLICT));
        };
        let written = (|| {
            std::fs::create_dir_all(change.request.parent().unwrap_or(&ss.paths.dir()))?;
            hostutil::atomic_write(&change.request, &serde_json::to_vec(request)?, 0o640)?;
            anyhow::Ok(())
        })();
        if let Err(e) = written {
            return Err((format!("The host couldn't take the request: {e:#}"), StatusCode::BAD_GATEWAY));
        }
        // On the clock the outcome is stamped with.
        let asked = std::fs::metadata(&change.request).and_then(|m| m.modified()).ok();
        let run = {
            let (state, unit) = (self.clone(), change.unit.clone());
            tokio::spawn(async move {
                let _changing = changing;
                state.backend.run_unit(&unit).await
            })
        };
        let ran = run.await.unwrap_or_else(|e| Err(anyhow::anyhow!(e)));
        self.forget().await;
        // The unit writes its outcome anew; one from before this request
        // says nothing of it.
        match (ran, asked.and_then(|at| read_outcome_since(&change.outcome, at))) {
            (_, Some(o)) if !o.ok => return Err((o.error, StatusCode::UNPROCESSABLE_ENTITY)),
            (Ok(()), Some(_)) => {}
            (Err(e), _) => return Err((format!("{e:#}"), StatusCode::BAD_GATEWAY)),
            (Ok(()), None) => return Err(("The host said nothing of it.".into(), StatusCode::BAD_GATEWAY)),
        }
        // A stopped VM boots with it anyway; one on its way somewhere is
        // left alone.
        if restart && !ws.state.changing() && ws.state != State::Stopped {
            if let Err(e) = self.backend.control(Verb::Restart, name).await {
                return Err((format!("Saved, but the restart failed: {e:#}"), StatusCode::BAD_GATEWAY));
            }
            self.restarted(name).await;
        }
        Ok(())
    }

    /// The `destroy` server function: a workspace made at run time, by its
    /// owner or an admin, once its name is typed again.
    pub async fn destroy_action(&self, viewer: &Viewer, name: &str, confirm: &str) -> Result<(), ServerFnError> {
        let ss = self.self_service()?;
        let ws = self.find(viewer, name).await?;
        let back = format!("/w/{name}/destroy");
        if !ws.self_service {
            return Err(refuse("only the Nix configuration removes this workspace", StatusCode::FORBIDDEN));
        }
        if confirm.trim() != name {
            status(StatusCode::UNPROCESSABLE_ENTITY);
            see_other(&format!("{back}?error=confirm"));
            return Err(ServerFnError::new("type the workspace's name to destroy it"));
        }
        let outcome = ss.paths.outcome(name);
        let ran = self.backend.run_unit(&format!("workbench-destroy@{name}.service")).await;
        self.forget().await;
        if let Err(e) = ran {
            let detail = read_outcome(&outcome).map_or_else(|| format!("{e:#}"), |o| o.error);
            status(StatusCode::BAD_GATEWAY);
            see_other(&format!("{back}?error=failed&detail={}", encode(&detail)));
            return Err(ServerFnError::new(detail));
        }
        see_other("/");
        Ok(())
    }

    /// The `quota` server function, for admins: sets a person's quota.
    pub fn quota_action(&self, viewer: &Viewer, user: &str, quota: &str) -> Result<(), ServerFnError> {
        let ss = self.self_service()?;
        if !viewer.admin {
            return Err(refuse("quotas are for admins", StatusCode::FORBIDDEN));
        }
        let user = user.trim();
        let Ok(quota) = quota.trim().parse::<u32>() else {
            return Err(refuse("a quota is a whole number", StatusCode::UNPROCESSABLE_ENTITY));
        };
        if !is_user_name(user) || quota > MAX_QUOTA {
            return Err(refuse(
                &format!("a uid, and a quota of at most {MAX_QUOTA}"),
                StatusCode::UNPROCESSABLE_ENTITY,
            ));
        }
        let path = ss.paths.quotas();
        let mut quotas = read_quotas(&path).map_err(super::failed)?;
        quotas.insert(user.to_owned(), quota);
        let raw = serde_json::to_vec_pretty(&quotas).map_err(|e| super::failed(e.into()))?;
        std::fs::create_dir_all(path.parent().unwrap_or(&ss.paths.dir())).map_err(|e| super::failed(e.into()))?;
        hostutil::atomic_write(&path, &raw, 0o644).map_err(|e| super::failed(e.into()))?;
        see_other("/host");
        Ok(())
    }

    /// The host page's quotas: everyone who owns a workspace made at run
    /// time or has a quota of their own.
    pub(crate) fn quotas(&self, all: &[Workspace]) -> anyhow::Result<Option<Quotas>> {
        let Some(ss) = &self.opts.self_service else { return Ok(None) };
        let set = read_quotas(&ss.paths.quotas())?;
        let mut people: BTreeMap<String, QuotaRow> = BTreeMap::new();
        for w in all.iter().filter(|w| w.configured && w.self_service) {
            people.entry(w.owner.clone()).or_default().owned += 1;
        }
        for user in set.keys() {
            people.entry(user.clone()).or_default();
        }
        let people = people
            .into_iter()
            .map(|(user, row)| QuotaRow {
                quota: quota_of(&set, ss.limits.quota, &user),
                own: set.contains_key(&user),
                owned: row.owned,
                user,
            })
            .collect();
        Ok(Some(Quotas { default: ss.limits.quota, people }))
    }
}

fn read_outcome(path: &std::path::Path) -> Option<Outcome> {
    let raw = hostutil::read_bounded(path, 1 << 16).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// An outcome written at `since` or after.
fn read_outcome_since(path: &std::path::Path, since: std::time::SystemTime) -> Option<Outcome> {
    let at = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    (at >= since).then(|| read_outcome(path)).flatten()
}

/// A change to a workspace made at run time: `what` changes, its request
/// and outcome files, and the unit that makes it as root.
struct Change {
    what: &'static str,
    busy: String,
    request: std::path::PathBuf,
    outcome: std::path::PathBuf,
    unit: String,
}

/// A name being made, or `<name>/<what>` a change of it, until the request
/// is answered.
struct Making(AppState, String);

impl Making {
    fn start(state: &AppState, name: &str) -> Option<Self> {
        super::lock(&state.making).insert(name.to_owned()).then(|| Making(state.clone(), name.to_owned()))
    }
}

impl Drop for Making {
    fn drop(&mut self) {
        super::lock(&self.0.making).remove(&self.1);
    }
}
