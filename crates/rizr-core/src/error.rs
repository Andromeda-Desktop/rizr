//! The error contract: every error has a code, a message, a hint and an example command (VISION section 10).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    pub hint: String,
    pub example: String,
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
    NotBuilt,
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
}

impl ErrorCode {
    pub fn class(self) -> ErrorClass {
        match self {
            ErrorCode::Usage => ErrorClass::Usage,
            ErrorCode::Untrusted => ErrorClass::NeedsHuman,
            ErrorCode::RuntimeUnavailable | ErrorCode::ImportFailed => ErrorClass::Environment,
            ErrorCode::StartFailed
            | ErrorCode::StartTimeout
            | ErrorCode::JobFailed
            | ErrorCode::Unhealthy
            | ErrorCode::DependencyFailed => ErrorClass::NotReady,
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
        }
    }
}
