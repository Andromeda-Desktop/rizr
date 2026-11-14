//! The code graph (VISION section 3): packages and the tasks that build, test, lint or migrate them.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Manager {
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PyTool {
    Uv,
    Poetry,
    Pip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Kind {
    Cargo,
    Node { manager: Manager },
    Python { tool: PyTool },
    Go,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum TaskName {
    Build,
    Test,
    Lint,
    Migrate,
}

/// Where a task comes from, weakest first: a stronger source wins (SPEC-d2 section 2).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Default,
    Script,
    Runner,
    Ci,
    Docs,
    Manifest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Task {
    /// A shell command line, run in the package directory.
    pub cmd: String,
    pub source: Source,
    /// The repo-relative file the task was read from.
    pub from: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Package {
    /// Repo-relative directory; "." is the root.
    pub dir: String,
    #[serde(flatten)]
    pub kind: Kind,
    /// Every candidate per task, strongest first.
    pub tasks: BTreeMap<TaskName, Vec<Task>>,
}

impl Package {
    /// The task that runs: the candidate from the strongest source.
    pub fn task(&self, name: TaskName) -> Option<&Task> {
        self.tasks.get(&name)?.first()
    }

    /// Adds a candidate and keeps the list strongest first; the first one read wins a tie.
    pub fn add(&mut self, name: TaskName, task: Task) {
        let list = self.tasks.entry(name).or_default();
        let at = list.partition_point(|t| t.source >= task.source);
        list.insert(at, task);
    }
}
