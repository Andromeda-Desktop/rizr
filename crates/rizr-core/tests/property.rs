//! Random event sequences from a fake runtime through `reduce` and `plan`; the invariants of SPEC-core
//! section 9 hold after every step, and the loop converges once faults stop (invariant 8).

#![allow(clippy::expect_used, reason = "a test panics on a broken precondition")]

use std::collections::{BTreeMap, VecDeque};

use proptest::prelude::*;
use rizr_core::event::{Answer, Event, Op, RuntimeKind, Seen, Status, Wait, World};
use rizr_core::invariants::check;
use rizr_core::machine::{Health, Machine, Service};
use rizr_core::model::{
    Dep, Digest, EffectId, Gate, Gen, InstanceId, JobKind, Model, NodeId, NodeKind, NodeName,
    NodeSpec, Probe, ProjectId, ProviderName, Scope,
};
use rizr_core::state::Target;
use rizr_core::view::outcome;
use rizr_core::{Action, Effect, State, plan, reduce};

#[derive(Clone, Debug)]
struct NodeGen {
    project: bool,
    kind: u8,
    deps: Vec<(usize, u8)>,
}

#[derive(Clone, Debug)]
enum Choice {
    Answer(usize, bool),
    Deliver(usize),
    Fire(usize),
    Crash(usize),
    Health(usize, bool),
    Lost,
    Up,
    Down,
    Restart(usize),
    Edit(usize),
}

#[derive(Clone, Debug)]
struct Container {
    spec: Digest,
    generation: Gen,
    status: Status,
    restarts_left: u8,
}

#[derive(Clone, Debug)]
enum Note {
    Event(Event),
    /// The runtime restart policy brings the container back when this note is delivered.
    Restarted(NodeId, Digest),
}

struct Fake {
    gens: Vec<NodeGen>,
    version: Vec<u32>,
    edits: u32,
    containers: BTreeMap<NodeId, Container>,
    notes: VecDeque<Note>,
    clock: u64,
    /// `Observe` answers hold the world as it was when the effect was sent, so later changes race it.
    snapshots: BTreeMap<EffectId, World>,
}

fn id(g: &NodeGen, i: usize) -> NodeId {
    let scope = if g.project {
        Scope::Project
    } else {
        Scope::Instance(InstanceId::from("main"))
    };
    NodeId {
        scope,
        name: NodeName(format!("n{i}")),
    }
}

impl Fake {
    fn new(gens: Vec<NodeGen>) -> Self {
        let version = vec![0; gens.len()];
        Self {
            gens,
            version,
            edits: 0,
            containers: BTreeMap::new(),
            notes: VecDeque::new(),
            clock: 0,
            snapshots: BTreeMap::new(),
        }
    }

    fn id(&self, i: usize) -> NodeId {
        id(&self.gens[i], i)
    }

    fn inputs(&self) -> Digest {
        Digest(format!("in{}", self.edits))
    }

    fn probe(&self, i: usize) -> Option<Probe> {
        match self.gens[i].kind {
            0 => Some(Probe::AtStart),
            1 => Some(Probe::HealthEvent),
            2 => Some(Probe::Tcp { ports: vec![80] }),
            _ => None,
        }
    }

    fn model(&self) -> Model {
        let nodes = (0..self.gens.len())
            .map(|i| {
                let kind = match self.probe(i) {
                    Some(probe) => NodeKind::Service { probe },
                    None => NodeKind::Job {
                        kind: JobKind::Migrate,
                    },
                };
                let deps = if i == 0 {
                    Vec::new()
                } else {
                    let mut deps: Vec<Dep> = self.gens[i]
                        .deps
                        .iter()
                        .map(|(d, g)| Dep {
                            node: self.id(d % i),
                            gate: [Gate::Started, Gate::Ready, Gate::Completed][*g as usize % 3],
                        })
                        .collect();
                    deps.sort_by(|a, b| a.node.cmp(&b.node));
                    deps.dedup_by(|a, b| a.node == b.node);
                    deps
                };
                let spec = NodeSpec {
                    kind,
                    deps,
                    spec: Digest(format!("s{i}v{}", self.version[i])),
                    provider: ProviderName::from("fake"),
                };
                (self.id(i), spec)
            })
            .collect();
        Model {
            inputs: self.inputs(),
            nodes,
        }
    }

    fn index(&self, node: &NodeId) -> Option<usize> {
        (0..self.gens.len()).find(|i| &self.id(*i) == node)
    }

    fn world(&self) -> World {
        World {
            at: self.clock,
            seen: self
                .containers
                .iter()
                .map(|(node, c)| Seen {
                    node: node.clone(),
                    spec: c.spec.clone(),
                    generation: c.generation,
                    status: c.status.clone(),
                })
                .collect(),
        }
    }

    fn event(&mut self, node: &NodeId, spec: &Digest, kind: RuntimeKind) {
        self.clock += 1;
        let at = self.clock;
        self.notes.push_back(Note::Event(Event::Runtime {
            node: node.clone(),
            spec: spec.clone(),
            kind,
            at,
        }));
    }

    fn answer(&mut self, e: &Effect, ok: bool) -> Answer {
        let fail = || Answer::Err {
            tail: "fake failure".into(),
        };
        match &e.op {
            Op::AcquireProjectLock => Answer::Ok,
            Op::Subscribe if ok => Answer::Ok,
            Op::Subscribe => fail(),
            Op::Observe => {
                Answer::Observed(self.snapshots.remove(&e.id).unwrap_or_else(|| self.world()))
            }
            Op::PromptTrust { .. } => Answer::Trust { granted: ok },
            Op::Import if ok => Answer::Imported(self.model()),
            Op::Import => fail(),
            Op::Inspect(n) => Answer::Inspected {
                restarting: matches!(
                    self.containers.get(n).map(|c| &c.status),
                    Some(Status::Restarting)
                ),
            },
            Op::Start { .. } if !ok => fail(),
            Op::Start {
                node,
                spec,
                generation,
            } => {
                if let Some(old) = self.containers.get(node).cloned()
                    && &old.spec != spec
                {
                    if matches!(old.status, Status::Running { .. } | Status::Restarting) {
                        self.event(node, &old.spec, RuntimeKind::Die { code: 137 });
                    }
                    self.event(node, &old.spec, RuntimeKind::Destroy);
                }
                let job = self.index(node).and_then(|i| self.probe(i)).is_none();
                let health = matches!(
                    self.index(node).and_then(|i| self.probe(i)),
                    Some(Probe::HealthEvent)
                );
                let status = if job {
                    Status::Exited { code: 0 }
                } else {
                    Status::Running {
                        health: health.then_some(Health::Starting),
                    }
                };
                let c = Container {
                    spec: spec.clone(),
                    generation: *generation,
                    status,
                    restarts_left: 1,
                };
                self.containers.insert(node.clone(), c);
                self.event(node, spec, RuntimeKind::Start);
                if job {
                    self.event(node, spec, RuntimeKind::Die { code: 0 });
                }
                Answer::Ok
            }
            // `rm --stop --force`: the container dies if it runs, then goes away.
            Op::Stop(node) => {
                if let Some(c) = self.containers.remove(node) {
                    if matches!(c.status, Status::Running { .. } | Status::Restarting) {
                        self.event(node, &c.spec, RuntimeKind::Die { code: 137 });
                    }
                    self.event(node, &c.spec, RuntimeKind::Destroy);
                }
                Answer::Ok
            }
            Op::Probe(n) => Answer::Probe {
                ok: ok
                    && matches!(
                        self.containers.get(n).map(|c| &c.status),
                        Some(Status::Running { .. })
                    ),
            },
            Op::ReleaseProjectLock | Op::SetDeadline { .. } => Answer::Ok,
        }
    }

    /// Delivers the oldest note of the node that note `i` belongs to: the runtime keeps each container's
    /// events in order, but nodes interleave. A restart note whose container moved on is skipped.
    fn deliver(&mut self, i: usize) -> Option<Event> {
        while !self.notes.is_empty() {
            let node_of = |n: &Note| match n {
                Note::Event(Event::Runtime { node, .. }) | Note::Restarted(node, _) => {
                    Some(node.clone())
                }
                Note::Event(_) => None,
            };
            let target = node_of(&self.notes[i % self.notes.len()]);
            let first = self
                .notes
                .iter()
                .position(|n| node_of(n) == target)
                .unwrap_or(0);
            match self.notes.remove(first)? {
                Note::Event(e) => return Some(e),
                Note::Restarted(node, spec) => {
                    let Some(c) = self.containers.get_mut(&node) else {
                        continue;
                    };
                    if !matches!(c.status, Status::Restarting) || c.spec != spec {
                        continue;
                    }
                    c.status = Status::Running { health: None };
                    self.clock += 1;
                    return Some(Event::Runtime {
                        node,
                        spec: c.spec.clone(),
                        kind: RuntimeKind::Start,
                        at: self.clock,
                    });
                }
            }
        }
        None
    }

    fn crash(&mut self, i: usize) {
        let node = self.id(i % self.gens.len());
        let Some(c) = self.containers.get_mut(&node) else {
            return;
        };
        if !matches!(c.status, Status::Running { .. }) {
            return;
        }
        let spec = c.spec.clone();
        if c.restarts_left > 0 {
            c.restarts_left -= 1;
            c.status = Status::Restarting;
            self.event(&node, &spec, RuntimeKind::Die { code: 1 });
            self.notes.push_back(Note::Restarted(node, spec));
        } else {
            c.status = Status::Exited { code: 1 };
            self.event(&node, &spec, RuntimeKind::Die { code: 1 });
        }
    }

    fn health(&mut self, i: usize, healthy: bool) -> bool {
        let i = i % self.gens.len();
        if self.probe(i) != Some(Probe::HealthEvent) {
            return false;
        }
        let node = self.id(i);
        let Some(c) = self.containers.get_mut(&node) else {
            return false;
        };
        let Status::Running { health } = &mut c.status else {
            return false;
        };
        let h = if healthy {
            Health::Healthy
        } else {
            Health::Unhealthy
        };
        if *health == Some(h) {
            return false;
        }
        *health = Some(h);
        let spec = c.spec.clone();
        self.event(&node, &spec, RuntimeKind::Health(h));
        true
    }

    /// The stream drops its notifications; the runtime itself moves on.
    fn lose_stream(&mut self) {
        for note in std::mem::take(&mut self.notes) {
            if let Note::Restarted(node, spec) = note
                && let Some(c) = self.containers.get_mut(&node)
                && matches!(c.status, Status::Restarting)
                && c.spec == spec
            {
                c.status = Status::Running { health: None };
                self.clock += 1;
            }
        }
    }
}

/// The shell side of the loop: send every planned effect until `plan` is empty, checking the invariants.
fn settle(mut s: State, fake: &mut Fake) -> Result<State, String> {
    for _ in 0..64 {
        let planned = plan(&s);
        check(&s, &planned)?;
        if planned.is_empty() {
            return Ok(s);
        }
        for e in planned {
            if e.op == Op::Observe {
                fake.snapshots.insert(e.id, fake.world());
            }
            s = reduce(s, Event::Sent(e));
        }
    }
    Err("plan never empties".into())
}

fn apply(mut s: State, fake: &mut Fake, c: &Choice) -> State {
    match c {
        Choice::Answer(i, ok) => {
            if let Some(e) = s.pending.values().nth(i % s.pending.len().max(1)).cloned() {
                let answer = fake.answer(&e, *ok);
                s = reduce(s, Event::Done { id: e.id, answer });
            }
        }
        Choice::Deliver(i) => {
            if let Some(e) = fake.deliver(*i) {
                s = reduce(s, e);
            }
        }
        Choice::Fire(i) => {
            if let Some(id) = s
                .deadlines
                .keys()
                .nth(i % s.deadlines.len().max(1))
                .copied()
            {
                s = reduce(s, Event::DeadlineHit(id));
            }
        }
        Choice::Crash(i) => fake.crash(*i),
        Choice::Health(i, ok) => {
            fake.health(*i, *ok);
        }
        Choice::Lost => {
            fake.lose_stream();
            s = reduce(s, Event::StreamLost);
        }
        Choice::Up => s = reduce(s, Event::Action(Action::Up { wait: Wait::Ready })),
        Choice::Down => s = reduce(s, Event::Action(Action::Down)),
        Choice::Restart(i) => {
            let name = fake.id(i % fake.gens.len()).name;
            s = reduce(s, Event::Action(Action::Restart { node: name }));
        }
        Choice::Edit(i) => {
            let i = i % fake.gens.len();
            fake.version[i] += 1;
            fake.edits += 1;
            s = reduce(
                s,
                Event::Hashed {
                    inputs: fake.inputs(),
                    trusted: false,
                },
            );
        }
    }
    s
}

/// Invariant 8: with no new fault, every effect succeeds and every event arrives; the loop must reach an
/// outcome within the bound.
fn converge(mut s: State, fake: &mut Fake) -> Result<usize, String> {
    let bound = 40 * fake.gens.len() + 60;
    for step in 0..bound {
        s = settle(s, fake)?;
        if let Some(result) = outcome(&s)
            && fake.notes.is_empty()
        {
            if result.is_ok() && s.intents.target == Target::Up {
                consistent(&s, fake)?;
            }
            if result.is_ok() && s.intents.target == Target::Down && !fake.containers.is_empty() {
                return Err(format!(
                    "down left containers: {:?}",
                    fake.containers.keys()
                ));
            }
            return Ok(step);
        }
        if let Some(e) = s.pending.values().next().cloned() {
            let answer = fake.answer(&e, true);
            s = reduce(s, Event::Done { id: e.id, answer });
        } else if !fake.notes.is_empty() {
            // `deliver` may only drop stale restart notes; that is progress too.
            if let Some(e) = fake.deliver(0) {
                s = reduce(s, e);
            }
        } else if (0..fake.gens.len()).any(|i| fake.health(i, true)) {
        } else if let Some(id) = first_deadline(&s) {
            s = reduce(s, Event::DeadlineHit(id));
        } else {
            return Err(format!(
                "stuck at step {step}: no outcome and nothing to do\n{s:#?}"
            ));
        }
    }
    Err(format!("no outcome within {bound} steps"))
}

/// A node the core calls ready runs in the fake runtime with the spec the core tracks.
fn consistent(s: &State, fake: &Fake) -> Result<(), String> {
    for (id, n) in &s.nodes {
        if n.machine == Machine::Service(Service::Ready) {
            let c = fake.containers.get(id);
            let ok = matches!(c, Some(c) if matches!(c.status, Status::Running { .. }) && Some(&c.spec) == n.live.as_ref());
            if !ok {
                return Err(format!(
                    "{} is ready in the core but {:?} in the runtime",
                    id.name.0,
                    c.map(|c| &c.status)
                ));
            }
        }
    }
    Ok(())
}

fn first_deadline(s: &State) -> Option<EffectId> {
    use rizr_core::event::DeadlineKind;
    let retry = s
        .deadlines
        .iter()
        .find(|(_, d)| d.kind == DeadlineKind::ProbeRetry);
    retry
        .or_else(|| s.deadlines.iter().next())
        .map(|(id, _)| *id)
}

fn node_gen() -> impl Strategy<Value = NodeGen> {
    (
        prop::bool::weighted(0.2),
        0u8..4,
        prop::collection::vec((any::<usize>(), 0u8..3), 0..3),
    )
        .prop_map(|(project, kind, deps)| NodeGen {
            project,
            kind,
            deps,
        })
}

fn choice() -> impl Strategy<Value = Choice> {
    prop_oneof![
        8 => (any::<usize>(), prop::bool::weighted(0.85)).prop_map(|(i, ok)| Choice::Answer(i, ok)),
        6 => any::<usize>().prop_map(Choice::Deliver),
        2 => any::<usize>().prop_map(Choice::Fire),
        1 => any::<usize>().prop_map(Choice::Crash),
        1 => (any::<usize>(), any::<bool>()).prop_map(|(i, ok)| Choice::Health(i, ok)),
        1 => Just(Choice::Lost),
        1 => Just(Choice::Up),
        1 => Just(Choice::Down),
        1 => any::<usize>().prop_map(Choice::Restart),
        1 => any::<usize>().prop_map(Choice::Edit),
    ]
}

fn start(fake: &mut Fake) -> Result<State, String> {
    let s = State::new(ProjectId::from("p"), InstanceId::from("main"), Gen(0));
    let s = reduce(
        s,
        Event::Hashed {
            inputs: fake.inputs(),
            trusted: false,
        },
    );
    let s = reduce(s, Event::Action(Action::Up { wait: Wait::Ready }));
    settle(s, fake)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, ..ProptestConfig::default() })]

    #[test]
    fn invariants_hold_and_the_loop_converges(
        gens in prop::collection::vec(node_gen(), 1..=5),
        choices in prop::collection::vec(choice(), 0..80),
    ) {
        let mut fake = Fake::new(gens);
        let mut s = start(&mut fake).map_err(TestCaseError::fail)?;
        for c in &choices {
            s = apply(s, &mut fake, c);
            s = settle(s, &mut fake).map_err(|e| TestCaseError::fail(format!("after {c:?}: {e}")))?;
        }
        converge(s, &mut fake).map_err(TestCaseError::fail)?;
    }
}

/// A clean start with no faults ends with every service ready and every job done.
#[test]
fn clean_up_reaches_up() {
    let gens = vec![
        NodeGen {
            project: false,
            kind: 1,
            deps: vec![],
        },
        NodeGen {
            project: false,
            kind: 3,
            deps: vec![(0, 1)],
        },
        NodeGen {
            project: false,
            kind: 2,
            deps: vec![(1, 2)],
        },
        NodeGen {
            project: true,
            kind: 0,
            deps: vec![],
        },
    ];
    let mut fake = Fake::new(gens);
    let s = start(&mut fake).expect("start");
    let mut s = s;
    let mut steps = 0;
    while outcome(&s).is_none() && steps < 200 {
        steps += 1;
        if let Some(e) = s.pending.values().next().cloned() {
            let answer = fake.answer(&e, true);
            s = reduce(s, Event::Done { id: e.id, answer });
        } else if let Some(e) = fake.deliver(0) {
            s = reduce(s, e);
        } else if !(0..4).any(|i| fake.health(i, true))
            && let Some(id) = first_deadline(&s)
        {
            s = reduce(s, Event::DeadlineHit(id));
        }
        s = settle(s, &mut fake).expect("settle");
    }
    assert_eq!(
        outcome(&s),
        Some(Ok(())),
        "{:?}",
        rizr_core::view::nodes(&s)
    );
    assert_eq!(
        rizr_core::view::instance_status(&s),
        rizr_core::view::InstanceStatus::Up
    );
}

/// A `die` buffered during a resync, older than the snapshot, must not undo the snapshot.
#[test]
fn stale_buffered_event_does_not_undo_the_snapshot() {
    let mut fake = Fake::new(vec![NodeGen {
        project: false,
        kind: 0,
        deps: vec![],
    }]);
    let mut s = start(&mut fake).expect("start");
    while outcome(&s).is_none() {
        if let Some(e) = s.pending.values().next().cloned() {
            let answer = fake.answer(&e, true);
            s = reduce(s, Event::Done { id: e.id, answer });
        } else if let Some(e) = fake.deliver(0) {
            s = reduce(s, e);
        }
        s = settle(s, &mut fake).expect("settle");
    }
    let node = fake.id(0);
    let spec = s.nodes[&node].live.clone().expect("live");
    fake.crash(0);
    fake.lose_stream();
    s = reduce(s, Event::StreamLost);
    s = settle(s, &mut fake).expect("settle");
    let subscribe = s.pending.values().next().cloned().expect("subscribe");
    s = reduce(
        s,
        Event::Done {
            id: subscribe.id,
            answer: Answer::Ok,
        },
    );
    s = settle(s, &mut fake).expect("settle");
    let observe = s.pending.values().next().cloned().expect("observe");
    s = reduce(
        s,
        Event::Runtime {
            node: node.clone(),
            spec,
            kind: RuntimeKind::Die { code: 1 },
            at: 1,
        },
    );
    let world = fake.snapshots.remove(&observe.id).expect("snapshot");
    assert!(world.at > 1);
    s = reduce(
        s,
        Event::Done {
            id: observe.id,
            answer: Answer::Observed(world),
        },
    );
    assert_eq!(s.nodes[&node].machine, Machine::Service(Service::Ready));
}

/// `status` right after `up`: the snapshot answers before the model loads, so it cannot know `migrate` is a job.
/// The finished job must still show as done, not pending.
#[test]
fn a_snapshot_before_the_model_keeps_a_finished_job_done() {
    use rizr_core::machine::Job;
    let node = NodeId {
        scope: Scope::Instance(InstanceId::from("main")),
        name: NodeName::from("migrate"),
    };
    let world = World {
        at: 1,
        seen: vec![Seen {
            node: node.clone(),
            spec: Digest::from("s"),
            generation: Gen(3),
            status: Status::Exited { code: 0 },
        }],
    };
    let model = Model {
        inputs: Digest::from("in"),
        nodes: BTreeMap::from([(
            node.clone(),
            NodeSpec {
                kind: NodeKind::Job {
                    kind: JobKind::Migrate,
                },
                deps: vec![],
                spec: Digest::from("s"),
                provider: ProviderName::from("fake"),
            },
        )]),
    };
    let mut s = State::new(ProjectId::from("p"), InstanceId::from("main"), Gen(3));
    s = reduce(
        s,
        Event::Hashed {
            inputs: Digest::from("in"),
            trusted: true,
        },
    );
    for _ in 0..16 {
        for e in plan(&s) {
            s = reduce(s, Event::Sent(e));
        }
        let Some(e) = s
            .pending
            .values()
            .find(|e| e.op != Op::Import)
            .or_else(|| s.pending.values().next())
            .cloned()
        else {
            break;
        };
        let answer = match e.op {
            Op::Observe => Answer::Observed(world.clone()),
            Op::Import => Answer::Imported(model.clone()),
            _ => Answer::Ok,
        };
        s = reduce(s, Event::Done { id: e.id, answer });
    }
    assert_eq!(s.nodes[&node].machine, Machine::Job(Job::Done(Gen(3))));
}
