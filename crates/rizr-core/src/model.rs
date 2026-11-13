use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

macro_rules! name {
    ($($n:ident),*) => {$(
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $n(pub String);

        impl From<&str> for $n {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }
    )*};
}
name!(ProjectId, InstanceId, NodeName, ProviderName, Digest);

/// The `up` generation: each `Up` action takes the next one.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub struct Gen(pub u64);

/// Sequence number of an effect; a deadline uses the id of its `SetDeadline` effect.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub struct EffectId(pub u64);

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Project,
    Instance(InstanceId),
}

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub struct NodeId {
    pub scope: Scope,
    pub name: NodeName,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    /// Digest of the trusted inputs this model was imported from.
    pub inputs: Digest,
    pub nodes: BTreeMap<NodeId, NodeSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeSpec {
    pub kind: NodeKind,
    pub deps: Vec<Dep>,
    /// Digest of the resolved service and the rizr override, without the generation label.
    pub spec: Digest,
    pub provider: ProviderName,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Service { probe: Probe },
    Job { kind: JobKind },
}

/// Readiness signal of a service, in the order of VISION section 3.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Probe {
    HealthEvent,
    LogLine {
        pattern: String,
    },
    Tcp {
        ports: Vec<u16>,
    },
    Http {
        port: u16,
        path: String,
    },
    /// No port and no signal: ready when the runtime reports `start`.
    AtStart,
}

impl Probe {
    /// A probe that rizr runs as an effect; the others resolve from runtime events.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Probe::LogLine { .. } | Probe::Tcp { .. } | Probe::Http { .. }
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    /// Runs on every `up`; migration tools skip applied steps.
    Migrate,
    /// Runs once per data set (D1).
    Seed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dep {
    pub node: NodeId,
    pub gate: Gate,
}

/// What a dependency must reach before its dependent starts (SPEC-core section 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    Started,
    Ready,
    Completed,
}

impl Model {
    /// Rank of each node: 0 for no dependencies, else 1 + the highest rank of its dependencies.
    /// The importer rejects cycles, so the recursion ends.
    pub fn ranks(&self) -> BTreeMap<NodeId, usize> {
        fn rank(m: &Model, id: &NodeId, out: &mut BTreeMap<NodeId, usize>, depth: usize) -> usize {
            if let Some(r) = out.get(id) {
                return *r;
            }
            let r = match m.nodes.get(id) {
                Some(spec) if depth <= m.nodes.len() => spec
                    .deps
                    .iter()
                    .map(|d| rank(m, &d.node, out, depth + 1) + 1)
                    .max()
                    .unwrap_or(0),
                _ => 0,
            };
            out.insert(id.clone(), r);
            r
        }
        let mut out = BTreeMap::new();
        for id in self.nodes.keys() {
            rank(self, id, &mut out, 0);
        }
        out
    }

    /// Node ids sorted by (rank, id): the order of every node effect.
    pub fn topo(&self) -> Vec<NodeId> {
        let ranks = self.ranks();
        let mut ids: Vec<NodeId> = self.nodes.keys().cloned().collect();
        ids.sort_by(|a, b| ranks[a].cmp(&ranks[b]).then_with(|| a.cmp(b)));
        ids
    }
}
