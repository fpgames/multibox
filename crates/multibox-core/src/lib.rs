//! The engine behind `git multibox`.
//!
//! Multibox gives a new git worktree everything a native one leaves behind:
//! the engine's import cache, generated files and IDE projects, so opening it
//! costs seconds instead of a full reimport. The crate is usable on its own, so
//! that editors, build systems and coding agents can call it directly.

pub mod adapter;
mod add;
pub mod git;
pub mod index;
mod plan;
mod remove;
mod rules;

pub use add::{AddOptions, AddOutcome, Mode, Observer, add};
pub use multibox_clone::{Capability, Progress, Report, Strategy};
pub use plan::{Plan, PlanSummary, build_plan};
pub use remove::{RemoveOptions, RemoveOutcome, purge_trash, remove, sweep_trash};
pub use rules::Rules;

use std::fmt;
use std::io;

#[derive(Debug)]
pub struct Error {
    message: String,
    unsupported: bool,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(message: impl Into<String>) -> Error {
        Error {
            message: message.into(),
            unsupported: false,
        }
    }

    /// A repository state the fast path does not handle. Callers fall back to a
    /// plain `git worktree add` instead of failing.
    pub fn unsupported(what: impl fmt::Display) -> Error {
        Error {
            message: format!("{what} is not supported"),
            unsupported: true,
        }
    }

    pub fn is_unsupported(&self) -> bool {
        self.unsupported
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error::new(err.to_string())
    }
}
