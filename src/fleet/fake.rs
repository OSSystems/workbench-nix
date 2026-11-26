//! A node for the front's tests: workspaces in memory, disks in files,
//! and failures on demand.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use tokio::sync::mpsc;
use workbench_model::apps::Boot;
use workbench_model::fleet::{HostInfo, MakeRequest, NodeWorkspace, OpenLinkAnswer, Record};
use workbench_model::{State, Verb, Workspace};

use super::client::{BoxFuture, CallError, DiskOut, Node};
use super::front::Fleet;
use super::{VERSION, disk};

const GIB: i64 = 1 << 30;

/// One workspace on a fake node.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct FakeWs {
    pub record: Record,
    pub state: State,
    pub agent_up: bool,
    pub stopped: bool,
    pub for_move: bool,
    /// It has a config here: a start of one Nix moved away answers 404.
    pub startable: bool,
}

pub struct FakeState {
    pub host: HostInfo,
    pub workspaces: BTreeMap<String, FakeWs>,
    /// What it was asked, in order: `make alice`, `stop alice`, …
    pub calls: Vec<String>,
    /// What it fails: `make`, `start`, `stop`, `delete`, `disk_out`,
    /// `disk_in`, `workspaces`; `corrupt` flips a byte of the disks it
    /// sends.
    pub fail: BTreeSet<&'static str>,
    pub unreachable: bool,
    /// Its agents answer once started.
    pub agents_answer: bool,
}

pub struct FakeNode {
    pub name: String,
    /// Its disks, `<ws>.raw`.
    pub dir: PathBuf,
    pub state: Mutex<FakeState>,
    /// The front's registry, to note a start while the registry says the
    /// workspace moves.
    pub watch: Mutex<Option<super::registry::Registry>>,
}

impl FakeNode {
    pub fn new(name: &str, dir: PathBuf, available_gib: i64) -> Arc<Self> {
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(FakeNode {
            name: name.into(),
            dir,
            watch: Mutex::new(None),
            state: Mutex::new(FakeState {
                host: HostInfo {
                    version: VERSION.into(),
                    mem_total: 64 * GIB,
                    mem_available: available_gib * GIB,
                    reserve: 4 * GIB,
                    disk_total: 1 << 40,
                    disk_free: 1 << 40,
                    ..HostInfo::default()
                },
                workspaces: BTreeMap::new(),
                calls: Vec::new(),
                fail: BTreeSet::new(),
                unreachable: false,
                agents_answer: true,
            }),
        })
    }

    pub fn disk(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.raw"))
    }

    /// A workspace it has, running or not, with a disk of `data`.
    pub fn has(&self, r: &Record, running: bool, data: &[u8]) {
        std::fs::write(self.disk(&r.name), data).unwrap();
        let state = if running { State::Running } else { State::Stopped };
        let ws =
            FakeWs { record: r.clone(), state, agent_up: running, stopped: !running, for_move: false, startable: true };
        self.state.lock().unwrap().workspaces.insert(r.name.clone(), ws);
    }

    pub fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn ws(&self, name: &str) -> Option<FakeWs> {
        self.state.lock().unwrap().workspaces.get(name).cloned()
    }

    pub fn fail(&self, what: &'static str) {
        self.state.lock().unwrap().fail.insert(what);
    }

    fn call(&self, what: &str, name: &str, op: &'static str) -> Result<(), CallError> {
        let mut s = self.state.lock().unwrap();
        if s.unreachable {
            return Err(CallError::Unreachable("down".into()));
        }
        s.calls.push(format!("{what} {name}").trim().to_owned());
        if s.fail.contains(op) {
            return Err(CallError::Refused { status: 500, error: format!("{op} failed") });
        }
        Ok(())
    }
}

/// A fleet of these nodes, as the front has them.
pub fn fleet(settings: super::FrontSettings, nodes: &[&Arc<FakeNode>]) -> Fleet {
    let nodes: BTreeMap<String, Arc<dyn Node>> =
        nodes.iter().map(|n| (n.name.clone(), Arc::clone(n) as Arc<dyn Node>)).collect();
    let mut f = Fleet::with_nodes(settings, nodes);
    f.start_wait = std::time::Duration::from_millis(50);
    f.copy_idle = std::time::Duration::from_millis(200);
    f
}

impl Node for FakeNode {
    fn host(&self) -> BoxFuture<'_, Result<HostInfo, CallError>> {
        Box::pin(async move {
            self.call("host", "", "host")?;
            Ok(self.state.lock().unwrap().host.clone())
        })
    }

    fn workspaces(&self) -> BoxFuture<'_, Result<Vec<NodeWorkspace>, CallError>> {
        Box::pin(async move {
            self.call("workspaces", "", "workspaces")?;
            let s = self.state.lock().unwrap();
            Ok(s.workspaces
                .values()
                .map(|w| NodeWorkspace {
                    workspace: Workspace {
                        name: w.record.name.clone(),
                        configured: true,
                        owner: "mallory".into(),
                        ip: w.record.ip.clone(),
                        state: w.state,
                        unit: if w.state == State::Stopped { "inactive".into() } else { "active".into() },
                        vm: "-".into(),
                        agent: if w.agent_up { "up".into() } else { String::new() },
                        ..Workspace::default()
                    },
                    booted: (w.state != State::Stopped)
                        .then(|| Boot { packages: "/nix/store/x-packages".into(), apps: vec![] }),
                    next: Boot { packages: "/nix/store/x-packages".into(), apps: vec![] },
                    stopped: w.stopped,
                })
                .collect())
        })
    }

    fn make<'a>(&'a self, req: &'a MakeRequest) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async move {
            self.call("make", &req.record.name, "make")?;
            if !req.for_move {
                std::fs::write(self.disk(&req.record.name), b"a fresh disk").unwrap();
            }
            let ws = FakeWs {
                record: req.record.clone(),
                state: State::Stopped,
                agent_up: false,
                stopped: req.for_move,
                for_move: req.for_move,
                startable: true,
            };
            self.state.lock().unwrap().workspaces.insert(req.record.name.clone(), ws);
            Ok(())
        })
    }

    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async move {
            self.call("delete", name, "delete")?;
            self.state.lock().unwrap().workspaces.remove(name);
            let _ = std::fs::remove_file(self.disk(name));
            Ok(())
        })
    }

    fn control<'a>(&'a self, name: &'a str, verb: Verb, _wait: bool) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async move {
            let op = match verb {
                Verb::Start => "start",
                Verb::Stop => "stop",
                Verb::Restart => "restart",
            };
            self.call(op, name, op)?;
            let watch = self.watch.lock().unwrap().clone();
            if verb == Verb::Start
                && let Some(reg) = watch
                && reg.get(name).ok().flatten().is_some_and(|r| r.is_moving())
            {
                self.state.lock().unwrap().calls.push(format!("start while moving {name}"));
            }
            let mut s = self.state.lock().unwrap();
            let answer = s.agents_answer;
            let Some(w) = s.workspaces.get_mut(name).filter(|w| w.startable || verb == Verb::Stop) else {
                return Err(CallError::Refused { status: 404, error: format!("no {name}") });
            };
            match verb {
                Verb::Start | Verb::Restart => {
                    w.state = State::Running;
                    w.agent_up = answer;
                    w.stopped = false;
                }
                Verb::Stop => {
                    w.state = State::Stopped;
                    w.agent_up = false;
                    w.stopped = true;
                }
            }
            Ok(())
        })
    }

    fn open_link<'a>(&'a self, name: &'a str, app: &'a str) -> BoxFuture<'a, Result<OpenLinkAnswer, CallError>> {
        Box::pin(async move {
            self.call("open-link", name, "open-link")?;
            Ok(OpenLinkAnswer::Link { link: format!("https://{name}--{app}.ws.test/pair") })
        })
    }

    fn disk_out<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<DiskOut, CallError>> {
        Box::pin(async move {
            self.call("disk_out", name, "disk_out")?;
            let corrupt = self.state.lock().unwrap().fail.contains("corrupt");
            // As a source that lost power: one piece, then nothing, the
            // stream open.
            let stall = self.state.lock().unwrap().fail.contains("stall");
            let path = self.disk(name);
            let (tx, mut raw) = mpsc::channel::<Bytes>(8);
            tokio::task::spawn_blocking(move || disk::send(&path, &tx));
            let (out, rx) = mpsc::channel(8);
            tokio::spawn(async move {
                let mut first = true;
                while let Some(mut piece) = raw.recv().await {
                    if corrupt && !first && piece.len() > 16 {
                        let mut v = piece.to_vec();
                        v[16] ^= 1;
                        piece = Bytes::from(v);
                    }
                    first = false;
                    if out.send(Ok(piece)).await.is_err() {
                        return;
                    }
                    if stall {
                        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                    }
                }
            });
            Ok(rx)
        })
    }

    fn disk_in<'a>(
        &'a self,
        name: &'a str,
        mut input: mpsc::Receiver<Bytes>,
    ) -> BoxFuture<'a, Result<String, CallError>> {
        Box::pin(async move {
            self.call("disk_in", name, "disk_in")?;
            let tmp = self.dir.join(format!("{name}.raw.new"));
            let _ = std::fs::remove_file(&tmp);
            let stream = futures_util::stream::poll_fn(move |cx| input.poll_recv(cx).map(|o| o.map(Ok::<_, String>)));
            let hash = disk::receive(Box::pin(stream), &tmp)
                .await
                .map_err(|e| CallError::Refused { status: 400, error: format!("{e:#}") })?;
            std::fs::rename(&tmp, self.disk(name)).unwrap();
            Ok(disk::hex(&hash))
        })
    }
}
