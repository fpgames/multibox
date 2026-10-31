//! macOS and Linux backend: `clonefile` on APFS, `FICLONE` on Btrfs and XFS.

use crate::{Times, Volume};
use std::ffi::CString;
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Capability {
    pub reflink: bool,
    pub same_volume: bool,
    pub filesystem: String,
    pub cluster_size: u32,
}

pub enum Outcome {
    Reflinked,
    Copied,
    Fallback,
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

fn existing_ancestor(path: &Path) -> PathBuf {
    let mut current = path;
    loop {
        if current.exists() {
            return current.to_path_buf();
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return path.to_path_buf(),
        }
    }
}

fn statfs(path: &Path) -> io::Result<libc::statfs> {
    let path = c_path(path)?;
    let mut info = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    if unsafe { libc::statfs(path.as_ptr(), info.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { info.assume_init() })
}

#[cfg(target_os = "macos")]
fn filesystem_name(info: &libc::statfs) -> String {
    let name: Vec<u8> = info
        .f_fstypename
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    String::from_utf8_lossy(&name).into_owned()
}

#[cfg(not(target_os = "macos"))]
// The width of f_type differs between targets, so the cast is not always a no-op.
#[allow(clippy::unnecessary_cast)]
fn filesystem_name(info: &libc::statfs) -> String {
    match info.f_type as i64 {
        0x9123_683E => "btrfs".to_owned(),
        0x5846_5342 => "xfs".to_owned(),
        0xEF53 => "ext4".to_owned(),
        0x0102_1994 => "tmpfs".to_owned(),
        other => format!("{other:#x}"),
    }
}

#[cfg(target_os = "macos")]
fn reflink_file(src: &Path, dst: &Path) -> io::Result<()> {
    const CLONE_NOFOLLOW: u32 = 0x0001;
    const CLONE_NOOWNERCOPY: u32 = 0x0002;
    let (src, dst) = (c_path(src)?, c_path(dst)?);
    let flags = CLONE_NOFOLLOW | CLONE_NOOWNERCOPY;
    if unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
fn reflink_file(src: &Path, dst: &Path) -> io::Result<()> {
    const FICLONE: libc::c_ulong = 0x4004_9409;
    let source = File::open(src)?;
    let target = OpenOptions::new().write(true).create_new(true).open(dst)?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::ioctl(target.as_raw_fd(), FICLONE as _, source.as_raw_fd()) } == 0 {
        apply_metadata(&source, &target)
    } else {
        let err = io::Error::last_os_error();
        let _ = fs::remove_file(dst);
        Err(err)
    }
}

#[cfg(not(target_os = "macos"))]
fn apply_metadata(source: &File, target: &File) -> io::Result<()> {
    let meta = source.metadata()?;
    target.set_permissions(meta.permissions())?;
    target.set_times(FileTimes::new().set_modified(meta.modified()?))
}

/// Reports what the filesystem can do between `src` and `dst`. A reflink is
/// attempted for real, because support depends on mount options and on-disk
/// format, not just the filesystem's name.
pub fn probe(src: &Path, dst: &Path) -> io::Result<Capability> {
    let src = existing_ancestor(src);
    let dst = existing_ancestor(dst);
    let same_volume = fs::metadata(&src)?.dev() == fs::metadata(&dst)?.dev();
    let info = statfs(&dst)?;
    let mut reflink = false;
    if same_volume {
        let id = std::process::id();
        let first = dst.join(format!(".multibox-probe-{id}-a"));
        let second = dst.join(format!(".multibox-probe-{id}-b"));
        if fs::write(&first, b"multibox").is_ok() {
            reflink = reflink_file(&first, &second).is_ok();
        }
        let _ = fs::remove_file(&first);
        let _ = fs::remove_file(&second);
    }
    Ok(Capability {
        reflink,
        same_volume,
        filesystem: filesystem_name(&info),
        cluster_size: info.f_bsize as u32,
    })
}

/// Unix volumes need no setup, so there is nothing to enumerate.
pub fn volumes() -> Vec<Volume> {
    Vec::new()
}

/// Bytes available to the caller on the volume holding `path`.
pub fn free_space(path: &Path) -> io::Result<u64> {
    let info = statfs(&existing_ancestor(path))?;
    Ok(info.f_bavail as u64 * info.f_bsize as u64)
}

fn copy(src: &Path, dst: &Path) -> io::Result<()> {
    let mut source = File::open(src)?;
    let meta = source.metadata()?;
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(meta.permissions().mode())
        .open(dst)?;
    io::copy(&mut source, &mut target)?;
    target.set_times(FileTimes::new().set_modified(meta.modified()?))
}

pub fn clone_file(
    src: &Path,
    dst: &Path,
    capability: &Capability,
    _buffer: &mut [u8],
) -> io::Result<Outcome> {
    if capability.reflink {
        match reflink_file(src, dst) {
            Ok(()) => return Ok(Outcome::Reflinked),
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(err),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => return Err(err),
            Err(_) => {
                copy(src, dst)?;
                return Ok(Outcome::Fallback);
            }
        }
    }
    copy(src, dst)?;
    Ok(Outcome::Copied)
}

pub fn create_link(_src: &Path, dst: &Path, target: &Path) -> io::Result<()> {
    symlink(target, dst)
}

pub fn set_dir_times(path: &Path, times: &Times) -> io::Result<()> {
    File::open(path)?.set_times(FileTimes::new().set_modified(times.modified))
}
