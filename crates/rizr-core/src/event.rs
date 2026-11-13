//! Events into `reduce` and effects out of `plan` (SPEC-core sections 5 and 6). Both are runtime-neutral: a
//! provider renders an effect to its real command.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::machine::Health;
use crate::model::{Digest, EffectId, Gen, Model, NodeId, NodeName};

/// A request from a client: the CLI, MCP or the dashboard. The CLI subcommands are these variants.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "clap", derive(clap::Subcommand))]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum Action {
    /// Start the stack and return when it is ready, or with the failing node and its cause. Also the wait: run it
    /// again while a node restarts; it starts only what is down.
    Up {
        #[cfg_attr(feature = "clap", arg(long, value_enum, default_value = "ready"))]
        wait: Wait,
    },
    /// Stop the stack. Data stays.
    Down,
    /// Clear the failure of one node and start it again.
    Restart { node: NodeName },
    /// Trust the repo files that rizr will read and run. After you edit them: `rizr trust --yes && rizr up`.
    Trust {
        /// Without a terminal: trust the changed files of a repo that a person trusted before.
        #[cfg_attr(feature = "clap", arg(long))]
        #[serde(default)]
        yes: bool,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[serde(rename_all = "snake_case")]
pub enum Wait {
    /// Return when the last start is sent; leaves may still be starting.
    Started,
    /// Return when every service is ready and every job is done.
    #[default]
    Ready,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The shell hashed the trust inputs with rizr's own parsers and looked them up in the trust store.
    Hashed {
        inputs: Digest,
        trusted: bool,
    },
    Action(Action),
    /// The shell sent an effect from `plan`.
    Sent(Effect),
    Done {
        id: EffectId,
        answer: Answer,
    },
    /// `at`: the runtime's own event time (Docker `timeNano`); compared, never read as a clock.
    Runtime {
        node: NodeId,
        spec: Digest,
        kind: RuntimeKind,
        at: u64,
    },
    StreamLost,
    DeadlineHit(EffectId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeKind {
    Start,
    Die { code: i32 },
    Destroy,
    Health(Health),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    Ok,
    Err { tail: String },
    Trust { granted: bool },
    Imported(Model),
    Observed(World),
    Inspected { restarting: bool },
    Probe { ok: bool },
}

/// What the runtime holds now: one entry per rizr-labelled container or unit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct World {
    /// Runtime time of the snapshot; buffered events up to it are already in `seen`.
    pub at: u64,
    pub seen: Vec<Seen>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub node: NodeId,
    pub spec: Digest,
    pub generation: Gen,
    pub status: Status,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Created,
    Running { health: Option<Health> },
    Restarting,
    Exited { code: i32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Effect {
    pub id: EffectId,
    pub op: Op,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeadlineKind {
    StartTimeout,
    ProbeRetry,
    Settle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    AcquireProjectLock,
    ReleaseProjectLock,
    Subscribe,
    Observe,
    PromptTrust {
        inputs: Digest,
    },
    Import,
    Inspect(NodeId),
    Start {
        node: NodeId,
        spec: Digest,
        generation: Gen,
    },
    Stop(NodeId),
    Probe(NodeId),
    SetDeadline {
        node: NodeId,
        kind: DeadlineKind,
        after_ms: u64,
    },
}

/// `RepoInput` effects read repo files with a repo tool or run repo-defined content (invariant 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Meta,
    RepoInput,
}

impl Op {
    pub fn class(&self) -> Class {
        match self {
            Op::Import | Op::Start { .. } | Op::Stop(_) | Op::Probe(_) => Class::RepoInput,
            _ => Class::Meta,
        }
    }

    pub fn node(&self) -> Option<&NodeId> {
        match self {
            Op::Inspect(n) | Op::Stop(n) | Op::Probe(n) => Some(n),
            Op::Start { node, .. } | Op::SetDeadline { node, .. } => Some(node),
            _ => None,
        }
    }
}
