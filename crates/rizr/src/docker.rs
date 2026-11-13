//! Docker provider and the shell loop: runs core effects, turns `docker events` and inspect output into core
//! events. The core decides; this file only renders and runs (SPEC-core section 6).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
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

use crate::compose;

enum Msg {
    Done(EffectId, Answer),
    Event(Event),
}

/// The current container id per service. A `die`, health or destroy event from another id is stale: it
/// belongs to a container that a later `Start` replaced (DECISIONS 2026-11-13, step 2 spike).
type Current = Arc<Mutex<BTreeMap<String, String>>>;

pub struct Stack {
    pub root: PathBuf,
    /// Compose files given with `-f`; empty means the default names.
    pub files: Vec<PathBuf>,
    pub project: ProjectId,
    pub instance: InstanceId,
    /// rizr's data directory for this instance: generation and override files. Never inside the repo.
    pub dir: PathBuf,
    compose_name: String,
}

impl Stack {
    /// The project is the repo, shared by all its git worktrees; the instance is `main` for the main worktree,
    /// else the worktree's directory name.
    pub fn new(root: PathBuf, files: Vec<PathBuf>, data: &Path) -> Self {
        let slug = |p: &Path| -> String {
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
        };
        let main = main_worktree(&root);
        let project = ProjectId(slug(main.as_deref().unwrap_or(&root)));
        let instance = InstanceId(main.as_ref().map_or("main".into(), |_| slug(&root)));
        let dir = data.join(format!("{}-{}", project.0, instance.0));
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
        }
    }

    /// Hold this instance's lock until the file drops: one mutating rizr call per instance at a time. The OS
    /// releases it when the process dies, so a kill -9 leaves no stale lock.
    pub fn lock(&self) -> std::io::Result<std::fs::File> {
        std::fs::create_dir_all(&self.dir)?;
        let f = std::fs::File::create(self.dir.join("lock"))?;
        f.lock()?;
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
        units: crate::supervisor::Units::of(stack),
        unit_events: None,
        project_lock: Arc::default(),
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
    units: crate::supervisor::Units,
    unit_events: Option<Child>,
    /// Held while this instance changes a node that all instances share; the OS frees it if rizr dies.
    project_lock: Arc<Mutex<Option<std::fs::File>>>,
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
            let next = self
                .deadlines
                .iter()
                .min_by_key(|(_, t)| **t)
                .map(|(id, t)| (*id, *t));
            let msg = match next {
                Some((id, at)) => {
                    match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                        Err(RecvTimeoutError::Timeout) => {
                            self.deadlines.remove(&id);
                            s = reduce(s, Event::DeadlineHit(id));
                            continue;
                        }
                        m => m.ok(),
                    }
                }
                None => rx.recv().ok(),
            };
            match msg {
                Some(Msg::Done(id, answer)) => s = reduce(s, Event::Done { id, answer }),
                Some(Msg::Event(e)) => s = reduce(s, e),
                None => unreachable!("the provider holds a sender"),
            }
        }
    }

    fn dispatch(&mut self, s: &State, id: EffectId, op: Op) {
        let tx = self.tx.clone();
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
                std::thread::spawn(move || {
                    done(
                        match observe(&labels, &instance, &current).and_then(|mut w| {
                            w.seen.extend(crate::supervisor::observe(&units)?);
                            Ok(w)
                        }) {
                            Ok(w) => Answer::Observed(w),
                            Err(tail) => Answer::Err { tail },
                        },
                    );
                });
            }
            Op::Import => {
                let (root, files, name, dir, instance, side) = (
                    st.root.clone(),
                    st.files.clone(),
                    st.compose_name(),
                    st.dir.clone(),
                    st.instance.clone(),
                    self.side.clone(),
                );
                let host = self.host.clone();
                let inputs = match &s.trust {
                    rizr_core::state::Trust::Trusted(d) => d.clone(),
                    _ => Digest(String::new()),
                };
                std::thread::spawn(move || {
                    done(
                        match import(&root, &files, &name, &dir, &instance, inputs) {
                            Ok((m, imported, services)) => {
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
                let svc = self
                    .host
                    .lock()
                    .ok()
                    .and_then(|h| h.get(&node.name.0).cloned());
                let units = self.units.clone();
                std::thread::spawn(move || {
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
            Op::Start {
                node,
                spec,
                generation,
            } => {
                if let Ok(mut c) = self.current.lock() {
                    c.remove(&node.name.0);
                }
                let mut cmd = self.stack.compose();
                let r = self.write_override(&node, &spec, generation).map(|files| {
                    for f in files {
                        cmd.arg("-f").arg(f);
                    }
                    cmd.args(["up", "-d", "--no-deps"]).arg(&node.name.0);
                });
                std::thread::spawn(move || {
                    done(match r {
                        Ok(()) => exec(cmd),
                        Err(tail) => Answer::Err { tail },
                    });
                });
            }
            Op::Stop(node) if self.is_host(&node.name) => {
                let cmd = crate::supervisor::stop(&self.units, &node.name.0);
                std::thread::spawn(move || done(exec(cmd)));
            }
            Op::Stop(node) => {
                let mut cmd = self.stack.compose();
                cmd.args(["rm", "--stop", "--force"]).arg(&node.name.0);
                std::thread::spawn(move || done(exec(cmd)));
            }
        }
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
    let Some(ip) = ip else { return false };
    !ports.is_empty()
        && ports.iter().all(|p| {
            TcpStream::connect_timeout(&SocketAddr::new(ip, *p), Duration::from_millis(400)).is_ok()
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

/// The real command of an effect, for `plan` and `eject`. `None`: rizr does it itself (timers, prompts).
pub fn render(stack: &Stack, op: &Op) -> Option<String> {
    let base = |c: Command| {
        std::iter::once(c.get_program())
            .chain(c.get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    };
    Some(match op {
        Op::Import => format!("{} config --format json", base(stack.compose())),
        Op::Start { node, .. } => format!(
            "{} -f {} up -d --no-deps {}",
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

/// The compose import (or an empty model for a repo with only `rizr.toml`), with the host services merged in.
#[allow(clippy::type_complexity, reason = "one call site")]
fn import(
    root: &Path,
    files: &[PathBuf],
    name: &str,
    dir: &Path,
    instance: &InstanceId,
    inputs: Digest,
) -> Result<
    (
        rizr_core::model::Model,
        BTreeMap<String, compose::Side>,
        BTreeMap<String, crate::manifest::HostService>,
    ),
    String,
> {
    let (mut m, side) =
        if compose::resolve(root, files).is_empty() && root.join("rizr.toml").is_file() {
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
    let host = crate::manifest::merge(root, instance, &mut m)?;
    if let Some(n) = compose::cycle(&m) {
        return Err(format!("dependency cycle through `{}`", n.name.0));
    }
    Ok((m, side, host))
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
