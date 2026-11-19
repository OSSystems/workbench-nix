//! The memory daemon: grows and shrinks running workspaces with vmkit's
//! memory policy and publishes what they hold. Workspaces are a first pool,
//! so yielding pools boot only into what they leave. It owns no VM:
//! restarting it changes nothing but the next decision.

use std::collections::HashMap;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, chown};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, bail};
use jiff::Timestamp;
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;
use vmkit::ch::{Client, VmInfo};
use vmkit::control::{self, Exchange, Reply};
use vmkit::hostutil;
use vmkit::memory::{self, Action, GIB, HostMemory, MIB, Status, VmMemory};

use crate::config::{self, Config};

/// How often memory is decided and the pool's status published.
const PASS_EVERY: Duration = Duration::from_secs(2);

pub struct Options {
    pub dirs: Vec<PathBuf>,
    pub status_file: PathBuf,
    pub behind: Vec<BehindPool>,
    pub reserve_percent: f64,
    pub reserve_min_gib: f64,
}

/// A pool that yields to workspaces: where it publishes its memory, and
/// what it keeps whatever workspaces want.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BehindPool {
    pub status: PathBuf,
    pub guaranteed: i64,
}

impl std::str::FromStr for BehindPool {
    type Err = String;

    /// `<status file>=<GiB>`.
    fn from_str(v: &str) -> Result<Self, String> {
        let bad = || format!("want <status file>=<GiB>, got {v:?}");
        let (path, gib) = v.rsplit_once('=').ok_or_else(bad)?;
        let gib: f64 = gib.parse().map_err(|_| bad())?;
        if path.is_empty() || !gib.is_finite() || gib < 0.0 {
            return Err(bad());
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let guaranteed = (gib * GIB as f64) as i64;
        Ok(BehindPool { status: path.into(), guaranteed })
    }
}

/// What workspaces leave for the pools behind them: each pool's guarantee
/// less what it holds.
fn owed(behind: &[BehindPool], now: Timestamp) -> i64 {
    behind.iter().map(|p| memory::owed(p.guaranteed, memory::read_status(&p.status, now).as_ref())).sum()
}

/// What a workspace may grow into: available memory less the host reserve,
/// what other workspaces were promised, and what the pools behind
/// workspaces are guaranteed but do not hold yet.
fn workspace_host_memory(
    total: i64,
    available: i64,
    percent: f64,
    min_gib: f64,
    pending: i64,
    owed: i64,
) -> HostMemory {
    HostMemory { available, reserve: memory::reserve(total, percent, min_gib) + owed, pending }
}

/// One running workspace.
struct Vm {
    c: Config,
    /// One start of the VM: its directory is made anew by every start.
    run: u64,
    mem: Arc<Mutex<VmState>>,
    /// Serves the guest's control port.
    listener: JoinHandle<()>,
}

impl Drop for Vm {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

struct VmState {
    mem: VmMemory,
    denied: bool,
}

fn lock(m: &Mutex<VmState>) -> std::sync::MutexGuard<'_, VmState> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Daemon {
    opts: Options,
    vms: HashMap<String, Vm>,
    /// When a workspace was last denied a plug.
    denied_at: Option<Timestamp>,
}

pub async fn run(opts: Options) -> anyhow::Result<()> {
    if opts.status_file.as_os_str().is_empty() {
        bail!("--memory-status is required");
    }
    let mut d = Daemon { opts, vms: HashMap::new(), denied_at: None };
    let mut tick = tokio::time::interval(PASS_EVERY);
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            _ = tick.tick() => d.pass(Timestamp::now()).await,
            _ = term.recv() => return Ok(()),
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
    }
}

/// The identity of a VM directory, which every start recreates.
fn run_id(dir: &Path) -> Option<u64> {
    std::fs::metadata(dir).ok().map(|m| m.ino())
}

fn socket_present(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.file_type().is_socket())
}

impl Daemon {
    /// Follows the configured workspaces whose VM is up: a new start gets
    /// fresh memory state and a fresh control socket. Returns what each
    /// VM's API said.
    async fn track(&mut self) -> HashMap<String, VmInfo> {
        let mut seen = HashMap::new();
        for name in config::configured(&self.opts.dirs) {
            let Ok(c) = config::load(&self.opts.dirs, &name) else { continue };
            let Some(run) = run_id(&c.vm_dir()) else { continue };
            let Ok(info) = Client::new(c.api_socket()).info().await else { continue };
            seen.insert(name.clone(), info);
            if self.vms.get(&name).is_some_and(|vm| vm.run == run && socket_present(&c.control_socket())) {
                continue;
            }
            self.vms.remove(&name);
            match listen(c, run) {
                Ok(vm) => {
                    eprintln!(
                        "managing workspace memory workspace={name} bootMiB={} maxMiB={}",
                        vm.c.boot_mem_mib, vm.c.max_mem_mib
                    );
                    self.vms.insert(name, vm);
                }
                Err(e) => eprintln!("listening for guest stats workspace={name} error={e:#}"),
            }
        }
        self.vms.retain(|name, _| seen.contains_key(name));
        seen
    }

    async fn pass(&mut self, now: Timestamp) {
        let infos = self.track().await;
        match hostutil::meminfo_kib() {
            Ok(mem) => {
                let field = |k: &str| mem.get(k).copied().unwrap_or(0) << 10;
                let (total, available) = (field("MemTotal"), field("MemAvailable"));
                let owed = owed(&self.opts.behind, now);
                for (name, vm) in &self.vms {
                    let Some(info) = infos.get(name) else { continue };
                    // What earlier VMs were just granted is pending too.
                    let host = workspace_host_memory(
                        total,
                        available,
                        self.opts.reserve_percent,
                        self.opts.reserve_min_gib,
                        self.pending(),
                        owed,
                    );
                    if control(name, vm, info, host, now).await {
                        self.denied_at = Some(now);
                    }
                }
            }
            Err(e) => eprintln!("reading host memory error={e}"),
        }
        if let Err(e) = memory::write_status(&self.opts.status_file, &self.status(now)) {
            eprintln!("publishing workspace memory error={e}");
        }
    }

    fn pending(&self) -> i64 {
        self.vms.values().map(|vm| lock(&vm.mem).mem.pending()).sum()
    }

    fn status(&self, now: Timestamp) -> Status {
        let mut s = Status { updated_at: now, committed_bytes: 0, pending_bytes: 0, denied_at: self.denied_at };
        for vm in self.vms.values() {
            let st = lock(&vm.mem);
            s.committed_bytes += st.mem.committed();
            s.pending_bytes += st.mem.pending();
        }
        s
    }
}

/// Decides and applies one VM's next memory change. Returns whether the
/// VM was denied a plug.
async fn control(name: &str, vm: &Vm, info: &VmInfo, host: HostMemory, now: Timestamp) -> bool {
    if info.memory_actual_size <= 0 {
        return false;
    }
    let (action, target, m, previous, first_denial) = {
        let mut st = lock(&vm.mem);
        st.mem.actual_bytes = info.memory_actual_size;
        let m = st.mem;
        let (action, target) = memory::decide(&m, host, now);
        let mut previous = None;
        let mut first_denial = false;
        match action {
            Action::Denied => {
                first_denial = !st.denied;
                st.denied = true;
            }
            Action::Plug | Action::Unplug => {
                previous = Some(st.mem.grant(m.actual_bytes, target, now));
                st.denied = false;
            }
            Action::Hold => {}
        }
        (action, target, m, previous, first_denial)
    };
    if first_denial {
        eprintln!("host full, workspace memory not grown workspace={name} actualGiB={:.1}", gib(m.actual_bytes));
    }
    let denied = action == Action::Denied;
    let Some(previous) = previous else { return denied };
    if let Err(e) = Client::new(vm.c.api_socket()).resize_memory(target).await {
        let mut st = lock(&vm.mem);
        let (stats, actual) = (st.mem.stats, st.mem.actual_bytes);
        st.mem = previous;
        st.mem.stats = stats;
        st.mem.actual_bytes = actual;
        drop(st);
        eprintln!("resizing workspace memory workspace={name} error={e}");
        return denied;
    }
    lock(&vm.mem).mem.resized_at = Some(Timestamp::now());
    let available = m.stats.map_or(0, |s| s.available());
    eprintln!(
        "workspace memory changed workspace={name} action={action} fromGiB={:.1} toGiB={:.1} guestAvailableGiB={:.1}",
        gib(m.actual_bytes),
        gib(target),
        gib(available)
    );
    denied
}

#[allow(clippy::cast_precision_loss)]
fn gib(bytes: i64) -> f64 {
    bytes as f64 / GIB as f64
}

/// Opens the host side of the guest's stats port: cloud-hypervisor forwards
/// the guest's connections to it.
fn listen(c: Config, run: u64) -> anyhow::Result<Vm> {
    let vm_user = hostutil::lookup_account(&c.user)?;
    let socket = c.control_socket();
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).with_context(|| socket.display().to_string())?;
    chown(&socket, Some(vm_user.uid), Some(vm_user.gid))?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    let mem =
        Arc::new(Mutex::new(VmState { mem: VmMemory::new(c.boot_mem_mib * MIB, c.max_mem_mib * MIB), denied: false }));
    let state = mem.clone();
    let listener = tokio::spawn(async move {
        let e = control::serve(listener, move |conn: UnixStream| {
            let state = state.clone();
            async move {
                let mut ex = Exchange::new(conn);
                let req = ex.read_request().await;
                let reply = answer_stats(req, &state, Timestamp::now());
                ex.reply(&reply).await;
            }
        })
        .await;
        eprintln!("guest stats stopped error={e}");
    });
    Ok(Vm { c, run, mem, listener })
}

/// Takes in a guest's report; workspaces send nothing else.
fn answer_stats(req: Result<control::Request, Reply>, state: &Mutex<VmState>, now: Timestamp) -> Reply {
    let req = match req {
        Ok(req) => req,
        Err(reply) => return reply,
    };
    if req.op != "stats" {
        return Reply::error("unknown op");
    }
    match req.stats(now) {
        Ok(s) => {
            lock(state).mem.observe(s);
            Reply::ok()
        }
        Err(reply) => reply,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noon() -> Timestamp {
        "2026-11-18T12:00:00Z".parse().unwrap()
    }

    /// Workspaces may not grow into the part of a yielding pool's
    /// guarantee that pool does not hold yet.
    #[test]
    fn workspaces_leave_pools_behind_their_guarantee() {
        let now = noon();
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("first.json"), dir.path().join("second.json"));
        let behind: Vec<BehindPool> =
            [format!("{}=8", first.display()), format!("{}=1.5", second.display())].map(|v| v.parse().unwrap()).into();
        for bad in ["no-guarantee", "=1", &format!("{}=-1", first.display()), &format!("{}=x", first.display())] {
            assert!(bad.parse::<BehindPool>().is_err(), "accepted {bad:?}");
        }
        // Neither pool has published: both guarantees are owed in full.
        assert_eq!(owed(&behind, now), 8 * GIB + 3 * GIB / 2, "idle pools");
        let status =
            |committed| Status { updated_at: now, committed_bytes: committed, pending_bytes: 0, denied_at: None };
        memory::write_status(&first, &status(5 * GIB)).unwrap();
        memory::write_status(&second, &status(30 * GIB)).unwrap();
        assert_eq!(owed(&behind, now), 3 * GIB, "one pool holding 5 of its 8 GiB, the other past its 1.5");
        // Reserve max(10% of 64 GiB, 2 GiB) = 6.4 GiB, plus what is owed.
        let h = workspace_host_memory(64 * GIB, 20 * GIB, 10.0, 2.0, GIB, owed(&behind, now));
        assert_eq!(h, HostMemory { available: 20 * GIB, reserve: 64 * GIB / 10 + 3 * GIB, pending: GIB });
    }

    #[test]
    fn stats_channel_records_only_stats() {
        let state = Mutex::new(VmState { mem: VmMemory::new(2 * GIB, 16 * GIB), denied: false });
        let at = "2026-11-17T12:00:00Z".parse().unwrap();
        let ask = |line: &str| answer_stats(control::Request::parse(line.as_bytes()), &state, at);
        let r = ask(r#"{"op":"reboot"}"#);
        assert!(!r.ok && !r.error.is_empty(), "a workspace was served an unknown op: {r:?}");
        let r = ask(r#"{"op":"stats","memAvailableKiB":5,"memTotalKiB":4,"psiSomeAvg10":0,"psiFullAvg10":0}"#);
        assert!(!r.error.is_empty(), "invalid stats accepted");
        let r =
            ask(r#"{"op":"stats","memAvailableKiB":524288,"memTotalKiB":2097152,"psiSomeAvg10":12,"psiFullAvg10":1}"#);
        assert!(r.ok, "stats rejected: {r:?}");
        let mem = lock(&state).mem;
        let stats = mem.stats.unwrap();
        assert!(stats.received_at == at && stats.pressured(), "recorded {stats:?}");
        let host = HostMemory { available: 40 * GIB, reserve: 6 * GIB, pending: 0 };
        assert_eq!(memory::decide(&mem, host, at + jiff::SignedDuration::from_secs(1)), (Action::Plug, 3 * GIB));
    }
}
