//! `plan`: the effects that move the runtime toward the intents. Pure; equal states give equal effects.

use crate::event::{DeadlineKind, Effect, Op};
use crate::machine::{Job, Machine, ProbeTurn, Service};
use crate::model::{Dep, EffectId, Gate, NodeId, NodeKind, Scope};
use crate::state::{DataAxis, Lock, ModelState, State, Sync, Target, Trust};

/// Below the 100 s budget of a verb (rizr docker.rs), so `up` gives a verdict on a stuck start before it returns.
pub const START_TIMEOUT_MS: u64 = 90_000;
pub const PROBE_RETRY_MS: u64 = 500;
/// How long a service with no probe must stay up before it counts as ready; most crashes at start come sooner.
pub const SETTLE_MS: u64 = 2_000;

pub fn plan(s: &State) -> Vec<Effect> {
    let mut ops: Vec<(Op, String)> = Vec::new();
    let pending = |f: &dyn Fn(&Op) -> bool| s.pending.values().any(|e| f(&e.op));

    match s.sync {
        Sync::Unsubscribed => ops.push((
            Op::Subscribe,
            "watch runtime events before reading state".into(),
        )),
        // A snapshot taken while a start or stop runs can miss its container; wait for it to answer.
        Sync::Subscribed if !pending(&|op| matches!(op, Op::Start { .. } | Op::Stop(_))) => {
            ops.push((Op::Observe, "read what the runtime holds now".into()))
        }
        _ => {}
    }
    if let Trust::Untrusted(inputs) = &s.trust
        && !pending(&|op| matches!(op, Op::PromptTrust { .. }))
    {
        ops.push((
            Op::PromptTrust {
                inputs: inputs.clone(),
            },
            "rizr runs nothing from this repo before you trust it".into(),
        ));
    }
    let Trust::Trusted(inputs) = &s.trust else {
        return number(s, ops);
    };
    let model = match &s.model {
        ModelState::Loaded(m) if &m.inputs == inputs => m,
        ModelState::Failed(_) => return number(s, ops),
        _ => {
            if !pending(&|op| matches!(op, Op::Import)) {
                ops.push((Op::Import, "resolve the compose model".into()));
            }
            return number(s, ops);
        }
    };
    if s.sync == Sync::Synced && s.intents.target == Target::Keep {
        // Observe only: one probe per running service shows whether it is ready; nothing starts or stops.
        for (id, n) in &s.nodes {
            let active = matches!(&n.spec, Some(sp) if matches!(&sp.kind, NodeKind::Service { probe } if probe.is_active()));
            if n.machine == Machine::Service(Service::Starting(ProbeTurn::Due))
                && active
                && n.running
                && !s.pending_for(id).any(|op| matches!(op, Op::Probe(_)))
            {
                ops.push((Op::Probe(id.clone()), "check readiness".into()));
            }
        }
    }
    if s.sync != Sync::Synced || s.intents.target == Target::Keep {
        return number(s, ops);
    }

    let mut order = model.topo();
    order.extend(
        s.nodes
            .keys()
            .filter(|id| !model.nodes.contains_key(*id))
            .cloned(),
    );
    if s.intents.target == Target::Down {
        order.reverse();
    }
    let mut node_ops: Vec<(Op, String)> = Vec::new();
    let mut need_lock = false;
    let mut project_busy = false;
    for id in &order {
        let Some(n) = s.nodes.get(id) else { continue };
        let before = node_ops.len();
        let busy = s
            .pending_for(id)
            .any(|op| matches!(op, Op::Start { .. } | Op::Stop(_)));
        let locked = id.scope != Scope::Project || s.lock == Lock::Held;
        let mut start_stop = |op: Op, why: String, out: &mut Vec<(Op, String)>| {
            if busy {
                return;
            }
            if locked {
                out.push((op, why));
            } else {
                need_lock = true;
            }
        };
        let up = s.intents.target == Target::Up && n.spec.is_some();
        match (&n.machine, up) {
            (Machine::Service(Service::Absent | Service::Outdated), true)
            | (Machine::Job(Job::Pending), true) => {
                let Some(spec) = n.spec.as_ref().map(|x| x.spec.clone()) else {
                    continue;
                };
                if gates_open(s, id) && !switching(s, id) {
                    let why = if matches!(n.machine, Machine::Service(Service::Outdated)) {
                        "the spec changed; recreate it"
                    } else {
                        "desired up; every dependency gate holds"
                    };
                    start_stop(
                        Op::Start {
                            node: id.clone(),
                            spec,
                            generation: s.intents.generation,
                        },
                        why.into(),
                        &mut node_ops,
                    );
                }
            }
            (Machine::Service(Service::Starting(turn)), true) => {
                if !s.has_deadline(id, &DeadlineKind::StartTimeout) {
                    node_ops.push((
                        Op::SetDeadline {
                            node: id.clone(),
                            kind: DeadlineKind::StartTimeout,
                            after_ms: START_TIMEOUT_MS,
                        },
                        "fail the start if it is not ready in time".into(),
                    ));
                }
                let active = matches!(&n.spec, Some(sp) if matches!(&sp.kind, NodeKind::Service { probe } if probe.is_active()));
                let probing = s.pending_for(id).any(|op| matches!(op, Op::Probe(_)));
                match turn {
                    ProbeTurn::Due if active && n.running && !probing => {
                        node_ops.push((Op::Probe(id.clone()), "check readiness".into()));
                    }
                    ProbeTurn::Wait if !s.has_deadline(id, &DeadlineKind::ProbeRetry) => node_ops
                        .push((
                            Op::SetDeadline {
                                node: id.clone(),
                                kind: DeadlineKind::ProbeRetry,
                                after_ms: PROBE_RETRY_MS,
                            },
                            "retry the probe".into(),
                        )),
                    ProbeTurn::Settle if !s.has_deadline(id, &DeadlineKind::Settle) => node_ops
                        .push((
                            Op::SetDeadline {
                                node: id.clone(),
                                kind: DeadlineKind::Settle,
                                after_ms: SETTLE_MS,
                            },
                            "ready if it is still up then".into(),
                        )),
                    _ => {}
                }
            }
            (Machine::Service(Service::Exited { .. }), true) => {
                if !s.pending_for(id).any(|op| matches!(op, Op::Inspect(_))) {
                    node_ops.push((
                        Op::Inspect(id.clone()),
                        "it died; does the restart policy restart it?".into(),
                    ));
                }
            }
            (
                Machine::Service(
                    Service::Starting(_)
                    | Service::Ready
                    | Service::Unhealthy
                    | Service::Restarting
                    | Service::Outdated
                    | Service::Exited { .. }
                    | Service::Completed
                    | Service::Failed(_),
                )
                | Machine::Job(Job::Running | Job::Done(_) | Job::Failed(_)),
                false,
            ) if id.scope != Scope::Project
                && (s.intents.target == Target::Down || n.spec.is_none()) =>
            {
                let why = if n.spec.is_none() {
                    "no longer in the model"
                } else {
                    "desired down"
                };
                start_stop(Op::Stop(id.clone()), why.into(), &mut node_ops);
            }
            // `live` holds the spec of a container that may remain: an exited job of an older generation.
            (Machine::Service(Service::Absent) | Machine::Job(Job::Pending), false)
                if id.scope != Scope::Project
                    && n.live.is_some()
                    && (s.intents.target == Target::Down || n.spec.is_none()) =>
            {
                start_stop(
                    Op::Stop(id.clone()),
                    "a container may remain".into(),
                    &mut node_ops,
                );
            }
            _ => {}
        }
        if id.scope == Scope::Project && (node_ops.len() > before || n.machine.is_waiting() || busy)
        {
            project_busy = true;
        }
    }

    match s.lock {
        Lock::Free if need_lock => ops.push((
            Op::AcquireProjectLock,
            "change a node shared by all instances".into(),
        )),
        Lock::Held if !need_lock && !project_busy => {
            node_ops.push((Op::ReleaseProjectLock, "no shared node changes now".into()));
        }
        _ => {}
    }
    ops.extend(node_ops);
    number(s, ops)
}

fn number(s: &State, ops: Vec<(Op, String)>) -> Vec<Effect> {
    ops.into_iter()
        .enumerate()
        .map(|(i, (op, reason))| Effect {
            id: EffectId(s.next + i as u64),
            op,
            reason,
        })
        .collect()
}

fn switching(s: &State, id: &NodeId) -> bool {
    match &id.scope {
        Scope::Instance(i) => matches!(s.data.get(i), Some(DataAxis::Switching { .. })),
        Scope::Project => false,
    }
}

/// Every dependency of `id` meets its gate (SPEC-core section 7).
pub fn gates_open(s: &State, id: &NodeId) -> bool {
    let Some(spec) = s.nodes.get(id).and_then(|n| n.spec.as_ref()) else {
        return false;
    };
    spec.deps.iter().all(|d| gate_holds(s, d))
}

pub fn gate_holds(s: &State, d: &Dep) -> bool {
    let Some(n) = s.nodes.get(&d.node) else {
        return false;
    };
    match (&d.gate, &n.machine) {
        (_, Machine::Service(Service::Ready)) | (_, Machine::Job(Job::Done(_))) => true,
        (Gate::Started, Machine::Service(Service::Starting(_) | Service::Unhealthy)) => n.running,
        (Gate::Started, Machine::Service(Service::Completed)) => true,
        (Gate::Started, Machine::Job(Job::Running)) => true,
        _ => false,
    }
}
