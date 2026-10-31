//! Parallel, metadata-preserving tree cloning.
//!
//! The crate has one job: reproduce a selected set of files from one directory
//! in another, as cheaply as the filesystem allows, keeping the timestamps that
//! git and game engines use to decide whether a file changed.
//!
//! The work happens in two phases so callers can inspect the cost first:
//!
//! 1. [`scan`] expands a [`ScanRequest`] into a [`Manifest`].
//! 2. [`execute`] materialises the manifest, using reflinks where the volume
//!    supports them (ReFS, APFS, Btrfs, XFS) and a plain copy where it does not.
//!
//! Existing destination files are never overwritten: every file is created
//! exclusively, so a stray target cannot be silently clobbered.

mod exec;
mod purge;
mod scan;
mod sync;

#[cfg_attr(windows, path = "sys/windows.rs")]
#[cfg_attr(unix, path = "sys/unix.rs")]
mod sys;

pub use exec::{Failure, Options, Progress, Report, Strategy, execute};
pub use purge::{PurgeReport, purge};
pub use scan::{Decision, DirEntry, FileEntry, LinkEntry, Manifest, ScanRequest, TreeFilter, scan};
pub use sync::{SyncReport, sync};
pub use sys::{Capability, free_space, os_build, probe, volumes};

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A mounted volume, as seen by `git multibox doctor` and `setup`.
#[derive(Clone, Debug)]
pub struct Volume {
    pub root: String,
    pub filesystem: String,
    pub reflink: bool,
    pub free: u64,
    pub total: u64,
    pub fixed: bool,
}

/// Timestamps of a directory, restored after its contents are in place.
#[derive(Clone, Copy, Debug)]
pub struct Times {
    pub modified: SystemTime,
    pub created: Option<SystemTime>,
}

/// Joins a `/`-separated relative path onto `root`. Verbatim Windows paths do
/// not normalise separators, so the conversion has to happen here.
pub(crate) fn join(root: &Path, rel: &str) -> PathBuf {
    if cfg!(windows) {
        root.join(rel.replace('/', "\\"))
    } else {
        root.join(rel)
    }
}

pub(crate) fn thread_count(requested: Option<usize>) -> usize {
    requested
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(4)
                .min(32)
        })
        .max(1)
}
