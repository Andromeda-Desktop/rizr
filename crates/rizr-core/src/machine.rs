//! The stored machines (SPEC-core section 4). Each has one transition function that rejects inputs that do
//! not fit the state.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::Gen;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Failure {
    Exit { code: i32 },
    StartTimeout,
    StartError { tail: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Starting,
    Healthy,
    Unhealthy,
}

/// When the next active probe may run while a service starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeTurn {
    Due,
    /// The last probe failed; the next one waits for the retry deadline.
    Wait,
    /// The service has no probe: it is ready when it is still up at the settle deadline.
    Settle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Service {
    Absent,
    Starting(ProbeTurn),
    Ready,
    Unhealthy,
    /// The container died; an `Inspect` decides between `Restarting` and `Failed`.
    Exited {
        code: i32,
    },
    /// The runtime restart policy runs; rizr only watches.
    Restarting,
    /// Runs an older spec; `plan` recreates it.
    Outdated,
    Stopping,
    Failed(Failure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Job {
    Pending,
    Running,
    Stopping,
    Done(Gen),
    Failed(Failure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Machine {
    Service(Service),
    Job(Job),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    StartSent,
    StartFailed(String),
    StopSent,
    Started,
    Died {
        code: i32,
    },
    Destroyed,
    Health(Health),
    ProbeResult(bool),
    RetryDue,
    Settled,
    Inspected {
        restarting: bool,
    },
    StartTimeout,
    /// A user action (`up`, `restart`) clears a failure.
    Reset,
    SpecChanged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejected;

/// Context a transition needs from the node and the intents.
#[derive(Clone, Copy, Debug)]
pub struct Ctx {
    pub ready_on_start: bool,
    pub generation: Gen,
}

impl Machine {
    pub fn step(&self, input: &Input, ctx: Ctx) -> Result<Machine, Rejected> {
        match self {
            Machine::Service(s) => service(s, input, ctx).map(Machine::Service),
            Machine::Job(j) => job(j, input, ctx).map(Machine::Job),
        }
    }

    /// A state that waits on an event, a deadline or an effect result (invariant 9).
    pub fn is_waiting(&self) -> bool {
        matches!(
            self,
            Machine::Service(
                Service::Starting(_)
                    | Service::Exited { .. }
                    | Service::Restarting
                    | Service::Stopping
            ) | Machine::Job(Job::Running | Job::Stopping)
        )
    }

    pub fn label(&self) -> String {
        match self {
            Machine::Service(Service::Starting(_)) => "starting".into(),
            Machine::Service(Service::Exited { code }) => format!("exited({code})"),
            Machine::Service(Service::Failed(f)) | Machine::Job(Job::Failed(f)) => {
                format!("failed({})", failure_label(f))
            }
            Machine::Service(s) => format!("{s:?}").to_lowercase(),
            Machine::Job(Job::Done(g)) => format!("done(generation {})", g.0),
            Machine::Job(j) => format!("{j:?}").to_lowercase(),
        }
    }
}

pub fn failure_label(f: &Failure) -> String {
    match f {
        Failure::Exit { code } => format!("exit {code}"),
        Failure::StartTimeout => "start timeout".into(),
        Failure::StartError { tail } => format!("start error: {tail}"),
    }
}

fn service(s: &Service, input: &Input, ctx: Ctx) -> Result<Service, Rejected> {
    use Input as I;
    use Service as S;
    let started = || {
        if ctx.ready_on_start {
            S::Starting(ProbeTurn::Settle)
        } else {
            S::Starting(ProbeTurn::Due)
        }
    };
    Ok(match (s, input) {
        (S::Absent | S::Outdated, I::StartSent) => S::Starting(ProbeTurn::Due),
        (S::Starting(_), I::StartFailed(tail)) => {
            S::Failed(Failure::StartError { tail: tail.clone() })
        }
        (S::Starting(_), I::Started) => started(),
        (S::Restarting | S::Exited { .. }, I::Started) => started(),
        (S::Starting(_), I::ProbeResult(true) | I::Health(Health::Healthy)) => S::Ready,
        (S::Starting(_), I::ProbeResult(false)) => S::Starting(ProbeTurn::Wait),
        (S::Starting(ProbeTurn::Wait), I::RetryDue) => S::Starting(ProbeTurn::Due),
        (S::Starting(ProbeTurn::Settle), I::Settled) => S::Ready,
        (S::Starting(_) | S::Ready, I::Health(Health::Unhealthy)) => S::Unhealthy,
        (S::Unhealthy, I::Health(Health::Healthy)) => S::Ready,
        (S::Starting(_), I::StartTimeout) => S::Failed(Failure::StartTimeout),
        (S::Starting(_) | S::Ready | S::Unhealthy | S::Restarting, I::Died { code }) => {
            S::Exited { code: *code }
        }
        (S::Exited { .. }, I::Inspected { restarting: true }) => S::Restarting,
        (S::Exited { code }, I::Inspected { restarting: false }) => {
            S::Failed(Failure::Exit { code: *code })
        }
        (
            S::Starting(_)
            | S::Ready
            | S::Unhealthy
            | S::Restarting
            | S::Outdated
            | S::Exited { .. }
            | S::Failed(_)
            | S::Absent,
            I::StopSent,
        ) => S::Stopping,
        (S::Stopping, I::Died { .. } | I::Destroyed) => S::Absent,
        (S::Failed(_), I::Reset | I::SpecChanged) => S::Absent,
        (
            S::Starting(_) | S::Ready | S::Unhealthy | S::Restarting | S::Exited { .. },
            I::SpecChanged,
        ) => S::Outdated,
        _ => return Err(Rejected),
    })
}

fn job(j: &Job, input: &Input, ctx: Ctx) -> Result<Job, Rejected> {
    use Input as I;
    use Job as J;
    Ok(match (j, input) {
        (J::Pending, I::StartSent) => J::Running,
        (J::Running, I::StartFailed(tail)) => J::Failed(Failure::StartError { tail: tail.clone() }),
        (J::Running, I::Started | I::Health(_)) => J::Running,
        (J::Running, I::Died { code: 0 }) => J::Done(ctx.generation),
        (J::Running, I::Died { code }) => J::Failed(Failure::Exit { code: *code }),
        (J::Pending | J::Running | J::Done(_) | J::Failed(_), I::StopSent) => J::Stopping,
        (J::Stopping, I::Died { .. } | I::Destroyed) => J::Pending,
        (J::Failed(_) | J::Done(_), I::Reset | I::SpecChanged) => J::Pending,
        _ => return Err(Rejected),
    })
}
