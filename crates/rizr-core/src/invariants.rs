//! The invariants of SPEC-core section 9 as one check. The property test calls it after every step;
//! invariant 8 (convergence) needs a driver and lives in the test.

use crate::event::{Class, DeadlineKind, Effect, Op};
use crate::machine::{Machine, Service};
use crate::model::Scope;
use crate::plan::{gates_open, plan};
use crate::state::{DataAxis, Lock, State, Sync, Target};
use crate::view::outcome;

/// `planned` is `plan(s)`; the check reads it so a caller can reuse one plan.
pub fn check(s: &State, planned: &[Effect]) -> Result<(), String> {
    let model = s.loaded();

    // 1. Trust.
    if !s.trusted()
        && let Some(e) = planned.iter().find(|e| e.op.class() == Class::RepoInput)
    {
        return Err(format!("1: {:?} planned before trust", e.op));
    }

    for e in planned {
        if let Op::Start { node, spec, .. } = &e.op {
            // 2. Order.
            if s.sync != Sync::Synced || !gates_open(s, node) {
                return Err(format!("2: start of {} before its gates hold", node.name.0));
            }
            // 3. Names: the start carries the model's spec for the node.
            let want = model.and_then(|m| m.nodes.get(node)).map(|x| &x.spec);
            if want != Some(spec) {
                return Err(format!(
                    "3: start of {} with a spec the model does not hold",
                    node.name.0
                ));
            }
            // 4. No start in a switching instance.
            if let Scope::Instance(i) = &node.scope
                && matches!(s.data.get(i), Some(DataAxis::Switching { .. }))
            {
                return Err(format!(
                    "4: start of {} while its instance switches data",
                    node.name.0
                ));
            }
        }
    }

    // 3. At most one start or stop per node, pending and planned together.
    for id in s.nodes.keys() {
        let n = s
            .pending
            .values()
            .chain(planned)
            .filter(|e| matches!(&e.op, Op::Start { node, .. } | Op::Stop(node) if node == id))
            .count();
        if n > 1 {
            return Err(format!("3: {n} starts or stops of {}", id.name.0));
        }
    }

    // 6. Quiet: an exited loop leaves no pending effect and no deadline. `down` leaves a shared project node
    // as it is, with its deadline: the next `up` of any instance drives it.
    let deadlines = s
        .deadlines
        .values()
        .filter(|d| s.intents.target != Target::Down || d.node.scope != Scope::Project)
        .count();
    if planned.is_empty()
        && matches!(outcome(s), Some(Ok(())))
        && (!s.pending.is_empty() || deadlines > 0)
    {
        return Err(format!(
            "6: quiet with {} pending and {} deadlines",
            s.pending.len(),
            deadlines
        ));
    }

    // 7. Deterministic, numbered from `next`.
    if plan(&s.clone()) != planned {
        return Err("7: plan differs on a clone".into());
    }
    if planned
        .iter()
        .enumerate()
        .any(|(i, e)| e.id.0 != s.next + i as u64)
    {
        return Err("7: effect ids are not consecutive from next".into());
    }

    // 9. No stall: checked once the plan is sent, so only with an empty plan. A loop that exits with an
    // error (refused trust, failed stream) leaves nothing waiting.
    if planned.is_empty() && !matches!(outcome(s), Some(Err(_))) {
        let watching = matches!(
            s.sync,
            Sync::Subscribing | Sync::Subscribed | Sync::Observing | Sync::Synced
        );
        // Node planning waits behind trust, import and sync; their pending effect wakes the loop.
        let gated = !s.trusted()
            || s.sync != Sync::Synced
            || !matches!((s.loaded(), &s.trust), (Some(m), crate::state::Trust::Trusted(i)) if &m.inputs == i);
        let gate_pending = gated && !s.pending.is_empty();
        for (id, n) in &s.nodes {
            if !n.machine.is_waiting() {
                continue;
            }
            let has_pending = s.pending_for(id).next().is_some();
            let ok = match &n.machine {
                Machine::Service(Service::Starting(_))
                    if s.intents.target == Target::Up && n.spec.is_some() =>
                {
                    s.has_deadline(id, &DeadlineKind::StartTimeout) || has_pending
                }
                _ => has_pending || watching,
            };
            if !ok && !gate_pending {
                return Err(format!(
                    "9: {} waits in {} with nothing to wake it",
                    id.name.0,
                    n.machine.label()
                ));
            }
        }
    }

    // 10. Project lock.
    let unlocked = s.pending.values().chain(planned).any(|e| {
        matches!(&e.op, Op::Start { node, .. } | Op::Stop(node) if node.scope == Scope::Project)
    }) && s.lock != Lock::Held;
    if unlocked {
        return Err("10: a shared node changes without the project lock".into());
    }
    Ok(())
}
