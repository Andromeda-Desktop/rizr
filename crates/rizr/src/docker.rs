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
}

impl Stack {
    pub fn new(root: PathBuf, files: Vec<PathBuf>, data: &Path) -> Self {
        let name: String = root
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
        let project = ProjectId(if name.is_empty() { "root".into() } else { name });
        let instance = InstanceId("main".into());
        let dir = data.join(format!("{}-{}", project.0, instance.0));
        Self {
            root,
            files,
            project,
            instance,
            dir,
        }
    }

    /// The compose project name: `<project>-<instance>` (SPEC-core section 2).
    pub fn compose_name(&self) -> String {
        format!("{}-{}", self.project.0, self.instance.0)
    }

    pub fn compose(&self) -> Command {
        compose::command(&self.root, &self.files, &self.compose_name(), &self.dir)
    }
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
        restart_set: Arc::default(),
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
    /// Services whose compose file sets `restart:`; the others get `on-failure:5` in the override.
    restart_set: Arc<Mutex<BTreeMap<String, bool>>>,
}

impl Drop for Provider<'_> {
    fn drop(&mut self) {
        if let Some(c) = &mut self.events {
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
            Op::AcquireProjectLock => done(Answer::Err {
                tail: "project-scope nodes come in D1".into(),
            }),
            Op::ReleaseProjectLock => {}
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
                let (labels, current) = (self.labels(), self.current.clone());
                std::thread::spawn(move || {
                    done(match observe(&labels, &current) {
                        Ok(w) => Answer::Observed(w),
                        Err(tail) => Answer::Err { tail },
                    });
                });
            }
            Op::Import => {
                let (root, files, name, dir, instance, restart_set) = (
                    st.root.clone(),
                    st.files.clone(),
                    st.compose_name(),
                    st.dir.clone(),
                    st.instance.clone(),
                    self.restart_set.clone(),
                );
                let inputs = match &s.trust {
                    rizr_core::state::Trust::Trusted(d) => d.clone(),
                    _ => Digest(String::new()),
                };
                std::thread::spawn(move || {
                    done(
                        match compose::import(&root, &files, &name, &dir, &instance, inputs) {
                            Ok((m, restart)) => {
                                if let Ok(mut r) = restart_set.lock() {
                                    *r = restart;
                                }
                                Answer::Imported(m)
                            }
                            Err(tail) => Answer::Err { tail },
                        },
                    );
                });
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
            Op::Probe(node) => {
                let ports = match s.loaded().and_then(|m| m.nodes.get(&node)).map(|n| &n.kind) {
                    Some(NodeKind::Service {
                        probe: Probe::Tcp { ports },
                    }) => ports.clone(),
                    _ => vec![],
                };
                let cid = self.container(&node.name);
                std::thread::spawn(move || {
                    done(Answer::Probe {
                        ok: probe(cid, &ports),
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
                let r = self.write_override(&node, &spec, generation).map(|f| {
                    cmd.arg("-f")
                        .arg(f)
                        .args(["up", "-d", "--no-deps"])
                        .arg(&node.name.0);
                });
                std::thread::spawn(move || {
                    done(match r {
                        Ok(()) => exec(cmd),
                        Err(tail) => Answer::Err { tail },
                    });
                });
            }
            Op::Stop(node) => {
                let mut cmd = self.stack.compose();
                cmd.args(["rm", "--stop", "--force"]).arg(&node.name.0);
                std::thread::spawn(move || done(exec(cmd)));
            }
        }
    }

    fn labels(&self) -> [String; 2] {
        [
            format!("rizr.project={}", self.stack.project.0),
            format!("rizr.instance={}", self.stack.instance.0),
        ]
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
    ) -> Result<PathBuf, String> {
        let st = self.stack;
        let mut svc = json!({"labels": {
            "rizr.project": st.project.0, "rizr.instance": st.instance.0, "rizr.node": node.name.0,
            "rizr.spec": spec.0, "rizr.gen": generation.0.to_string()}});
        let restart_set = self
            .restart_set
            .lock()
            .ok()
            .and_then(|r| r.get(&node.name.0).copied())
            .unwrap_or(true);
        if !restart_set {
            svc["restart"] = json!("on-failure:5");
        }
        let path = st.dir.join(format!("override-{}.yml", node.name.0));
        std::fs::create_dir_all(&st.dir).map_err(|e| e.to_string())?;
        // JSON is YAML: compose reads it, and no value needs YAML quoting.
        std::fs::write(&path, json!({"services": {&node.name.0: svc}}).to_string())
            .map_err(|e| e.to_string())?;
        Ok(path)
    }

    fn subscribe(&mut self) -> Result<(), String> {
        if let Some(mut c) = self.events.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
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
        node: node_id(instance, node),
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

fn node_id(instance: &InstanceId, name: &str) -> NodeId {
    NodeId {
        scope: Scope::Instance(instance.clone()),
        name: NodeName(name.into()),
    }
}

fn observe(labels: &[String; 2], current: &Current) -> Result<World, String> {
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
        let (Some(node), Some(inst), Some(spec)) = (
            l["rizr.node"].as_str(),
            l["rizr.instance"].as_str(),
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
            node: node_id(&InstanceId(inst.into()), node),
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
            "docker events --filter label=rizr.project={} --filter label=rizr.instance={}",
            stack.project.0, stack.instance.0
        ),
        Op::Observe => format!(
            "docker ps -aq --filter label=rizr.project={}",
            stack.project.0
        ),
        Op::Inspect(n) | Op::Probe(n) => format!("docker inspect <{} container>", n.name.0),
        _ => return None,
    })
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
                {"rizr.node": "db", "rizr.spec": "s", "exitCode": "1"}}, "timeNano": 5})
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
