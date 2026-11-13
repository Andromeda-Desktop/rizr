//! Derived views, never stored: instance status, the exit rule, drift and the run receipt (SPEC-core section 8).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorCode};
use crate::event::Wait;
use crate::machine::{Failure, Job, Machine, Service, failure_label};
use crate::model::{Gate, NodeId, NodeKind, NodeName, Probe, Scope};
use crate::plan::{gate_holds, plan};
use crate::state::{ModelState, State, Sync, Target, Trust};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstanceStatus {
    Down,
    Starting,
    Up,
    Degraded,
}

pub fn instance_status(s: &State) -> InstanceStatus {
    let nodes: Vec<&Machine> = s
        .nodes
        .values()
        .filter(|n| n.spec.is_some())
        .map(|n| &n.machine)
        .collect();
    let failed = |m: &&Machine| {
        matches!(
            m,
            Machine::Service(Service::Failed(_) | Service::Unhealthy)
                | Machine::Job(Job::Failed(_))
        )
    };
    let fine = |m: &&Machine| {
        matches!(
            m,
            Machine::Service(Service::Ready) | Machine::Job(Job::Done(_))
        )
    };
    let off = |m: &&Machine| {
        matches!(
            m,
            Machine::Service(Service::Absent) | Machine::Job(Job::Pending)
        )
    };
    // Shared nodes keep running for other instances after this one is down: Down looks only at its own nodes.
    if nodes.iter().any(failed) {
        InstanceStatus::Degraded
    } else if !nodes.is_empty() && nodes.iter().all(fine) {
        InstanceStatus::Up
    } else if s
        .nodes
        .iter()
        .filter(|(id, n)| id.scope != Scope::Project && n.spec.is_some())
        .all(|(_, n)| off(&&n.machine))
    {
        InstanceStatus::Down
    } else {
        InstanceStatus::Starting
    }
}

/// A node that cannot start because a dependency failed or never reaches its gate on its own.
fn blocked(s: &State, id: &NodeId) -> bool {
    let Some(spec) = s.nodes.get(id).and_then(|n| n.spec.as_ref()) else {
        return false;
    };
    spec.deps.iter().any(|d| {
        if gate_holds(s, d) {
            return false;
        }
        match s.nodes.get(&d.node).map(|n| &n.machine) {
            None => true,
            Some(Machine::Service(Service::Failed(_) | Service::Unhealthy))
            | Some(Machine::Job(Job::Failed(_))) => true,
            Some(Machine::Service(Service::Absent | Service::Outdated))
            | Some(Machine::Job(Job::Pending)) => blocked(s, &d.node),
            Some(Machine::Service(Service::Ready)) => d.gate == Gate::Completed,
            _ => false,
        }
    })
}

/// `Some` when a one-shot verb may exit: `Ok` or the first error in start order.
pub fn outcome(s: &State) -> Option<Result<(), Error>> {
    if let Trust::Refused(_) = s.trust {
        return Some(Err(Error::new(
            ErrorCode::Untrusted,
            "this repo is not trusted",
            "rizr runs nothing from a repo before you trust its files",
            "rizr trust",
        )));
    }
    if let Sync::Failed(tail) = &s.sync {
        return Some(Err(Error::new(
            ErrorCode::RuntimeUnavailable,
            format!("the container runtime does not answer: {tail}"),
            "start the docker service",
            "systemctl start docker",
        )));
    }
    if let ModelState::Failed(tail) = &s.model {
        return Some(Err(Error::new(
            ErrorCode::ImportFailed,
            format!("the compose file does not resolve: {tail}"),
            "run the same command by hand to see the full error",
            "docker compose config",
        )));
    }
    if !s.trusted() || s.sync != Sync::Synced || s.loaded().is_none() || !s.pending.is_empty() {
        return None;
    }
    // `down` leaves shared project nodes as they are, so it does not wait for them.
    let quiet = plan(s).is_empty()
        && !s.nodes.iter().any(|(id, n)| {
            n.machine.is_waiting()
                && (s.intents.target != Target::Down || id.scope != Scope::Project)
        });
    match s.intents.target {
        Target::Keep => Some(Ok(())),
        Target::Down => quiet.then_some(Ok(())),
        Target::Up => {
            let started = s.intents.wait == Wait::Started
                && s.nodes
                    .iter()
                    .filter(|(_, n)| n.spec.is_some())
                    .all(|(id, n)| {
                        !matches!(
                            n.machine,
                            Machine::Service(Service::Absent | Service::Outdated)
                                | Machine::Job(Job::Pending)
                        ) || blocked(s, id)
                    });
            if !(quiet || started) {
                return None;
            }
            Some(first_problem(s).map_or(Ok(()), Err))
        }
    }
}

fn first_problem(s: &State) -> Option<Error> {
    let m = s.loaded()?;
    node_problem(s, &m.topo()).or_else(|| m.findings.first().map(|f| f.error.clone()))
}

fn node_problem(s: &State, order: &[NodeId]) -> Option<Error> {
    order.iter().find_map(|id| {
        let n = s.nodes.get(id)?;
        let name = &id.name.0;
        let logs = format!("rizr logs {name}");
        let e = match &n.machine {
            Machine::Service(Service::Failed(Failure::StartTimeout)) => Error::new(
                ErrorCode::StartTimeout,
                format!("{name} did not become ready in time"),
                "read its logs; its readiness signal is in `rizr why`",
                logs,
            ),
            Machine::Service(Service::Failed(f)) => Error::new(
                ErrorCode::StartFailed,
                format!("{name} failed: {}", failure_label(f)),
                "read its logs",
                logs,
            ),
            Machine::Job(Job::Failed(f)) => Error::new(
                ErrorCode::JobFailed,
                format!("job {name} failed: {}", failure_label(f)),
                "fix the job, then run up again; migrations skip applied steps",
                logs,
            ),
            Machine::Service(Service::Unhealthy) => Error::new(
                ErrorCode::Unhealthy,
                format!("{name} is unhealthy"),
                "its healthcheck fails; read its logs",
                logs,
            ),
            Machine::Service(Service::Absent | Service::Outdated) | Machine::Job(Job::Pending)
                if blocked(s, id) =>
            {
                Error::new(
                    ErrorCode::DependencyFailed,
                    format!("{name} waits for a dependency that failed"),
                    "fix the first failed node above it",
                    format!("rizr why {name}"),
                )
            }
            _ => return None,
        };
        Some(e.on(name))
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NodeView {
    pub node: NodeId,
    pub state: String,
    /// The readiness signal: healthcheck, log line, port probe or "ready at start".
    pub signal: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Drift {
    pub node: NodeId,
    pub desired: String,
    pub actual: String,
    /// What `plan` does next for this node, if anything.
    pub next: Option<String>,
}

/// Proof that the stack ran: an agent attaches it to its report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Receipt {
    pub generation: u64,
    pub status: InstanceStatus,
    pub nodes: Vec<NodeView>,
    /// Started nodes whose readiness rests on an open port or the start alone: nothing proves they serve requests.
    pub unverified: Vec<NodeName>,
    /// FNV-1a 64 over the model inputs, generation and node states.
    pub hash: String,
}

pub fn signal(kind: Option<&NodeKind>) -> String {
    match kind {
        Some(NodeKind::Service { probe }) => match probe {
            Probe::HealthEvent => "healthcheck".into(),
            Probe::LogLine { pattern } => format!("log line {pattern:?}"),
            Probe::Tcp { ports } => format!("tcp {ports:?}"),
            Probe::Http { port, path } => format!("http :{port}{path}"),
            Probe::AtStart => "ready at start".into(),
        },
        Some(NodeKind::Job { .. }) => "exit 0".into(),
        None => "not in the model".into(),
    }
}

/// A ready service whose signal is an open port or the start alone: the app behind it may still fail.
fn unproven(n: &crate::state::Node) -> bool {
    n.machine == Machine::Service(Service::Ready)
        && matches!(
            n.spec.as_ref().map(|x| &x.kind),
            Some(NodeKind::Service {
                probe: Probe::Tcp { .. } | Probe::AtStart
            })
        )
}

pub fn unverified(s: &State) -> Vec<NodeName> {
    s.nodes
        .iter()
        .filter(|(_, n)| unproven(n))
        .map(|(id, _)| id.name.clone())
        .collect()
}

pub fn nodes(s: &State) -> Vec<NodeView> {
    s.nodes
        .iter()
        .map(|(id, n)| NodeView {
            node: id.clone(),
            state: if unproven(n) {
                "started (unverified)".into()
            } else {
                n.machine.label()
            },
            signal: signal(n.spec.as_ref().map(|x| &x.kind)),
        })
        .collect()
}

pub fn drift(s: &State) -> Vec<Drift> {
    let planned = plan(s);
    s.nodes
        .iter()
        .map(|(id, n)| {
            let desired = match (s.intents.target, n.spec.is_some()) {
                (_, false) => "removed",
                (Target::Up, true) => "up",
                (Target::Down, true) => "down",
                (Target::Keep, true) => "unchanged",
            };
            let next = planned
                .iter()
                .find(|e| e.op.node() == Some(id))
                .map(|e| e.reason.clone());
            Drift {
                node: id.clone(),
                desired: desired.into(),
                actual: n.machine.label(),
                next,
            }
        })
        .filter(|d| {
            d.next.is_some()
                || match d.desired.as_str() {
                    "up" => !(d.actual == "ready" || d.actual.starts_with("done")),
                    "down" => !(d.actual == "absent" || d.actual == "pending"),
                    "removed" => d.actual != "absent",
                    _ => false,
                }
        })
        .collect()
}

pub fn receipt(s: &State) -> Receipt {
    let nodes = nodes(s);
    let inputs = s.loaded().map(|m| m.inputs.0.clone()).unwrap_or_default();
    let body =
        serde_json::to_string(&(&inputs, s.intents.generation.0, &nodes)).unwrap_or_default();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in body.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    Receipt {
        generation: s.intents.generation.0,
        status: instance_status(s),
        nodes,
        unverified: unverified(s),
        hash: format!("{h:016x}"),
    }
}
