//! The pure core of rizr: no IO, no clock. See .agent/SPEC-core.md.

pub mod code;
pub mod error;
pub mod event;
pub mod invariants;
pub mod machine;
pub mod model;
pub mod plan;
pub mod state;
pub mod view;

pub use event::{Action, Answer, Effect, Event, Op};
pub use plan::plan;
pub use state::{State, reduce};
