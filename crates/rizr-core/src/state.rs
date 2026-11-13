//! `State` and `reduce`, the only place where state changes (SPEC-core sections 3 and 4).

use std::collections::BTreeMap;

use crate::event::{
    Action, Answer, DeadlineKind, Effect, Event, Op, RuntimeKind, Status, Wait, World,
};
use crate::machine::{Ctx, Health, Input, Job, Machine, ProbeTurn, Service};
use crate::model::{
    Digest, EffectId, Gen, InstanceId, JobKind, Model, NodeId, NodeKind, NodeSpec, Probe,
    ProjectId, Scope,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trust {
    Unhashed,
    Untrusted(Digest),
    Refused(Digest),
    Trusted(Digest),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sync {
    Unsubscribed,
    Subscribing,
    Subscribed,
    Observing,
    Synced,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelState {
    Absent,
    Loaded(Model),
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lock {
    Free,
    Wanted,
    Held,
}

/// D0 holds only `Steady`; transitions come with snapshot and restore in D1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataAxis {
    Steady(String),
    Switching { from: String, to: String },
}

/// `Keep` = observe only (`status`, `why`); no node effect is planned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Keep,
    Up,
    Down,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intents {
    pub target: Target,
    pub generation: Gen,
    pub wait: Wait,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// `None` for a node that the runtime holds but the model no longer has.
    pub spec: Option<NodeSpec>,
    pub machine: Machine,
    /// Spec digest of the container this machine tracks; events with another digest are ignored.
    pub live: Option<Digest>,
    pub running: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deadline {
    pub node: NodeId,
    pub kind: DeadlineKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub project: ProjectId,
    pub instance: InstanceId,
    pub trust: Trust,
    pub sync: Sync,
    pub model: ModelState,
    pub intents: Intents,
    pub nodes: BTreeMap<NodeId, Node>,
    pub data: BTreeMap<InstanceId, DataAxis>,
    pub lock: Lock,
    pub pending: BTreeMap<EffectId, Effect>,
    pub deadlines: BTreeMap<EffectId, Deadline>,
    pub next: u64,
    /// Runtime events that arrive between `Subscribe` and the `Observe` answer; replayed after it.
    pub buffer: Vec<Event>,
    /// Inputs that did not fit the state.
    pub rejected: Vec<String>,
}

impl State {
    /// `generation` comes from the intents file, so it survives a crash.
    pub fn new(project: ProjectId, instance: InstanceId, generation: Gen) -> Self {
        Self {
            project,
            data: BTreeMap::from([(instance.clone(), DataAxis::Steady(instance.0.clone()))]),
            instance,
            trust: Trust::Unhashed,
            sync: Sync::Unsubscribed,
            model: ModelState::Absent,
            intents: Intents {
                target: Target::Keep,
                generation,
                wait: Wait::Ready,
            },
            nodes: BTreeMap::new(),
            lock: Lock::Free,
            pending: BTreeMap::new(),
            deadlines: BTreeMap::new(),
            next: 0,
            buffer: Vec::new(),
            rejected: Vec::new(),
        }
    }

    pub fn trusted(&self) -> bool {
        matches!(self.trust, Trust::Trusted(_))
    }

    pub fn loaded(&self) -> Option<&Model> {
        match &self.model {
            ModelState::Loaded(m) => Some(m),
            _ => None,
        }
    }

    pub fn pending_for(&self, node: &NodeId) -> impl Iterator<Item = &Op> {
        self.pending
            .values()
            .map(|e| &e.op)
            .filter(move |op| op.node() == Some(node))
    }

    pub fn has_deadline(&self, node: &NodeId, kind: &DeadlineKind) -> bool {
        self.deadlines
            .values()
            .any(|d| &d.node == node && &d.kind == kind)
    }

    fn reject(&mut self, what: String) {
        self.rejected.push(what);
    }

    fn step(&mut self, id: &NodeId, input: Input) {
        let generation = self.intents.generation;
        let Some(node) = self.nodes.get_mut(id) else {
            self.reject(format!("{input:?} for unknown node {}", id.name.0));
            return;
        };
        let ready_on_start = matches!(
            node.spec.as_ref().map(|s| &s.kind),
            Some(NodeKind::Service {
                probe: Probe::AtStart
            }) | None
        );
        match node.machine.step(
            &input,
            Ctx {
                ready_on_start,
                generation,
            },
        ) {
            Ok(m) => node.machine = m,
            Err(_) => {
                let what = format!("{input:?} in {} of {}", node.machine.label(), id.name.0);
                self.reject(what);
                return;
            }
        }
        if let (Machine::Job(Job::Done(_)), Some(spec)) = (&node.machine, &node.spec)
            && node.live.as_ref() != Some(&spec.spec)
        {
            node.machine = Machine::Job(Job::Pending);
        }
        self.prune(id);
    }

    /// Drops the deadlines that no longer fit the node's state.
    fn prune(&mut self, id: &NodeId) {
        let machine = self.nodes.get(id).map(|n| n.machine.clone());
        self.deadlines.retain(|_, d| {
            &d.node != id
                || matches!(
                    (&d.kind, &machine),
                    (
                        DeadlineKind::StartTimeout,
                        Some(Machine::Service(Service::Starting(_)))
                    ) | (
                        DeadlineKind::ProbeRetry,
                        Some(Machine::Service(Service::Starting(ProbeTurn::Wait)))
                    )
                )
        });
    }
}

fn fresh(spec: &NodeSpec) -> Machine {
    match spec.kind {
        NodeKind::Service { .. } => Machine::Service(Service::Absent),
        NodeKind::Job { .. } => Machine::Job(Job::Pending),
    }
}

pub fn reduce(mut s: State, event: Event) -> State {
    match event {
        Event::Hashed { inputs, trusted } => {
            s.trust = if trusted {
                Trust::Trusted(inputs)
            } else {
                Trust::Untrusted(inputs)
            };
        }
        Event::Action(a) => action(&mut s, a),
        Event::Sent(e) => sent(&mut s, e),
        Event::Done { id, answer } => done(&mut s, id, answer),
        Event::Runtime {
            node,
            spec,
            kind,
            at,
        } => match s.sync {
            Sync::Synced => runtime(&mut s, node, spec, kind),
            Sync::Subscribed | Sync::Observing => s.buffer.push(Event::Runtime {
                node,
                spec,
                kind,
                at,
            }),
            _ => {}
        },
        Event::StreamLost => {
            s.sync = Sync::Unsubscribed;
            s.buffer.clear();
            s.pending
                .retain(|_, e| !matches!(e.op, Op::Subscribe | Op::Observe));
        }
        Event::DeadlineHit(id) => {
            if let Some(d) = s.deadlines.remove(&id) {
                let input = match d.kind {
                    DeadlineKind::StartTimeout => Input::StartTimeout,
                    DeadlineKind::ProbeRetry => Input::RetryDue,
                };
                s.step(&d.node, input);
            }
        }
    }
    s
}

fn action(s: &mut State, a: Action) {
    match a {
        Action::Up { wait } => {
            s.intents = Intents {
                target: Target::Up,
                generation: Gen(s.intents.generation.0 + 1),
                wait,
            };
            if let Trust::Refused(i) = &s.trust {
                s.trust = Trust::Untrusted(i.clone());
            }
            if matches!(s.sync, Sync::Failed(_)) {
                s.sync = Sync::Unsubscribed;
            }
            if matches!(s.model, ModelState::Failed(_)) {
                s.model = ModelState::Absent;
            }
            let ids: Vec<NodeId> = s.nodes.keys().cloned().collect();
            for id in ids {
                let n = &s.nodes[&id];
                let reset = matches!(
                    (&n.machine, n.spec.as_ref().map(|x| &x.kind)),
                    (
                        Machine::Service(Service::Failed(_)) | Machine::Job(Job::Failed(_)),
                        _
                    ) | (
                        Machine::Job(Job::Done(_)),
                        Some(NodeKind::Job {
                            kind: JobKind::Migrate
                        })
                    )
                );
                if reset {
                    s.step(&id, Input::Reset);
                }
            }
        }
        Action::Down => s.intents.target = Target::Down,
        Action::Restart { node } => {
            let id = [Scope::Instance(s.instance.clone()), Scope::Project]
                .into_iter()
                .map(|scope| NodeId {
                    scope,
                    name: node.clone(),
                })
                .find(|id| s.nodes.contains_key(id));
            match id {
                Some(id) => {
                    s.intents.target = Target::Up;
                    s.step(&id, Input::Reset);
                }
                None => s.reject(format!("restart of unknown node {}", node.0)),
            }
        }
        Action::Trust => {
            if let Trust::Untrusted(i) | Trust::Refused(i) = &s.trust {
                s.trust = Trust::Trusted(i.clone());
            }
        }
    }
}

fn sent(s: &mut State, e: Effect) {
    s.next = s.next.max(e.id.0 + 1);
    match &e.op {
        Op::SetDeadline { node, kind, .. } => {
            s.deadlines.insert(
                e.id,
                Deadline {
                    node: node.clone(),
                    kind: kind.clone(),
                },
            );
            return;
        }
        Op::ReleaseProjectLock => {
            s.lock = Lock::Free;
            return;
        }
        Op::AcquireProjectLock => s.lock = Lock::Wanted,
        Op::Subscribe => s.sync = Sync::Subscribing,
        Op::Observe => s.sync = Sync::Observing,
        Op::Start { node, spec, .. } => {
            if let Some(n) = s.nodes.get_mut(node) {
                n.live = Some(spec.clone());
                n.running = false;
            }
            let node = node.clone();
            s.step(&node, Input::StartSent);
        }
        Op::Stop(node) => {
            let node = node.clone();
            s.step(&node, Input::StopSent);
        }
        Op::PromptTrust { .. } | Op::Import | Op::Inspect(_) | Op::Probe(_) => {}
    }
    s.pending.insert(e.id, e);
}

fn done(s: &mut State, id: EffectId, answer: Answer) {
    let Some(e) = s.pending.remove(&id) else {
        s.reject(format!("answer for unknown effect {}", id.0));
        return;
    };
    match (e.op, answer) {
        (Op::AcquireProjectLock, Answer::Ok) => s.lock = Lock::Held,
        (Op::Subscribe, Answer::Ok) => s.sync = Sync::Subscribed,
        (Op::Subscribe | Op::Observe, Answer::Err { tail }) => s.sync = Sync::Failed(tail),
        (Op::Observe, Answer::Observed(world)) => {
            rebuild(s, &world);
            s.sync = Sync::Synced;
            for ev in std::mem::take(&mut s.buffer) {
                if let Event::Runtime {
                    node,
                    spec,
                    kind,
                    at,
                } = ev
                    && at > world.at
                {
                    runtime(s, node, spec, kind);
                }
            }
        }
        (Op::PromptTrust { inputs }, Answer::Trust { granted }) => {
            if s.trust == Trust::Untrusted(inputs.clone()) {
                s.trust = if granted {
                    Trust::Trusted(inputs)
                } else {
                    Trust::Refused(inputs)
                };
            }
        }
        (Op::Import, Answer::Imported(model)) => load(s, model),
        (Op::Import, Answer::Err { tail }) => s.model = ModelState::Failed(tail),
        (Op::Inspect(n), Answer::Inspected { restarting }) => {
            s.step(&n, Input::Inspected { restarting })
        }
        (Op::Start { node, .. }, Answer::Ok) => {
            if let Some(n) = s.nodes.get_mut(&node) {
                n.running = true;
            }
            s.step(&node, Input::Started);
        }
        (Op::Start { node, .. }, Answer::Err { tail }) => s.step(&node, Input::StartFailed(tail)),
        (Op::Stop(node), Answer::Ok) => {
            s.step(&node, Input::Destroyed);
            // Only an accepted stop clears `live`: a resync may have moved the node on meanwhile.
            if let Some(n) = s.nodes.get_mut(&node)
                && matches!(
                    n.machine,
                    Machine::Service(Service::Absent) | Machine::Job(Job::Pending)
                )
            {
                n.live = None;
                n.running = false;
            }
        }
        (Op::Probe(node), Answer::Probe { ok }) => s.step(&node, Input::ProbeResult(ok)),
        (Op::AcquireProjectLock, Answer::Err { tail }) => {
            s.lock = Lock::Free;
            s.reject(format!("project lock: {tail}"));
        }
        (op, answer) => s.reject(format!("{answer:?} for {op:?}")),
    }
}

fn runtime(s: &mut State, node: NodeId, spec: Digest, kind: RuntimeKind) {
    let Some(n) = s.nodes.get_mut(&node) else {
        return;
    };
    if n.live.as_ref() != Some(&spec) {
        return;
    }
    let input = match kind {
        RuntimeKind::Start => {
            n.running = true;
            Input::Started
        }
        RuntimeKind::Die { code } => {
            n.running = false;
            Input::Died { code }
        }
        RuntimeKind::Destroy => {
            n.running = false;
            Input::Destroyed
        }
        RuntimeKind::Health(h) => Input::Health(h),
    };
    s.step(&node, input);
}

/// Loads an imported model: new nodes start fresh, changed specs move their nodes on, removed nodes lose
/// their spec so `plan` stops them.
fn load(s: &mut State, model: Model) {
    for (id, spec) in &model.nodes {
        let changed = match s.nodes.get_mut(id) {
            None => {
                let machine = fresh(spec);
                s.nodes.insert(
                    id.clone(),
                    Node {
                        spec: Some(spec.clone()),
                        machine,
                        live: None,
                        running: false,
                    },
                );
                false
            }
            Some(n) => {
                let same_kind = matches!(
                    (&n.machine, &spec.kind),
                    (Machine::Service(_), NodeKind::Service { .. })
                        | (Machine::Job(_), NodeKind::Job { .. })
                );
                if !same_kind && !n.machine.is_waiting() {
                    n.machine = fresh(spec);
                }
                n.spec = Some(spec.clone());
                n.live.as_ref().is_some_and(|l| l != &spec.spec)
            }
        };
        if changed {
            s.step(id, Input::SpecChanged);
        }
    }
    for (id, n) in s.nodes.iter_mut() {
        if !model.nodes.contains_key(id) {
            n.spec = None;
        }
    }
    s.model = ModelState::Loaded(model);
}

/// Rebuilds every machine from what the runtime holds (SPEC-core section 4.4).
fn rebuild(s: &mut State, world: &World) {
    let generation = s.intents.generation;
    for seen in &world.seen {
        s.nodes.entry(seen.node.clone()).or_insert_with(|| Node {
            spec: None,
            machine: Machine::Service(Service::Absent),
            live: None,
            running: false,
        });
    }
    let ids: Vec<NodeId> = s.nodes.keys().cloned().collect();
    for id in ids {
        let seen = world.seen.iter().find(|x| x.node == id);
        let Some(n) = s.nodes.get_mut(&id) else {
            continue;
        };
        let spec = n.spec.clone();
        n.live = seen.map(|x| x.spec.clone());
        n.running = matches!(seen.map(|x| &x.status), Some(Status::Running { .. }));
        let outdated = matches!((&spec, seen), (Some(sp), Some(x)) if sp.spec != x.spec);
        n.machine = match (spec.as_ref().map(|x| &x.kind), seen) {
            (Some(NodeKind::Job { .. }), None) => Machine::Job(Job::Pending),
            (Some(NodeKind::Job { .. }), Some(x)) => Machine::Job(match x.status {
                Status::Created => Job::Pending,
                Status::Running { .. } | Status::Restarting => Job::Running,
                Status::Exited { code: 0 } if !outdated => Job::Done(x.generation.min(generation)),
                Status::Exited { code: 0 } => Job::Pending,
                Status::Exited { code } => Job::Failed(crate::machine::Failure::Exit { code }),
            }),
            (_, None) => Machine::Service(Service::Absent),
            (kind, Some(x)) => Machine::Service(match &x.status {
                Status::Created => Service::Absent,
                Status::Exited {
                    code: 0 | 137 | 143,
                } => Service::Absent,
                Status::Exited { code } => {
                    Service::Failed(crate::machine::Failure::Exit { code: *code })
                }
                _ if outdated => Service::Outdated,
                Status::Restarting => Service::Restarting,
                Status::Running {
                    health: Some(Health::Healthy),
                } => Service::Ready,
                Status::Running {
                    health: Some(Health::Unhealthy),
                } => Service::Unhealthy,
                Status::Running {
                    health: Some(Health::Starting),
                } => Service::Starting(ProbeTurn::Due),
                Status::Running { health: None } => match kind {
                    Some(NodeKind::Service {
                        probe: Probe::AtStart,
                    })
                    | None => Service::Ready,
                    _ => Service::Starting(ProbeTurn::Due),
                },
            }),
        };
        s.prune(&id);
    }
}
