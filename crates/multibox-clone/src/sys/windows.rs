//! Windows backend. Reflinks use `FSCTL_DUPLICATE_EXTENTS_TO_FILE` (ReFS, and
//! therefore Dev Drive); everything else is a hand-rolled copy so that the
//! creation time, which git stores as `ctime`, survives on the same handle.

use crate::{Times, Volume};
use std::ffi::c_void;
use std::fs::{self, FileTimes, OpenOptions};
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{FileTimesExt, OpenOptionsExt, symlink_dir, symlink_file};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_FILE_EXISTS, ERROR_PATH_NOT_FOUND, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_HIDDEN,
    FILE_ATTRIBUTE_INTEGRITY_STREAM, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_NOT_CONTENT_INDEXED,
    FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SPARSE_FILE, FILE_ATTRIBUTE_SYSTEM, FILE_BASIC_INFO,
    FILE_BEGIN, FILE_END_OF_FILE_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_SEQUENTIAL_SCAN,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO,
    FILE_WRITE_ATTRIBUTES, FileBasicInfo, FileEndOfFileInfo, FileStandardInfo,
    GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetDriveTypeW, GetFileInformationByHandleEx,
    GetLogicalDrives, GetVolumeInformationW, GetVolumePathNameW, OPEN_EXISTING, ReadFile, SetFileInformationByHandle, SetFilePointerEx,
    WriteFile,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

// From winnt.h / winioctl.h; not all of these are exposed by windows-sys.
const FILE_SUPPORTS_BLOCK_REFCOUNTING: u32 = 0x0800_0000;
const FSCTL_DUPLICATE_EXTENTS_TO_FILE: u32 = 0x0009_8344;
const FSCTL_GET_INTEGRITY_INFORMATION: u32 = 0x0009_027C;
const FSCTL_SET_INTEGRITY_INFORMATION: u32 = 0x0009_C280;
const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;

/// A single duplicate-extents call must stay under 4 GiB.
const MAX_CLONE_CHUNK: u64 = 1 << 30;

#[repr(C)]
struct DuplicateExtentsData {
    file_handle: HANDLE,
    source_file_offset: i64,
    target_file_offset: i64,
    byte_count: i64,
}

#[repr(C)]
#[derive(Default)]
struct GetIntegrityBuffer {
    checksum_algorithm: u16,
    reserved: u16,
    flags: u32,
    checksum_chunk_size: u32,
    cluster_size: u32,
}

#[repr(C)]
struct SetIntegrityBuffer {
    checksum_algorithm: u16,
    reserved: u16,
    flags: u32,
}

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

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn check(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
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

fn volume_root(path: &Path) -> io::Result<Vec<u16>> {
    let path = wide(&existing_ancestor(path));
    let mut root = vec![0u16; path.len().max(64) + 2];
    check(unsafe { GetVolumePathNameW(path.as_ptr(), root.as_mut_ptr(), root.len() as u32) })?;
    let end = root.iter().position(|unit| *unit == 0).unwrap_or(root.len());
    root.truncate(end + 1);
    Ok(root)
}

struct VolumeInfo {
    serial: u32,
    flags: u32,
    filesystem: String,
}

fn volume_info(root: &[u16]) -> io::Result<VolumeInfo> {
    let (mut serial, mut max_name, mut flags) = (0u32, 0u32, 0u32);
    let mut name = [0u16; 64];
    check(unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            null_mut(),
            0,
            &mut serial,
            &mut max_name,
            &mut flags,
            name.as_mut_ptr(),
            name.len() as u32,
        )
    })?;
    let end = name.iter().position(|unit| *unit == 0).unwrap_or(name.len());
    Ok(VolumeInfo {
        serial,
        flags,
        filesystem: String::from_utf16_lossy(&name[..end]),
    })
}

fn cluster_size(root: &[u16]) -> io::Result<u32> {
    let (mut sectors, mut bytes, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
    check(unsafe { GetDiskFreeSpaceW(root.as_ptr(), &mut sectors, &mut bytes, &mut free, &mut total) })?;
    Ok(sectors * bytes)
}

/// Reports what the filesystem can do between `src` and `dst`. Either path may
/// not exist yet; the nearest existing ancestor stands in for it.
pub fn probe(src: &Path, dst: &Path) -> io::Result<Capability> {
    let src_root = volume_root(src)?;
    let dst_root = volume_root(dst)?;
    let src_info = volume_info(&src_root)?;
    let dst_info = volume_info(&dst_root)?;
    let same_volume = src_info.serial == dst_info.serial;
    Ok(Capability {
        reflink: same_volume && dst_info.flags & FILE_SUPPORTS_BLOCK_REFCOUNTING != 0,
        same_volume,
        filesystem: dst_info.filesystem,
        cluster_size: cluster_size(&dst_root)?,
    })
}

/// Every lettered volume on the machine. Removable or unready drives are listed
/// only if they answer.
pub fn volumes() -> Vec<Volume> {
    const DRIVE_FIXED: u32 = 3;
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|bit| mask & (1 << bit) != 0)
        .filter_map(|bit| {
            let root = format!("{}:\\", (b'A' + bit) as char);
            let wide_root: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
            let info = volume_info(&wide_root).ok()?;
            let (mut free, mut total) = (0u64, 0u64);
            check(unsafe {
                GetDiskFreeSpaceExW(wide_root.as_ptr(), &mut free, &mut total, null_mut())
            })
            .ok()?;
            Some(Volume {
                root,
                filesystem: info.filesystem,
                reflink: info.flags & FILE_SUPPORTS_BLOCK_REFCOUNTING != 0,
                free,
                total,
                fixed: unsafe { GetDriveTypeW(wide_root.as_ptr()) } == DRIVE_FIXED,
            })
        })
        .collect()
}

/// Bytes available to the caller on the volume holding `path`.
pub fn free_space(path: &Path) -> io::Result<u64> {
    let root = volume_root(path)?;
    let mut available = 0u64;
    check(unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut available, null_mut(), null_mut()) })?;
    Ok(available)
}

fn open_source(path: &Path) -> io::Result<Handle> {
    let path = wide(path);
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_SEQUENTIAL_SCAN,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(Handle(handle))
    }
}

fn create_target(path: &Path) -> io::Result<Handle> {
    let path = wide(path);
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE | FILE_WRITE_ATTRIBUTES,
            0,
            null(),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let err = io::Error::last_os_error();
        // CREATE_NEW reports a missing parent as ERROR_PATH_NOT_FOUND, which
        // must not be mistaken for the source having vanished.
        if err.raw_os_error() == Some(ERROR_PATH_NOT_FOUND as i32) {
            return Err(io::Error::other(format!("target directory missing: {err}")));
        }
        if err.raw_os_error() == Some(ERROR_FILE_EXISTS as i32) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, err));
        }
        Err(err)
    } else {
        Ok(Handle(handle))
    }
}

fn set_length(file: &Handle, length: u64) -> io::Result<()> {
    let info = FILE_END_OF_FILE_INFO {
        EndOfFile: length as i64,
    };
    check(unsafe {
        SetFileInformationByHandle(
            file.0,
            FileEndOfFileInfo,
            &info as *const _ as *const c_void,
            size_of::<FILE_END_OF_FILE_INFO>() as u32,
        )
    })
}

fn ioctl(file: &Handle, code: u32, input: *const c_void, input_len: usize) -> io::Result<()> {
    let mut returned = 0u32;
    check(unsafe {
        DeviceIoControl(
            file.0,
            code,
            input,
            input_len as u32,
            null_mut(),
            0,
            &mut returned,
            null_mut(),
        )
    })
}

fn match_integrity(source: &Handle, target: &Handle) -> io::Result<()> {
    let mut info = GetIntegrityBuffer::default();
    let mut returned = 0u32;
    check(unsafe {
        DeviceIoControl(
            source.0,
            FSCTL_GET_INTEGRITY_INFORMATION,
            null(),
            0,
            &mut info as *mut _ as *mut c_void,
            size_of::<GetIntegrityBuffer>() as u32,
            &mut returned,
            null_mut(),
        )
    })?;
    let set = SetIntegrityBuffer {
        checksum_algorithm: info.checksum_algorithm,
        reserved: 0,
        flags: info.flags,
    };
    ioctl(
        target,
        FSCTL_SET_INTEGRITY_INFORMATION,
        &set as *const _ as *const c_void,
        size_of::<SetIntegrityBuffer>(),
    )
}

fn reflink(
    source: &Handle,
    target: &Handle,
    size: u64,
    attributes: u32,
    cluster: u32,
) -> io::Result<()> {
    if attributes & FILE_ATTRIBUTE_SPARSE_FILE != 0 {
        ioctl(target, FSCTL_SET_SPARSE, null(), 0)?;
    }
    if attributes & FILE_ATTRIBUTE_INTEGRITY_STREAM != 0 {
        match_integrity(source, target)?;
    }
    set_length(target, size)?;
    // The range must be cluster-aligned; rounding past end-of-file is allowed.
    let cluster = u64::from(cluster.max(1));
    let rounded = size.div_ceil(cluster) * cluster;
    let mut offset = 0;
    while offset < rounded {
        let count = (rounded - offset).min(MAX_CLONE_CHUNK);
        let data = DuplicateExtentsData {
            file_handle: source.0,
            source_file_offset: offset as i64,
            target_file_offset: offset as i64,
            byte_count: count as i64,
        };
        ioctl(
            target,
            FSCTL_DUPLICATE_EXTENTS_TO_FILE,
            &data as *const _ as *const c_void,
            size_of::<DuplicateExtentsData>(),
        )?;
        offset += count;
    }
    Ok(())
}

fn copy(source: &Handle, target: &Handle, size: u64, buffer: &mut [u8]) -> io::Result<()> {
    if size >= 1 << 20 {
        set_length(target, size)?;
    }
    loop {
        let mut read = 0u32;
        check(unsafe {
            ReadFile(
                source.0,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                &mut read,
                null_mut(),
            )
        })?;
        if read == 0 {
            return Ok(());
        }
        let mut pending = &buffer[..read as usize];
        while !pending.is_empty() {
            let mut written = 0u32;
            check(unsafe {
                WriteFile(
                    target.0,
                    pending.as_ptr(),
                    pending.len() as u32,
                    &mut written,
                    null_mut(),
                )
            })?;
            pending = &pending[written as usize..];
        }
    }
}

fn rewind(file: &Handle) -> io::Result<()> {
    set_length(file, 0)?;
    check(unsafe { SetFilePointerEx(file.0, 0, null_mut(), FILE_BEGIN) })
}

fn clone_into(
    source: &Handle,
    target: &Handle,
    capability: &Capability,
    buffer: &mut [u8],
) -> io::Result<Outcome> {
    let mut basic: FILE_BASIC_INFO = unsafe { zeroed() };
    let mut standard: FILE_STANDARD_INFO = unsafe { zeroed() };
    check(unsafe {
        GetFileInformationByHandleEx(
            source.0,
            FileBasicInfo,
            &mut basic as *mut _ as *mut c_void,
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    })?;
    check(unsafe {
        GetFileInformationByHandleEx(
            source.0,
            FileStandardInfo,
            &mut standard as *mut _ as *mut c_void,
            size_of::<FILE_STANDARD_INFO>() as u32,
        )
    })?;
    let size = standard.EndOfFile as u64;

    let outcome = if capability.reflink && size > 0 {
        match reflink(source, target, size, basic.FileAttributes, capability.cluster_size) {
            Ok(()) => Outcome::Reflinked,
            Err(_) => {
                rewind(target)?;
                copy(source, target, size, buffer)?;
                Outcome::Fallback
            }
        }
    } else {
        copy(source, target, size, buffer)?;
        Outcome::Copied
    };

    basic.FileAttributes &= FILE_ATTRIBUTE_READONLY
        | FILE_ATTRIBUTE_HIDDEN
        | FILE_ATTRIBUTE_SYSTEM
        | FILE_ATTRIBUTE_ARCHIVE
        | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;
    if basic.FileAttributes == 0 {
        basic.FileAttributes = FILE_ATTRIBUTE_NORMAL;
    }
    check(unsafe {
        SetFileInformationByHandle(
            target.0,
            FileBasicInfo,
            &basic as *const _ as *const c_void,
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    })?;
    Ok(outcome)
}

pub fn clone_file(
    src: &Path,
    dst: &Path,
    capability: &Capability,
    buffer: &mut [u8],
) -> io::Result<Outcome> {
    let source = open_source(src)?;
    let target = create_target(dst)?;
    let result = clone_into(&source, &target, capability, buffer);
    drop(target);
    if result.is_err() {
        let _ = fs::remove_file(dst);
    }
    result
}

pub fn clone_link(src: &Path, dst: &Path) -> io::Result<()> {
    let target = fs::read_link(src)?;
    if fs::metadata(src).map(|meta| meta.is_dir()).unwrap_or(false) {
        symlink_dir(target, dst)
    } else {
        symlink_file(target, dst)
    }
}

pub fn set_dir_times(path: &Path, times: &Times) -> io::Result<()> {
    let dir = OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let mut update = FileTimes::new().set_modified(times.modified);
    if let Some(created) = times.created {
        update = update.set_created(created);
    }
    dir.set_times(update)
}
