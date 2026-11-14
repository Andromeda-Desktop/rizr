//! The error contract: every error has a code, a message, a hint and an example command (VISION section 10).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    pub hint: String,
    pub example: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<Box<Cause>>,
}

/// The node an error is about, with runtime evidence.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Cause {
    pub node: String,
    /// The last healthcheck output, else the node's last log lines.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Usage,
    Untrusted,
    RuntimeUnavailable,
    ImportFailed,
    StartFailed,
    StartTimeout,
    JobFailed,
    Unhealthy,
    DependencyFailed,
    /// A node shares no network with a node it depends on: it can never reach it.
    Unreachable,
    NotBuilt,
    /// An `exec` job outlived the call's wait; `rizr wait` picks it up.
    StillRunning,
    /// No package or no command for the task (`rizr test`, `build`).
    NoTask,
    /// The task ran and exited non-zero: failing tests, a failed build.
    TaskFailed,
    /// A compose option gives a service the host; a human allows it in rizr.toml.
    Refused,
}

/// Exit code classes: an agent can branch on the class without parsing the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// 2: the command line is wrong.
    Usage,
    /// 3: a human must decide (trust, confirm).
    NeedsHuman,
    /// 4: the container runtime or a repo tool failed.
    Environment,
    /// 5: the stack did not reach ready.
    NotReady,
    /// 6: the command exists in the plan but is not built yet.
    NotBuilt,
    /// 7: a task ran and failed (tests, build).
    TaskFailed,
}

impl ErrorCode {
    pub fn class(self) -> ErrorClass {
        match self {
            ErrorCode::Usage | ErrorCode::NoTask => ErrorClass::Usage,
            ErrorCode::TaskFailed => ErrorClass::TaskFailed,
            ErrorCode::Untrusted | ErrorCode::Refused => ErrorClass::NeedsHuman,
            ErrorCode::RuntimeUnavailable | ErrorCode::ImportFailed => ErrorClass::Environment,
            ErrorCode::StartFailed
            | ErrorCode::StartTimeout
            | ErrorCode::JobFailed
            | ErrorCode::Unhealthy
            | ErrorCode::DependencyFailed
            | ErrorCode::Unreachable
            | ErrorCode::StillRunning => ErrorClass::NotReady,
            ErrorCode::NotBuilt => ErrorClass::NotBuilt,
        }
    }
}

impl ErrorClass {
    pub fn exit_code(self) -> u8 {
        match self {
            ErrorClass::Usage => 2,
            ErrorClass::NeedsHuman => 3,
            ErrorClass::Environment => 4,
            ErrorClass::NotReady => 5,
            ErrorClass::NotBuilt => 6,
            ErrorClass::TaskFailed => 7,
        }
    }
}

impl Error {
    pub fn new(
        code: ErrorCode,
        message: impl Into<String>,
        hint: impl Into<String>,
        example: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            hint: hint.into(),
            example: example.into(),
            cause: None,
        }
    }

    pub fn on(mut self, node: &str) -> Self {
        self.cause = Some(Box::new(Cause {
            node: node.to_owned(),
            output: String::new(),
        }));
        self
    }
}
