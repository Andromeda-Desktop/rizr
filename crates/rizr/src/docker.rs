//! Docker provider and the shell loop: runs core effects, turns `docker events` and inspect output into core
//! events. The core decides; this file only renders and runs (SPEC-core section 6).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, IsTerminal};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rizr_core::error::Error;
use rizr_core::event::{RuntimeKind, Seen, Status, World};
use rizr_core::machine::Health;
use rizr_core::model::{
    Digest, EffectId, Gen, InstanceId, NodeId, NodeKind, NodeName, Probe, ProjectId, Scope,
};
use rizr_core::{Action, Answer, Event, Op, State, plan, reduce};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::compose;

enum Msg {
    Done(EffectId, Answer),
    Event(Event),
    /// A pull or build step of a node while compose starts it: shown to the caller, not part of the state.
    Progress(String, String),
}

/// The current container id per service. A `die`, health or destroy event from another id is stale: it
/// belongs to a container that a later `Start` replaced (DECISIONS 2026-11-13, step 2 spike).
type Current = Arc<Mutex<BTreeMap<String, String>>>;

/// A verb returns before an agent's 120 s tool timeout; the runtime keeps starting and the next `up` waits again.
const BUDGET: Duration = Duration::from_secs(100);

pub struct Attach {
    pub instance: String,
    pub compose_name: String,
    /// Its data dir: its `postgres.json` names the databases its containers serve.
    pub dir: PathBuf,
}

pub struct Stack {
    pub root: PathBuf,
    /// Compose files given with `-f`; empty means the default names.
    pub files: Vec<PathBuf>,
    pub project: ProjectId,
    pub instance: InstanceId,
    /// rizr's data directory for this instance: generation and override files. Never inside the repo.
    pub dir: PathBuf,
    compose_name: String,
    /// From `rizr.toml` `attach`: the instance whose containers this one uses; it then runs only host services.
    pub attach: Option<Attach>,
    /// The compose project name of the main worktree, whose data a new instance starts from.
    pub main_compose_name: String,
    /// The main worktree's root.
    pub main_root: PathBuf,
}

impl Stack {
    /// The project is the repo, shared by all its git worktrees; the instance is `main` for the main worktree,
    /// else the worktree's directory name.
    pub fn new(root: PathBuf, files: Vec<PathBuf>, data: &Path) -> Self {
        let main = main_worktree(&root);
        let main_root = main.as_deref().unwrap_or(&root);
        // The path hash keeps two repos with one directory name apart: their servers and data never mix.
        let path_hash = compose::hex(&Sha256::digest(main_root.as_os_str().as_encoded_bytes()));
        let project = ProjectId(format!("{}-{}", slug(main_root), &path_hash[..6]));
        let instance = InstanceId(main.as_ref().map_or("main".into(), |_| slug(&root)));
        let dir = data.join(format!("{}-{}", project.0, instance.0));
        let main_compose_name = compose::default_name(main_root, &files);
        let main_root = main_root.to_path_buf();
        let attach = crate::manifest::attach(&root)
            .filter(|a| *a != instance.0)
            .map(|a| Attach {
                compose_name: if a == "main" {
                    main_compose_name.clone()
                } else {
                    format!("{main_compose_name}-{a}")
                },
                dir: data.join(format!("{}-{a}", project.0)),
                instance: a,
            });
        let compose_name = match &main {
            None => compose::default_name(&root, &files),
            Some(m) => format!("{}-{}", compose::default_name(m, &files), instance.0),
        };
        Self {
            root,
            files,
            project,
            instance,
            dir,
            compose_name,
            main_compose_name,
            main_root,
            attach,
        }
    }

    /// Hold this instance's lock until the file drops: one mutating rizr call per instance at a time. The OS
    /// releases it when the process dies, so a kill -9 leaves no stale lock.
    pub fn lock(&self) -> std::io::Result<std::fs::File> {
        std::fs::create_dir_all(&self.dir)?;
        let f = std::fs::File::create(self.dir.join("lock"))?;
        if f.try_lock().is_err() {
            eprintln!(
                "waiting: another rizr call (up, down or restart) still runs on {}; stop it to go on now",
                self.compose_name
            );
            f.lock()?;
        }
        Ok(f)
    }

    /// The compose project name: the one plain `docker compose` uses in the main worktree, so its volumes carry
    /// over; `<that>-<instance>` in another worktree.
    pub fn compose_name(&self) -> String {
        self.compose_name.clone()
    }

    pub fn compose(&self) -> Command {
        compose::command(&self.root, &self.files, &self.compose_name(), &self.dir)
    }
}

/// An instance name from a directory name: lowercase, other characters as `-`.
pub fn slug(p: &Path) -> String {
    let s: String = p
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.is_empty() { "root".into() } else { s }
}

/// For a linked git worktree, the main worktree's root. Reads only the `.git` file and `commondir`: no git call,
/// so it is safe before trust.
fn main_worktree(root: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(root.join(".git")).ok()?;
    let gitdir = root.join(text.strip_prefix("gitdir:")?.trim());
    let common = std::fs::read_to_string(gitdir.join("commondir"))
        .map_or_else(|_| gitdir.join("../.."), |c| gitdir.join(c.trim()));
    let common = common.canonicalize().ok()?;
    (common.file_name()? == ".git").then(|| common.parent().map(Path::to_path_buf))?
}

/// Run one action to its outcome. The caller has passed the trust gate.
/// Run one action to its outcome; `None` only observes (status, why, plan, context).
pub fn run(stack: &Stack, inputs: Digest, action: Option<Action>) -> Result<State, Error> {
    let gen_file = stack.dir.join("generation");
    let generation = std::fs::read_to_string(&gen_file)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .map_or(Gen(0), Gen);
    let (tx, rx) = channel();
    let mut p = Provider {
        stack,
        tx,
        current: Current::default(),
        events: None,
        deadlines: BTreeMap::new(),
        side: Arc::default(),
        host: Arc::default(),
        pg: Arc::default(),
        dbs: Arc::default(),
        pg_env: Arc::default(),
        units: crate::supervisor::Units::of(stack),
        unit_events: None,
        project_lock: Arc::default(),
        progress: action.is_some().then(|| (Instant::now(), BTreeMap::new())),
        quiet: (!std::io::stderr().is_terminal()).then(Instant::now),
        budget: match &action {
            Some(Action::Up {
                timeout: Some(t), ..
            }) => Duration::from_secs(*t),
            _ => BUDGET,
        },
    };
    let mut s = State::new(stack.project.clone(), stack.instance.clone(), generation);
    s = reduce(
        s,
        Event::Hashed {
            inputs,
            trusted: true,
        },
    );
    if let Some(a) = action {
        s = reduce(s, Event::Action(a));
    }
    let result = p.drive(s, &rx);
    if let Ok(s) = &result {
        let _ = std::fs::create_dir_all(&stack.dir);
        let _ = std::fs::write(&gen_file, s.intents.generation.0.to_string());
    }
    result
}

struct Provider<'a> {
    stack: &'a Stack,
    tx: Sender<Msg>,
    current: Current,
    events: Option<Child>,
    deadlines: BTreeMap<EffectId, Instant>,
    /// Per service data from the import that the model does not carry.
    side: Arc<Mutex<BTreeMap<String, compose::Side>>>,
    /// Host-process services from `rizr.toml`, run by the supervisor.
    host: Arc<Mutex<BTreeMap<String, crate::manifest::HostService>>>,
    /// Shared postgres servers by node name.
    pg: Arc<Mutex<BTreeMap<String, crate::postgres::Server>>>,
    /// This instance's databases on shared servers, by compose service name.
    dbs: Arc<Mutex<BTreeMap<String, crate::postgres::Db>>>,
    /// Per compose service, the env vars that point it at this instance's databases.
    pg_env: Arc<Mutex<BTreeMap<String, BTreeMap<String, String>>>>,
    units: crate::supervisor::Units,
    unit_events: Option<Child>,
    /// Held while this instance changes a node that all instances share; the OS frees it if rizr dies.
    project_lock: Arc<Mutex<Option<std::fs::File>>>,
    /// For a verb that changes the stack: its start and the last state printed per node.
    progress: Option<(Instant, BTreeMap<String, String>)>,
    /// Set when stderr is no terminal (an agent or a log): the time of the last line. Then only problem
    /// states and a heartbeat print, so the result is near the top of the output.
    quiet: Option<Instant>,
    /// How long a verb waits before it returns "still starting": `up --timeout`, else 100 s.
    budget: Duration,
}

impl Drop for Provider<'_> {
    fn drop(&mut self) {
        for c in [&mut self.events, &mut self.unit_events]
            .into_iter()
            .flatten()
        {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Provider<'_> {
    fn drive(&mut self, mut s: State, rx: &Receiver<Msg>) -> Result<State, Error> {
        let limit = self.progress.as_ref().map(|(t, _)| *t + self.budget);
        loop {
            for _ in 0..64 {
                let planned = plan(&s);
                if planned.is_empty() {
                    break;
                }
                for e in planned {
                    let (id, op) = (e.id, e.op.clone());
                    if std::env::var_os("RIZR_TRACE").is_some() {
                        eprintln!("effect {:?} {:?} ({})", id, op, e.reason);
                    }
                    s = reduce(s, Event::Sent(e));
                    self.dispatch(&s, id, op);
                }
            }
            if let Some(r) = rizr_core::view::outcome(&s) {
                return r.map(|()| s);
            }
            self.print_progress(&s);
            // In quiet mode no message may arrive for minutes: wake up for the heartbeat.
            let wake = |t: Instant| match self.quiet {
                Some(_) => t.min(Instant::now() + Duration::from_secs(10)),
                None => t,
            };
            let next = self
                .deadlines
                .iter()
                .min_by_key(|(_, t)| **t)
                .map(|(id, t)| (*id, *t));
            if limit.is_some_and(|l| Instant::now() >= l) {
                return Err(self.still_starting(&s));
            }
            let msg = match next {
                Some((id, at)) if limit.is_none_or(|l| at <= l) => {
                    match rx.recv_timeout(wake(at).saturating_duration_since(Instant::now())) {
                        Err(RecvTimeoutError::Timeout) if Instant::now() < at => continue,
                        Err(RecvTimeoutError::Timeout) => {
                            self.deadlines.remove(&id);
                            s = reduce(s, Event::DeadlineHit(id));
                            continue;
                        }
                        m => m.ok(),
                    }
                }
                _ => match limit {
                    Some(l) => {
                        match rx.recv_timeout(wake(l).saturating_duration_since(Instant::now())) {
                            Err(RecvTimeoutError::Timeout) => continue,
                            m => m.ok(),
                        }
                    }
                    None => rx.recv().ok(),
                },
            };
            match msg {
                Some(Msg::Done(id, answer)) => s = reduce(s, Event::Done { id, answer }),
                Some(Msg::Event(e)) => s = reduce(s, e),
                Some(Msg::Progress(node, step)) => {
                    if let (Some((start, _)), None) = (&self.progress, self.quiet) {
                        eprintln!("[{:>3}s] {node} {step}", start.elapsed().as_secs());
                    }
                }
                None => unreachable!("the provider holds a sender"),
            }
            self.print_progress(&s);
        }
    }

    /// The budget ran out: each node still on its way, with its last log line, and the command that waits again.
    fn still_starting(&self, s: &State) -> Error {
        let ids = self.current.lock().map(|c| c.clone()).unwrap_or_default();
        let lines: Vec<String> = rizr_core::view::nodes(s)
            .into_iter()
            .filter(|n| {
                !["ready", "done", "started"]
                    .iter()
                    .any(|p| n.state.starts_with(p))
            })
            .map(|n| {
                let last = ids
                    .get(&n.node.name.0)
                    .and_then(|id| {
                        Command::new("docker")
                            .args(["logs", "--tail", "1", id])
                            .output()
                            .ok()
                    })
                    .map(|o| {
                        let all = [o.stdout, o.stderr].concat();
                        String::from_utf8_lossy(&all)
                            .trim()
                            .chars()
                            .take(160)
                            .collect::<String>()
                    })
                    .filter(|l| !l.is_empty())
                    .map(|l| format!(", last log: {l}"))
                    .unwrap_or_default();
                let state = if ["absent", "pending"].contains(&n.state.as_str()) {
                    "not started yet: it waits for a dependency, or its image pull or build runs"
                        .into()
                } else {
                    n.state.to_string()
                };
                format!("{} {state}{last}", n.node.name.0)
            })
            .collect();
        let again = std::iter::once("rizr".to_owned())
            .chain(std::env::args().skip(1))
            .collect::<Vec<_>>()
            .join(" ");
        Error::new(
            rizr_core::error::ErrorCode::StillRunning,
            format!(
                "not failed, still starting after {} s: {}",
                self.budget.as_secs(),
                lines.join("; ")
            ),
            format!(
                "the containers keep starting in the background and nothing restarts; for a slow start wait in one call, with a timeout below your command's own: `rizr up --timeout 540`; `{again}` waits {} s more",
                self.budget.as_secs()
            ),
            {
                let mut args = std::env::args().skip(1).peekable();
                let mut kept = vec!["rizr".to_owned()];
                while let Some(a) = args.next() {
                    if a == "--timeout" {
                        args.next();
                    } else if !a.starts_with("--timeout=") {
                        kept.push(a);
                    }
                }
                kept.push("--timeout 540".into());
                kept.join(" ")
            },
        )
    }

    /// One stderr line per node state change (in quiet mode: problems and a 30 s heartbeat), so a caller with a
    /// short timeout sees how far `up` got.
    fn print_progress(&mut self, s: &State) {
        let Some((start, last)) = &mut self.progress else {
            return;
        };
        let nodes = rizr_core::view::nodes(s);
        if let Some(beat) = &mut self.quiet
            && beat.elapsed() >= Duration::from_secs(30)
        {
            let busy: Vec<String> = nodes
                .iter()
                .filter(|n| {
                    [
                        "starting",
                        "building",
                        "pulling",
                        "pending",
                        "stopping",
                        "restarting",
                    ]
                    .iter()
                    .any(|b| n.state.starts_with(b))
                })
                .map(|n| format!("{} ({})", n.node.name.0, n.state))
                .collect();
            if !busy.is_empty() {
                eprintln!(
                    "[{:>3}s] waiting for {}",
                    start.elapsed().as_secs(),
                    busy.join(", ")
                );
                *beat = Instant::now();
            }
        }
        for n in nodes {
            let state = n.state.to_string();
            let problem = ["exited", "restarting", "failed"]
                .iter()
                .any(|p| state.starts_with(p));
            if last.get(&n.node.name.0) != Some(&state) {
                if self.quiet.is_none() || problem {
                    eprintln!(
                        "[{:>3}s] {} {state}",
                        start.elapsed().as_secs(),
                        n.node.name.0
                    );
                }
                last.insert(n.node.name.0, state);
            }
        }
    }

    fn dispatch(&mut self, s: &State, id: EffectId, op: Op) {
        let tx = self.tx.clone();
        let progress = self.tx.clone();
        let done = move |a: Answer| {
            let _ = tx.send(Msg::Done(id, a));
        };
        let st = self.stack;
        match op {
            Op::AcquireProjectLock => {
                let (held, path) = (
                    self.project_lock.clone(),
                    st.dir
                        .with_file_name(format!("{}.project.lock", st.project.0)),
                );
                std::thread::spawn(move || {
                    let f = std::fs::File::create(&path).and_then(|f| f.lock().map(|()| f));
                    match (f, held.lock()) {
                        (Ok(f), Ok(mut h)) => {
                            *h = Some(f);
                            done(Answer::Ok);
                        }
                        (Err(e), _) => done(Answer::Err {
                            tail: format!("{}: {e}", path.display()),
                        }),
                        (_, Err(e)) => done(Answer::Err {
                            tail: e.to_string(),
                        }),
                    }
                });
            }
            Op::ReleaseProjectLock => {
                if let Ok(mut h) = self.project_lock.lock() {
                    *h = None;
                }
            }
            Op::SetDeadline { after_ms, .. } => {
                self.deadlines
                    .insert(id, Instant::now() + Duration::from_millis(after_ms));
            }
            Op::PromptTrust { .. } => {
                let granted = crate::trust::gate(&st.root, &st.files, true).is_ok();
                done(Answer::Trust { granted });
            }
            Op::Subscribe => done(match self.subscribe() {
                Ok(()) => Answer::Ok,
                Err(tail) => Answer::Err { tail },
            }),
            Op::Observe => {
                let (labels, current, instance) = (
                    self.labels(),
                    self.current.clone(),
                    self.stack.instance.clone(),
                );
                let units = self.units.clone();
                let (dbs, project) = (
                    self.dbs.lock().map(|d| d.clone()).unwrap_or_default(),
                    self.stack.project.0.clone(),
                );
                std::thread::spawn(move || {
                    done(
                        match observe(&labels, &instance, &current).and_then(|mut w| {
                            w.seen.extend(crate::supervisor::observe(&units)?);
                            // A shared database is up while its server answers to the service name; a
                            // container of the same name is the compose service from before the switch.
                            for (svc, db) in &dbs {
                                w.seen.retain(|x| {
                                    x.node.name.0 != *svc || x.node.scope == Scope::Project
                                });
                                let server = inspect_one(&db.shared.server.container(&project));
                                if server.is_ok_and(|s| db.attached(&s, svc)) {
                                    w.seen.push(Seen {
                                        node: NodeId {
                                            scope: Scope::Instance(instance.clone()),
                                            name: NodeName(svc.clone()),
                                        },
                                        spec: db.spec.clone(),
                                        generation: Gen(0),
                                        status: Status::Running {
                                            health: Some(Health::Healthy),
                                        },
                                    });
                                }
                            }
                            Ok(w)
                        }) {
                            Ok(w) => Answer::Observed(w),
                            Err(tail) => Answer::Err { tail },
                        },
                    );
                });
            }
            Op::Import => {
                let attach = st
                    .attach
                    .as_ref()
                    .map(|a| (a.instance.clone(), running_services(&st.project.0, a)));
                let (root, files, name, dir, instance, side) = (
                    st.root.clone(),
                    st.files.clone(),
                    st.compose_name(),
                    st.dir.clone(),
                    st.instance.clone(),
                    self.side.clone(),
                );
                let (host, pg, dbs, pg_env) = (
                    self.host.clone(),
                    self.pg.clone(),
                    self.dbs.clone(),
                    self.pg_env.clone(),
                );
                let inputs = match &s.trust {
                    rizr_core::state::Trust::Trusted(d) => d.clone(),
                    _ => Digest(String::new()),
                };
                std::thread::spawn(move || {
                    done(
                        match import(
                            &root,
                            &files,
                            &name,
                            &dir,
                            &instance,
                            attach.as_ref(),
                            inputs,
                        ) {
                            Ok((m, imported, services, res)) => {
                                if let Ok(mut r) = pg.lock() {
                                    *r = res.servers;
                                }
                                if let Ok(mut r) = dbs.lock() {
                                    *r = res.dbs;
                                }
                                if let Ok(mut r) = pg_env.lock() {
                                    *r = res.env;
                                }
                                if let Ok(mut r) = side.lock() {
                                    *r = imported;
                                }
                                if let Ok(mut r) = host.lock() {
                                    *r = services;
                                }
                                Answer::Imported(m)
                            }
                            Err(tail) => Answer::Err { tail },
                        },
                    );
                });
            }
            Op::Inspect(node) if self.is_host(&node.name) => {
                done(Answer::Inspected { restarting: false })
            }
            Op::Inspect(node) => {
                let cid = self.container(&node.name);
                std::thread::spawn(move || {
                    let restarting = cid
                        .and_then(|c| inspect(&[c]).ok())
                        .and_then(|v| v.first()?["State"]["Restarting"].as_bool())
                        .unwrap_or(false);
                    done(Answer::Inspected { restarting });
                });
            }
            Op::Probe(node) if self.is_host(&node.name) => {
                let probe = match s.loaded().and_then(|m| m.nodes.get(&node)).map(|n| &n.kind) {
                    Some(NodeKind::Service { probe }) => probe.clone(),
                    _ => Probe::AtStart,
                };
                let unit = crate::supervisor::Units::of(self.stack).unit(&node.name.0);
                std::thread::spawn(move || {
                    let ok = match &probe {
                        Probe::Tcp { ports } => crate::supervisor::probe(ports, None),
                        Probe::Http { port, path } => {
                            crate::supervisor::probe(&[], Some((*port, path)))
                        }
                        Probe::LogLine { pattern } => {
                            let mut show = Command::new("systemctl");
                            show.args(["--user", "show", "-p", "InvocationID", "--value", &unit]);
                            let id = show.output().map_or(String::new(), |o| {
                                String::from_utf8_lossy(&o.stdout).trim().to_owned()
                            });
                            let mut c = Command::new("journalctl");
                            c.args(["--user", "-f", "-o", "cat", "-n", "all"])
                                .arg(format!("_SYSTEMD_INVOCATION_ID={id}"));
                            !id.is_empty() && log_line(c, pattern)
                        }
                        _ => true,
                    };
                    done(Answer::Probe { ok })
                });
            }
            Op::Probe(node) => {
                let kind = s.loaded().and_then(|m| m.nodes.get(&node)).map(|n| &n.kind);
                let (ports, pattern) = match kind {
                    Some(NodeKind::Service {
                        probe: Probe::Tcp { ports },
                    }) => (ports.clone(), None),
                    Some(NodeKind::Service {
                        probe: Probe::LogLine { pattern },
                    }) => (vec![], Some(pattern.clone())),
                    _ => (vec![], None),
                };
                let cid = self.container(&node.name);
                std::thread::spawn(move || {
                    let ok = match (pattern, cid) {
                        (Some(p), Some(c)) => {
                            // A restarted container keeps its old lines; only this run's lines count.
                            let since = inspect(std::slice::from_ref(&c))
                                .ok()
                                .and_then(|v| {
                                    v.first()?["State"]["StartedAt"].as_str().map(str::to_owned)
                                })
                                .unwrap_or_default();
                            let mut cmd = Command::new("sh");
                            cmd.args([
                                "-c",
                                "exec docker logs -f --since \"$2\" \"$1\" 2>&1",
                                "sh",
                                &c,
                                &since,
                            ]);
                            !since.is_empty() && log_line(cmd, &p)
                        }
                        (Some(_), None) => false,
                        (None, cid) => probe(cid, &ports),
                    };
                    done(Answer::Probe { ok })
                });
            }
            Op::Start {
                node,
                spec,
                generation,
            } if self.is_host(&node.name) => {
                let mut svc = self
                    .host
                    .lock()
                    .ok()
                    .and_then(|h| h.get(&node.name.0).cloned());
                // An attached instance uses the databases of the instance it attaches to.
                let dbs: BTreeMap<String, crate::postgres::DbInfo> = match &self.stack.attach {
                    Some(a) => crate::postgres::Info::load(&a.dir).databases,
                    None => self
                        .dbs
                        .lock()
                        .map(|d| d.iter().map(|(n, d)| (n.clone(), d.info())).collect())
                        .unwrap_or_default(),
                };
                let project = self.stack.project.0.clone();
                let units = self.units.clone();
                std::thread::spawn(move || {
                    // Host processes reached the database on its published port; now it is the server's port.
                    for db in dbs.values() {
                        let port =
                            crate::postgres::host_port(&format!("rizr-{project}-{}", db.server));
                        if let (Some(s), Some(p)) = (svc.as_mut(), port) {
                            let one = BTreeMap::from([(String::new(), db.clone())]);
                            let vars = crate::postgres::rewrite(&s.env, &one, Some(p));
                            s.env.extend(vars);
                        }
                    }
                    done(match svc {
                        Some(svc) => exec(crate::supervisor::start(
                            &units,
                            &node.name.0,
                            &svc,
                            &spec,
                            generation,
                        )),
                        None => Answer::Err {
                            tail: format!("{} is not in rizr.toml", node.name.0),
                        },
                    })
                });
            }
            Op::Start { node, spec, .. } if self.db(&node).is_some() => {
                let db = self.db(&node);
                let (project, tx) = (self.stack.project.0.clone(), self.tx.clone());
                let instance = self.stack.instance.0.clone();
                let dir = self.stack.dir.clone();
                let names = (
                    self.stack.main_compose_name.clone(),
                    self.stack.compose_name(),
                );
                let mut old = self.stack.compose();
                old.args(["rm", "--stop", "--force"]).arg(&node.name.0);
                let mut ps = self.stack.compose();
                ps.args(["ps", "-aq"]).arg(&node.name.0);
                std::thread::spawn(move || {
                    let r = db.ok_or_else(String::new).and_then(|db| {
                        // The old container's mount holds the data, even an anonymous volume; it must stop
                        // before another postgres opens the same data.
                        let mount = ps
                            .output()
                            .ok()
                            .and_then(|o| {
                                String::from_utf8_lossy(&o.stdout)
                                    .lines()
                                    .next()
                                    .map(str::to_owned)
                            })
                            .and_then(|id| inspect_one(&id).ok())
                            .and_then(|c| {
                                c["Mounts"].as_array()?.iter().find_map(|m| {
                                    let target = m["Destination"].as_str()?;
                                    let source =
                                        m["Name"].as_str().or_else(|| m["Source"].as_str())?;
                                    target
                                        .starts_with("/var/lib/postgresql")
                                        .then(|| (source.to_owned(), target.to_owned()))
                                })
                            })
                            .or_else(|| db.shared.old_data.clone());
                        let _ = old.output();
                        if db.attach(&project, &node.name.0)? && instance != "main" {
                            let mark = dir.join(format!("seed-{}", node.name.0));
                            match db.seed(&project, &node.name.0, &names.0, &names.1)? {
                                Some(source) => {
                                    let n = crate::postgres::tables(
                                        &db.shared.server.container(&project),
                                        db.shared.server.user(),
                                        &db.database,
                                    )
                                    .map_or(String::new(), |n| format!(" (tables: {n})"));
                                    std::fs::write(&mark, format!("{source}{n}"))
                                }
                                None => std::fs::remove_file(&mark).or(Ok(())),
                            }
                            .map_err(|e| e.to_string())?;
                        }
                        match mount {
                            Some(m) => db.import(&project, &instance, &m).map(|_| ()),
                            None => Ok(()),
                        }
                    });
                    if r.is_ok() {
                        // The database exists and the name resolves: that is its health check.
                        let _ = tx.send(Msg::Event(synthetic(
                            node.clone(),
                            spec.clone(),
                            RuntimeKind::Start,
                        )));
                        let _ = tx.send(Msg::Event(synthetic(
                            node,
                            spec,
                            RuntimeKind::Health(Health::Healthy),
                        )));
                    }
                    done(match r {
                        Ok(()) => Answer::Ok,
                        Err(tail) => Answer::Err { tail },
                    });
                });
            }
            Op::Start {
                node,
                spec,
                generation,
            } if node.scope == Scope::Project => {
                let (project, tx) = (self.stack.project.0.clone(), self.tx.clone());
                let pg = self
                    .pg
                    .lock()
                    .ok()
                    .and_then(|p| p.get(&node.name.0).cloned());
                std::thread::spawn(move || {
                    done(match pg {
                        Some(pg) => {
                            let name = pg.container(&project);
                            let running = inspect(std::slice::from_ref(&name))
                                .ok()
                                .and_then(|v| v.into_iter().next())
                                .filter(|c| c["State"]["Running"] == true);
                            let mut same = running.as_ref().is_some_and(|c| {
                                c["Config"]["Labels"]["rizr.spec"] == spec.0.as_str()
                            });
                            // The image applies POSTGRES_PASSWORD only when it creates the data. A changed
                            // compose value needs no new server: the role takes it on the running one.
                            if let (false, Some(c), Some(pw)) =
                                (same, &running, pg.env.get("POSTGRES_PASSWORD"))
                            {
                                same = password_only(c, &pg)
                                    && crate::postgres::admin(
                                        &name,
                                        pg.user(),
                                        &format!(
                                            "alter role \"{}\" password '{}'",
                                            pg.user().replace('"', "\"\""),
                                            pw.replace('\'', "''")
                                        ),
                                        None,
                                    )
                                    .is_ok();
                            }
                            // Another instance started it while this one waited for the project lock: use it.
                            // Its start event came before this start, so this instance replays what it missed.
                            if let (true, Some(c)) = (same, &running) {
                                let _ = tx.send(Msg::Event(synthetic(
                                    node.clone(),
                                    spec.clone(),
                                    RuntimeKind::Start,
                                )));
                                if let Some(h) =
                                    c["State"]["Health"]["Status"].as_str().and_then(health)
                                {
                                    let _ = tx.send(Msg::Event(synthetic(
                                        node,
                                        spec,
                                        RuntimeKind::Health(h),
                                    )));
                                }
                                Answer::Ok
                            // Other instances use a running server: never replace it under them.
                            } else if let Some(c) = running {
                                Answer::Err {
                                    tail: format!(
                                        "{name} runs {}, and other worktrees may use it; this one wants {} or other \
                                         POSTGRES_* values. Make the compose files agree, or set \
                                         `[resource.<service>] shared = false` in rizr.toml. `docker rm -f \
                                         {name}` replaces it for all worktrees; its data volume stays.",
                                        c["Config"]["Image"].as_str().unwrap_or("?"),
                                        pg.image
                                    ),
                                }
                            } else {
                                let _ = Command::new("docker")
                                    .args(["rm", "--force", &name])
                                    .output();
                                exec(pg.run(&project, &spec, generation.0))
                            }
                        }
                        None => Answer::Err {
                            tail: format!("{} is not a shared server of this project", node.name.0),
                        },
                    })
                });
            }
            Op::Start {
                node,
                spec,
                generation,
            } => {
                if let Ok(mut c) = self.current.lock() {
                    c.remove(&node.name.0);
                }
                // A data job may break the data: each of this instance's databases gets a rewind point first.
                let is_job = matches!(
                    s.loaded().and_then(|m| m.nodes.get(&node)).map(|n| &n.kind),
                    Some(NodeKind::Job { .. })
                );
                let points: Vec<_> = if is_job {
                    self.dbs
                        .lock()
                        .map(|d| {
                            d.values()
                                .map(|db| db.rewind_target(&self.stack.project.0))
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                let (dir, job) = (self.stack.dir.clone(), node.name.0.clone());
                let log = self.stack.dir.join(format!("start-{job}.log"));
                let mut cmd = self.stack.compose();
                let r = self.write_override(&node, &spec, generation).map(|files| {
                    for f in files {
                        cmd.arg("-f").arg(f);
                    }
                    cmd.args(["up", "-d", "--build", "--no-deps"])
                        .arg(&node.name.0);
                });
                std::thread::spawn(move || {
                    for t in &points {
                        crate::rewind::take(t, &dir, &job, true);
                    }
                    done(match r {
                        Ok(()) => exec_progress(cmd, &job, &progress, &log),
                        Err(tail) => Answer::Err { tail },
                    });
                });
            }
            Op::Stop(node) if self.is_host(&node.name) => {
                let cmd = crate::supervisor::stop(&self.units, &node.name.0);
                std::thread::spawn(move || done(exec(cmd)));
            }
            Op::Stop(node) if self.db(&node).is_some() => {
                let (db, project, tx) = (
                    self.db(&node),
                    self.stack.project.0.clone(),
                    self.tx.clone(),
                );
                let spec = s
                    .nodes
                    .get(&node)
                    .and_then(|n| n.live.clone())
                    .unwrap_or(Digest(String::new()));
                std::thread::spawn(move || {
                    let r = db.map_or(Ok(()), |db| db.detach(&project));
                    if r.is_ok() {
                        let _ = tx.send(Msg::Event(synthetic(node, spec, RuntimeKind::Destroy)));
                    }
                    done(match r {
                        Ok(()) => Answer::Ok,
                        Err(tail) => Answer::Err { tail },
                    });
                });
            }
            Op::Stop(node) if node.scope == Scope::Project => {
                let mut cmd = Command::new("docker");
                cmd.args(["rm", "--force"])
                    .arg(format!("rizr-{}-{}", self.stack.project.0, node.name.0));
                std::thread::spawn(move || done(exec(cmd)));
            }
            Op::Stop(node) => {
                let mut cmd = self.stack.compose();
                cmd.args(["rm", "--stop", "--force"]).arg(&node.name.0);
                std::thread::spawn(move || done(exec(cmd)));
            }
        }
    }

    /// The shared database behind an instance node, when the node is one.
    fn db(&self, node: &NodeId) -> Option<crate::postgres::Db> {
        if node.scope == Scope::Project {
            return None;
        }
        self.dbs.lock().ok()?.get(&node.name.0).cloned()
    }

    fn is_host(&self, node: &NodeName) -> bool {
        self.host.lock().is_ok_and(|h| h.contains_key(&node.0))
    }

    /// Docker filters AND their labels, so the filter is the project; `scope_of` keeps this instance's
    /// containers and the shared ones.
    fn labels(&self) -> [String; 1] {
        [format!("rizr.project={}", self.stack.project.0)]
    }

    fn container(&self, node: &NodeName) -> Option<String> {
        self.current.lock().ok()?.get(&node.0).cloned()
    }

    /// One override file per node: compose merges it over the repo files, so the repo stays untouched.
    fn write_override(
        &self,
        node: &NodeId,
        spec: &Digest,
        generation: Gen,
    ) -> Result<Vec<PathBuf>, String> {
        let st = self.stack;
        let mut svc = json!({"labels": {
            "rizr.project": st.project.0, "rizr.instance": st.instance.0, "rizr.node": node.name.0,
            "rizr.spec": spec.0, "rizr.gen": generation.0.to_string()}});
        let side = self
            .side
            .lock()
            .ok()
            .and_then(|r| r.get(&node.name.0).cloned())
            .unwrap_or_default();
        if !side.restart {
            svc["restart"] = json!("on-failure:5");
        }
        if let Some(name) = side
            .container_name
            .as_ref()
            .filter(|_| st.instance.0 != "main")
        {
            svc["container_name"] = json!(format!("{name}-{}", st.instance.0));
        }
        if let Some(env) = self
            .pg_env
            .lock()
            .ok()
            .and_then(|e| e.get(&node.name.0).cloned())
        {
            svc["environment"] = json!(env);
        }
        let path = st.dir.join(format!("override-{}.yml", node.name.0));
        std::fs::create_dir_all(&st.dir).map_err(|e| e.to_string())?;
        // JSON is YAML: compose reads it, and no value needs YAML quoting.
        std::fs::write(&path, json!({"services": {&node.name.0: svc}}).to_string())
            .map_err(|e| e.to_string())?;
        let ports = crate::ports::prepare(
            &st.dir,
            &st.root,
            &node.name.0,
            &side.ports,
            &st.dir
                .with_file_name(format!("{}.ports.json", st.project.0)),
            &st.instance.0,
        )?;
        Ok([Some(path), ports].into_iter().flatten().collect())
    }

    fn subscribe(&mut self) -> Result<(), String> {
        for c in [self.events.take(), self.unit_events.take()]
            .into_iter()
            .flatten()
        {
            let mut c = c;
            let _ = c.kill();
            let _ = c.wait();
        }
        let tx = self.tx.clone();
        self.unit_events = Some(crate::supervisor::subscribe(&self.units, move |e| {
            tx.send(Msg::Event(e)).is_ok()
        })?);
        // `--since` replays from before the call: no event is lost between the stream start and the observe.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let mut cmd = Command::new("docker");
        cmd.args(["events", "--format", "{{json .}}", "--since"])
            .arg(format!("{}.{:09}", now.as_secs(), now.subsec_nanos()));
        for l in self.labels() {
            cmd.arg("--filter").arg(format!("label={l}"));
        }
        for e in ["start", "die", "destroy", "health_status"] {
            cmd.arg("--filter").arg(format!("event={e}"));
        }
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot run docker events: {e}"))?;
        let out = child.stdout.take().ok_or("docker events has no stdout")?;
        let (tx, current, instance) = (
            self.tx.clone(),
            self.current.clone(),
            self.stack.instance.clone(),
        );
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if let Some(e) = runtime_event(&line, &instance, &current) {
                    let _ = tx.send(Msg::Event(e));
                }
            }
            let _ = tx.send(Msg::Event(Event::StreamLost));
        });
        self.events = Some(child);
        Ok(())
    }
}

fn runtime_event(line: &str, instance: &InstanceId, current: &Current) -> Option<Event> {
    let v: Value = serde_json::from_str(line).ok()?;
    let attrs = &v["Actor"]["Attributes"];
    let (cid, node) = (v["Actor"]["ID"].as_str()?, attrs["rizr.node"].as_str()?);
    let scope = scope_of(attrs, instance)?;
    let action = v["Action"].as_str()?;
    let kind = match action {
        "start" => RuntimeKind::Start,
        "die" => RuntimeKind::Die {
            code: attrs["exitCode"]
                .as_str()
                .and_then(|c| c.parse().ok())
                .unwrap_or(-1),
        },
        "destroy" => RuntimeKind::Destroy,
        // Podman sends a bare `health_status` after each check run, with no result: read it from the container.
        "health_status" => RuntimeKind::Health(health(
            String::from_utf8_lossy(
                &std::process::Command::new("docker")
                    .args(["inspect", "--format", "{{.State.Health.Status}}", cid])
                    .output()
                    .ok()?
                    .stdout,
            )
            .trim(),
        )?),
        a => RuntimeKind::Health(health(a.strip_prefix("health_status: ")?)?),
    };
    let mut cur = current.lock().ok()?;
    if kind == RuntimeKind::Start {
        cur.insert(node.into(), cid.into());
    } else if cur.get(node).map(String::as_str) != Some(cid) {
        return None;
    }
    Some(Event::Runtime {
        node: NodeId {
            scope,
            name: NodeName(node.into()),
        },
        spec: Digest(attrs["rizr.spec"].as_str()?.into()),
        kind,
        at: v["timeNano"].as_u64()?,
    })
}

fn health(s: &str) -> Option<Health> {
    match s {
        "healthy" => Some(Health::Healthy),
        "unhealthy" => Some(Health::Unhealthy),
        "starting" => Some(Health::Starting),
        _ => None,
    }
}

/// The scope of a container from its labels: shared by the project, this instance's, or another instance's (None).
fn scope_of(labels: &Value, instance: &InstanceId) -> Option<Scope> {
    if labels["rizr.scope"] == "project" {
        Some(Scope::Project)
    } else if labels["rizr.instance"] == instance.0.as_str() {
        Some(Scope::Instance(instance.clone()))
    } else {
        None
    }
}

fn observe(labels: &[String], instance: &InstanceId, current: &Current) -> Result<World, String> {
    // Events at or before `at` are in this snapshot; the core replays only later ones.
    let at = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    )
    .unwrap_or(u64::MAX);
    let mut cmd = Command::new("docker");
    cmd.args(["ps", "-aq", "--no-trunc"]);
    for l in labels {
        cmd.arg("--filter").arg(format!("label={l}"));
    }
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run docker ps: {e}"))?;
    if !out.status.success() {
        return Err(compose::tail(&out.stderr));
    }
    let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    let containers = if ids.is_empty() {
        vec![]
    } else {
        inspect(&ids)?
    };
    let mut seen = Vec::new();
    let mut cur = current.lock().map_err(|e| e.to_string())?;
    for c in &containers {
        let l = &c["Config"]["Labels"];
        let (Some(node), Some(scope), Some(spec)) = (
            l["rizr.node"].as_str(),
            scope_of(l, instance),
            l["rizr.spec"].as_str(),
        ) else {
            continue;
        };
        let st = &c["State"];
        let status = if st["Restarting"] == true {
            Status::Restarting
        } else if st["Running"] == true {
            Status::Running {
                health: st["Health"]["Status"].as_str().and_then(health),
            }
        } else if st["Status"] == "created" {
            Status::Created
        } else {
            Status::Exited {
                code: st["ExitCode"]
                    .as_i64()
                    .and_then(|c| i32::try_from(c).ok())
                    .unwrap_or(-1),
            }
        };
        if let Some(id) = c["Id"].as_str() {
            cur.insert(node.into(), id.into());
        }
        seen.push(Seen {
            node: NodeId {
                scope,
                name: NodeName(node.into()),
            },
            spec: Digest(spec.into()),
            generation: Gen(l["rizr.gen"]
                .as_str()
                .and_then(|g| g.parse().ok())
                .unwrap_or(0)),
            status,
        });
    }
    Ok(World { at, seen })
}

/// The runtime event of a node that has no container of its own.
fn synthetic(node: NodeId, spec: Digest, kind: RuntimeKind) -> Event {
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    Event::Runtime {
        node,
        spec,
        kind,
        at,
    }
}

/// The running server differs from the wanted one only in `POSTGRES_PASSWORD`.
fn password_only(running: &Value, pg: &crate::postgres::Server) -> bool {
    let env: BTreeMap<&str, &str> = running["Config"]["Env"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e.as_str()?.split_once('='))
        .filter(|(k, _)| k.starts_with("POSTGRES_") && *k != "POSTGRES_PASSWORD")
        .collect();
    let want: BTreeMap<&str, &str> = pg
        .env
        .iter()
        .filter(|(k, _)| *k != "POSTGRES_PASSWORD")
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    running["Config"]["Image"] == pg.image.as_str() && env == want
}

pub fn inspect_one(name: &str) -> Result<Value, String> {
    inspect(&[name.to_owned()])?
        .into_iter()
        .next()
        .ok_or_else(|| format!("no container {name}"))
}

fn inspect(ids: &[String]) -> Result<Vec<Value>, String> {
    let out = Command::new("docker")
        .arg("inspect")
        .args(ids)
        .output()
        .map_err(|e| format!("cannot run docker inspect: {e}"))?;
    if !out.status.success() {
        return Err(compose::tail(&out.stderr));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
}

/// One tcp connect to each container port on the container address.
fn probe(cid: Option<String>, ports: &[u16]) -> bool {
    let Some(c) = cid
        .and_then(|c| inspect(&[c]).ok())
        .and_then(|v| v.into_iter().next())
    else {
        return false;
    };
    let ip: Option<IpAddr> = c["NetworkSettings"]["Networks"]
        .as_object()
        .into_iter()
        .flat_map(|n| n.values())
        .find_map(|n| n["IPAddress"].as_str()?.parse().ok());
    let pid = c["State"]["Pid"].as_u64().filter(|p| *p > 0);
    !ports.is_empty()
        && ports.iter().all(|p| {
            ip.is_some_and(|ip| {
                TcpStream::connect_timeout(&SocketAddr::new(ip, *p), Duration::from_millis(400))
                    .is_ok()
            }) || pid.is_some_and(|pid| listening(pid, *p))
        })
}

/// Whether a socket in the network namespace of process `pid` listens on `port`. Rootless podman puts
/// containers on a network the host cannot reach; their own /proc view still shows the listener.
fn listening(pid: u64, port: u16) -> bool {
    let hex = format!(":{port:04X}");
    ["tcp", "tcp6"].iter().any(|f| {
        std::fs::read_to_string(format!("/proc/{pid}/net/{f}")).is_ok_and(|t| {
            t.lines().skip(1).any(|l| {
                let w: Vec<&str> = l.split_whitespace().collect();
                w.get(1).is_some_and(|a| a.ends_with(&hex)) && w.get(3) == Some(&"0A")
            })
        })
    })
}

/// Follows the output of `cmd` until a line contains `pattern` (true) or the output ends (false). The follow
/// ends after the start timeout, so a node that never prints the line leaves no process behind.
fn log_line(cmd: Command, pattern: &str) -> bool {
    let mut t = Command::new("timeout");
    t.arg((rizr_core::plan::START_TIMEOUT_MS / 1000 + 5).to_string())
        .arg(cmd.get_program())
        .args(cmd.get_args())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let Ok(mut child) = t.spawn() else {
        return false;
    };
    let found = child.stdout.take().is_some_and(|out| {
        BufReader::new(out)
            .lines()
            .map_while(Result::ok)
            .any(|l| l.contains(pattern))
    });
    let _ = child.kill();
    let _ = child.wait();
    found
}

/// `exec` that reads compose's output as it comes and reports each new pull or build step. Compose writes pulls
/// to stderr and build steps to stdout.
/// Compose writes to `log`, not to a pipe: when `up` returns at its budget, a pipe to the gone rizr would kill
/// the build with SIGPIPE on its next line, and every rerun would start the build again. Compose inherits the
/// lock on `log` with its stdout, so a later `up` waits for a start that still runs instead of starting another.
fn exec_progress(mut cmd: Command, node: &str, tx: &Sender<Msg>, log: &Path) -> Answer {
    let fail = |e: std::io::Error| Answer::Err {
        tail: format!("cannot run docker compose: {e}"),
    };
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        Ok(f) => f,
        Err(e) => return fail(e),
    };
    let mut from = file.metadata().map_or(0, |m| m.len());
    let follow = Command::new("tail")
        .arg(format!("--bytes=+{}", from + 1))
        .arg("--follow")
        .arg(log)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let reader = follow.ok().map(|mut t| {
        let out = t.stdout.take();
        let (tx, node) = (tx.clone(), node.to_string());
        let thread = std::thread::spawn(move || {
            let mut last = String::new();
            for line in out
                .into_iter()
                .flat_map(|o| BufReader::new(o).lines().map_while(Result::ok))
            {
                if let Some(step) = progress_step(&line)
                    && step != last
                {
                    let _ = tx.send(Msg::Progress(node.clone(), step.clone()));
                    last = step;
                }
            }
        });
        (t, thread)
    });
    let status = file
        .lock()
        .and_then(|()| {
            // A long history is dropped once no other compose writes to the file.
            if file.metadata()?.len() > 4 << 20 {
                file.set_len(0)?;
                from = 0;
            }
            let err = file.try_clone()?;
            cmd.stdin(Stdio::null()).stdout(file).stderr(err).spawn()
        })
        .and_then(|mut c| c.wait());
    if let Some((mut t, thread)) = reader {
        let _ = t.kill();
        let _ = t.wait();
        let _ = thread.join();
    }
    let out = std::fs::read(log).unwrap_or_default();
    match status {
        Ok(st) if st.success() => Answer::Ok,
        Ok(_) => Answer::Err {
            tail: compose::tail(out.get(from as usize..).unwrap_or(&out)),
        },
        Err(e) => fail(e),
    }
}

/// The step a compose `up` line shows: ` Image x Pulling`, ` Image x Building`, `#6 [2/3] RUN ...`.
fn progress_step(line: &str) -> Option<String> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("Image ") {
        let (image, verb) = rest.rsplit_once(' ')?;
        return match verb {
            "Pulling" => Some(format!("pulling {image}")),
            "Building" => Some("building".into()),
            _ => None,
        };
    }
    let rest = line.strip_prefix('#')?.split_once(' ')?.1;
    // `[2/3]`, or `[backend 2/3]` when compose builds several images
    let step = rest
        .strip_prefix('[')?
        .split_once(']')?
        .0
        .rsplit(' ')
        .next()?;
    let (n, total) = step.split_once('/')?;
    (n.parse::<u32>().is_ok() && total.parse::<u32>().is_ok())
        .then(|| format!("building: step {n}/{total}"))
}

fn exec(mut cmd: Command) -> Answer {
    match cmd.stdin(Stdio::null()).output() {
        Ok(o) if o.status.success() => Answer::Ok,
        Ok(o) => Answer::Err {
            tail: compose::tail(&o.stderr),
        },
        Err(e) => Answer::Err {
            tail: format!("cannot run docker compose: {e}"),
        },
    }
}

/// The container of a node that all instances of the project share, when it exists.
pub fn shared_container(stack: &Stack, node: &str) -> Option<String> {
    let info = crate::postgres::Info::load(&stack.dir);
    let node = info.databases.get(node).map_or(node, |d| d.server.as_str());
    let out = Command::new("docker")
        .args(["ps", "-aq", "--filter", "label=rizr.scope=project"])
        .arg(format!("--filter=label=rizr.project={}", stack.project.0))
        .arg(format!("--filter=label=rizr.node={node}"))
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(str::to_owned)
}

/// The real command of an effect, for `plan` and `eject`. `None`: rizr does it itself (timers, prompts).
pub fn render(stack: &Stack, s: &State, op: &Op) -> Option<String> {
    let db = |n: &NodeId| {
        n.scope != Scope::Project
            && s.loaded()
                .and_then(|m| m.nodes.get(n))
                .is_some_and(|x| x.provider.0 == crate::postgres::PROVIDER)
    };
    let base = |c: Command| {
        std::iter::once(c.get_program())
            .chain(c.get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    };
    Some(match op {
        Op::Import => format!("{} config --format json", base(stack.compose())),
        Op::Start { node, .. } if db(node) => format!(
            "docker exec <server> createdb <database>; docker network connect --alias {} <network> <server>{}",
            node.name.0,
            if stack.instance.0 == "main" {
                ""
            } else {
                "; a new database gets a copy of main's data: main's database on the server, else main's running compose container, else main's volume"
            }
        ),
        Op::Stop(node) if db(node) => format!(
            "docker network disconnect <network> <server> ({})",
            node.name.0
        ),
        Op::Start { node, .. } if node.scope == Scope::Project => format!(
            "docker run -d --name rizr-{0}-{1} --volume rizr-{0}-{1}:<data> --publish 127.0.0.1::5432 <image>",
            stack.project.0, node.name.0
        ),
        Op::Stop(node) if node.scope == Scope::Project => {
            format!("docker rm --force rizr-{}-{}", stack.project.0, node.name.0)
        }
        Op::Start { node, .. } => format!(
            "{} -f {} up -d --build --no-deps {}",
            base(stack.compose()),
            stack
                .dir
                .join(format!("override-{}.yml", node.name.0))
                .display(),
            node.name.0
        ),
        Op::Stop(node) => format!(
            "{} rm --stop --force {}",
            base(stack.compose()),
            node.name.0
        ),
        Op::Subscribe => format!(
            "docker events --filter label=rizr.project={}",
            stack.project.0
        ),
        Op::Observe => format!(
            "docker ps -aq --filter label=rizr.project={}",
            stack.project.0
        ),
        Op::Inspect(n) | Op::Probe(n) => format!("docker inspect <{} container>", n.name.0),
        _ => return None,
    })
}

/// The services that instance `a` runs now: its compose containers, and its databases on a running shared server.
fn running_services(project: &str, a: &Attach) -> std::collections::BTreeSet<String> {
    let ps = |filter: String, format: &str| -> Vec<String> {
        Command::new("docker")
            .args(["ps", "--format", format, "--filter", &filter])
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let servers = ps(format!("name=^rizr-{project}-"), "{{.Names}}");
    let mut v: std::collections::BTreeSet<String> = ps(
        format!("label=com.docker.compose.project={}", a.compose_name),
        "{{.Label \"com.docker.compose.service\"}}",
    )
    .into_iter()
    .collect();
    v.extend(
        crate::postgres::Info::load(&a.dir)
            .databases
            .into_iter()
            .filter(|(_, d)| servers.contains(&format!("rizr-{project}-{}", d.server)))
            .map(|(n, _)| n),
    );
    v
}

/// The compose import (or an empty model for a repo with only `rizr.toml` or an attached instance), with the
/// host services merged in.
#[allow(clippy::type_complexity, reason = "one call site")]
fn import(
    root: &Path,
    files: &[PathBuf],
    name: &str,
    dir: &Path,
    instance: &InstanceId,
    attach: Option<&(String, std::collections::BTreeSet<String>)>,
    inputs: Digest,
) -> Result<
    (
        rizr_core::model::Model,
        BTreeMap<String, compose::Side>,
        BTreeMap<String, crate::manifest::HostService>,
        crate::postgres::Resources,
    ),
    String,
> {
    let (mut m, side) = if attach.is_some()
        || (compose::resolve(root, files).is_empty() && root.join("rizr.toml").is_file())
    {
        (
            rizr_core::model::Model {
                inputs,
                nodes: BTreeMap::new(),
                findings: Vec::new(),
            },
            BTreeMap::new(),
        )
    } else {
        compose::import(root, files, name, dir, instance, inputs)?
    };
    let external = attach.map(|(target, r)| (target.as_str(), r));
    let host = crate::manifest::merge(root, instance, external, &mut m)?;
    let pg = crate::postgres::add(&mut m, &side, &crate::manifest::unshared(root), instance)?;
    pg.info.save(dir)?;
    if let Some(n) = compose::cycle(&m) {
        return Err(format!("dependency cycle through `{}`", n.name.0));
    }
    Ok((m, side, host, pg))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test panics on a broken precondition")]
mod tests {
    use super::*;

    #[test]
    fn a_die_from_a_replaced_container_is_dropped() {
        let current = Current::default();
        let inst = InstanceId("main".into());
        let ev = |action: &str, id: &str| {
            json!({"Action": action, "Actor": {"ID": id, "Attributes":
                {"rizr.node": "db", "rizr.instance": "main", "rizr.spec": "s", "exitCode": "1"}},
                "timeNano": 5})
            .to_string()
        };
        current.lock().unwrap().insert("db".into(), "old".into());
        // A Start effect clears the id; the old container's die then arrives late.
        current.lock().unwrap().remove("db");
        assert!(runtime_event(&ev("die", "old"), &inst, &current).is_none());
        assert!(runtime_event(&ev("start", "new"), &inst, &current).is_some());
        assert!(runtime_event(&ev("die", "old"), &inst, &current).is_none());
        assert!(matches!(
            runtime_event(&ev("die", "new"), &inst, &current),
            Some(Event::Runtime {
                kind: RuntimeKind::Die { code: 1 },
                ..
            })
        ));
    }
}
