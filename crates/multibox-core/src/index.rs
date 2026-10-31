//! Reader and rewriter for the git index (`.git/index`), versions 2 to 4.
//!
//! Reusing the source worktree's index is what makes the first `git status` in
//! a new worktree instant: git only re-hashes a file when its cached stat data
//! disagrees with the disk. This module keeps entries byte-for-byte, drops the
//! extensions that describe one specific worktree, and can patch stat fields on
//! platforms where a clone cannot keep them.

use crate::{Error, Result};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::fs;
use std::path::Path;

const HEADER_LEN: usize = 12;
const STAT_LEN: usize = 40;
const FLAG_EXTENDED: u16 = 0x4000;
const EXT_FLAG_SKIP_WORKTREE: u16 = 0x4000;
const MODE_TYPE_MASK: u32 = 0o170000;
const MODE_SYMLINK: u32 = 0o120000;
const MODE_GITLINK: u32 = 0o160000;
const MODE_SPARSE_DIR: u32 = 0o040000;

#[derive(Clone, Debug)]
pub struct Entry {
    offset: usize,
    pub path: String,
    pub mode: u32,
    pub size: u32,
    pub stage: u8,
    pub skip_worktree: bool,
}

impl Entry {
    pub fn is_gitlink(&self) -> bool {
        self.mode & MODE_TYPE_MASK == MODE_GITLINK
    }

    pub fn is_symlink(&self) -> bool {
        self.mode & MODE_TYPE_MASK == MODE_SYMLINK
    }
}

/// The stat fields of an index entry, in on-disk order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    pub ctime_sec: u32,
    pub ctime_nsec: u32,
    pub mtime_sec: u32,
    pub mtime_nsec: u32,
    pub dev: u32,
    pub ino: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
}

#[derive(Debug)]
pub struct Index {
    data: Vec<u8>,
    version: u32,
    hash_len: usize,
    extensions_start: usize,
    pub entries: Vec<Entry>,
}

fn be32(data: &[u8], offset: usize) -> Result<u32> {
    data.get(offset..offset + 4)
        .map(|bytes| u32::from_be_bytes(bytes.try_into().unwrap()))
        .ok_or_else(|| Error::new("index is truncated"))
}

fn be16(data: &[u8], offset: usize) -> Result<u16> {
    data.get(offset..offset + 2)
        .map(|bytes| u16::from_be_bytes(bytes.try_into().unwrap()))
        .ok_or_else(|| Error::new("index is truncated"))
}

/// The offset-encoded varint used for path prefixes in index version 4.
fn varint(data: &[u8], offset: &mut usize) -> Result<usize> {
    let mut next = || {
        let byte = *data
            .get(*offset)
            .ok_or_else(|| Error::new("index is truncated"))?;
        *offset += 1;
        Ok::<u8, Error>(byte)
    };
    let mut byte = next()?;
    let mut value = usize::from(byte & 0x7f);
    while byte & 0x80 != 0 {
        byte = next()?;
        value = ((value + 1) << 7) + usize::from(byte & 0x7f);
    }
    Ok(value)
}

fn nul_terminated(data: &[u8], start: usize) -> Result<usize> {
    data[start..]
        .iter()
        .position(|byte| *byte == 0)
        .map(|at| start + at)
        .ok_or_else(|| Error::new("index path is not terminated"))
}

impl Index {
    /// `hash_len` is 20 for SHA-1 repositories and 32 for SHA-256 ones.
    pub fn read(path: &Path, hash_len: usize) -> Result<Index> {
        Index::parse(fs::read(path)?, hash_len)
    }

    pub fn parse(data: Vec<u8>, hash_len: usize) -> Result<Index> {
        if data.len() < HEADER_LEN + hash_len || &data[..4] != b"DIRC" {
            return Err(Error::new("not a git index"));
        }
        let version = be32(&data, 4)?;
        if !(2..=4).contains(&version) {
            return Err(Error::new(format!("unsupported index version {version}")));
        }
        let count = be32(&data, 8)? as usize;
        let fixed = STAT_LEN + hash_len + 2;
        let mut entries = Vec::with_capacity(count);
        let mut offset = HEADER_LEN;
        let mut previous: Vec<u8> = Vec::new();

        for _ in 0..count {
            let start = offset;
            let mode = be32(&data, start + 24)?;
            let size = be32(&data, start + 36)?;
            let flags = be16(&data, start + STAT_LEN + hash_len)?;
            let mut cursor = start + fixed;
            let mut skip_worktree = false;
            if flags & FLAG_EXTENDED != 0 {
                skip_worktree = be16(&data, cursor)? & EXT_FLAG_SKIP_WORKTREE != 0;
                cursor += 2;
            }
            if mode & MODE_TYPE_MASK == MODE_SPARSE_DIR {
                return Err(Error::unsupported("sparse index"));
            }

            let name: Vec<u8>;
            if version == 4 {
                let strip = varint(&data, &mut cursor)?;
                let end = nul_terminated(&data, cursor)?;
                let keep = previous
                    .len()
                    .checked_sub(strip)
                    .ok_or_else(|| Error::new("index path prefix is invalid"))?;
                let mut full = previous[..keep].to_vec();
                full.extend_from_slice(&data[cursor..end]);
                name = full;
                offset = end + 1;
            } else {
                let end = nul_terminated(&data, cursor)?;
                name = data[cursor..end].to_vec();
                let length = end - start;
                offset = start + (length + 8) / 8 * 8;
            }
            let path = String::from_utf8(name.clone())
                .map_err(|_| Error::unsupported("non-UTF-8 path in index"))?;
            previous = name;
            entries.push(Entry {
                offset: start,
                path,
                mode,
                size,
                stage: ((flags >> 12) & 0x3) as u8,
                skip_worktree,
            });
        }

        let index = Index {
            data,
            version,
            hash_len,
            extensions_start: offset,
            entries,
        };
        index.check_extensions()?;
        Ok(index)
    }

    fn extensions(&self) -> Result<Vec<(&[u8], usize, usize)>> {
        let end = self.data.len() - self.hash_len;
        let mut offset = self.extensions_start;
        let mut found = Vec::new();
        while offset < end {
            let signature = self
                .data
                .get(offset..offset + 4)
                .ok_or_else(|| Error::new("index extension is truncated"))?;
            let size = be32(&self.data, offset + 4)? as usize;
            let next = offset + 8 + size;
            if next > end {
                return Err(Error::new("index extension is truncated"));
            }
            found.push((signature, offset, next));
            offset = next;
        }
        Ok(found)
    }

    fn check_extensions(&self) -> Result<()> {
        for (signature, _, _) in self.extensions()? {
            match signature {
                b"link" => return Err(Error::unsupported("split index")),
                b"sdir" => return Err(Error::unsupported("sparse index")),
                // A lowercase first letter marks an extension git must understand.
                other if other[0].is_ascii_lowercase() => {
                    return Err(Error::unsupported(format!(
                        "index extension {}",
                        String::from_utf8_lossy(other)
                    )));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Overwrites the stat data of entry `at`.
    pub fn set_stat(&mut self, at: usize, stat: &Stat) {
        let offset = self.entries[at].offset;
        let mode = self.data[offset + 24..offset + 28].to_vec();
        let fields = [
            stat.ctime_sec,
            stat.ctime_nsec,
            stat.mtime_sec,
            stat.mtime_nsec,
            stat.dev,
            stat.ino,
        ];
        for (i, field) in fields.iter().enumerate() {
            self.data[offset + i * 4..offset + i * 4 + 4].copy_from_slice(&field.to_be_bytes());
        }
        self.data[offset + 24..offset + 28].copy_from_slice(&mode);
        self.data[offset + 28..offset + 32].copy_from_slice(&stat.uid.to_be_bytes());
        self.data[offset + 32..offset + 36].copy_from_slice(&stat.gid.to_be_bytes());
        self.data[offset + 36..offset + 40].copy_from_slice(&stat.size.to_be_bytes());
    }

    /// Serialises the index for use by a different worktree.
    ///
    /// Only the cache tree survives: it describes content. Fsmonitor tokens and
    /// the untracked cache describe one directory on disk, the offset tables
    /// describe this exact byte layout, and resolve-undo belongs to the source.
    pub fn to_worktree_bytes(&self) -> Result<Vec<u8>> {
        let mut out = self.data[..self.extensions_start].to_vec();
        for (signature, start, end) in self.extensions()? {
            if signature == b"TREE" {
                out.extend_from_slice(&self.data[start..end]);
            }
        }
        let trailer = &self.data[self.data.len() - self.hash_len..];
        if trailer.iter().all(|byte| *byte == 0) {
            // index.skipHash: an all-zero trailer means "not computed".
            out.extend_from_slice(trailer);
        } else if self.hash_len == 32 {
            let digest = Sha256::digest(&out);
            out.extend_from_slice(&digest);
        } else {
            let digest = Sha1::digest(&out);
            out.extend_from_slice(&digest);
        }
        Ok(out)
    }

    pub fn version(&self) -> u32 {
        self.version
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, mode: u32, flags_extra: Option<u16>) -> Vec<u8> {
        let mut out = vec![0u8; STAT_LEN + 20];
        out[24..28].copy_from_slice(&mode.to_be_bytes());
        out[36..40].copy_from_slice(&7u32.to_be_bytes());
        let mut flags = path.len() as u16;
        if flags_extra.is_some() {
            flags |= FLAG_EXTENDED;
        }
        out.extend_from_slice(&flags.to_be_bytes());
        if let Some(extra) = flags_extra {
            out.extend_from_slice(&extra.to_be_bytes());
        }
        out.extend_from_slice(path.as_bytes());
        out.push(0);
        while out.len() % 8 != 0 {
            out.push(0);
        }
        out
    }

    fn build(version: u32, entries: &[Vec<u8>], extensions: &[u8]) -> Vec<u8> {
        let mut out = b"DIRC".to_vec();
        out.extend_from_slice(&version.to_be_bytes());
        out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for bytes in entries {
            out.extend_from_slice(bytes);
        }
        out.extend_from_slice(extensions);
        let digest = Sha1::digest(&out);
        out.extend_from_slice(&digest);
        out
    }

    fn extension(signature: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = signature.to_vec();
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn reads_v2_entries() {
        let data = build(
            2,
            &[
                entry("a.txt", 0o100644, None),
                entry("dir/link", MODE_SYMLINK, None),
                entry("sub", MODE_GITLINK, None),
            ],
            &[],
        );
        let index = Index::parse(data, 20).unwrap();
        let paths: Vec<_> = index.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "dir/link", "sub"]);
        assert!(index.entries[1].is_symlink());
        assert!(index.entries[2].is_gitlink());
        assert_eq!(index.entries[0].size, 7);
    }

    #[test]
    fn reads_extended_flags() {
        let data = build(
            3,
            &[
                entry("a", 0o100644, Some(EXT_FLAG_SKIP_WORKTREE)),
                entry("b", 0o100644, None),
            ],
            &[],
        );
        let index = Index::parse(data, 20).unwrap();
        assert!(index.entries[0].skip_worktree);
        assert!(!index.entries[1].skip_worktree);
    }

    #[test]
    fn reads_v4_prefix_compression() {
        let mut body = Vec::new();
        for (strip, suffix) in [(0u8, "dir/alpha"), (5, "beta"), (8, "z")] {
            let mut bytes = vec![0u8; STAT_LEN + 20];
            bytes[24..28].copy_from_slice(&0o100644u32.to_be_bytes());
            bytes.extend_from_slice(&(suffix.len() as u16).to_be_bytes());
            bytes.push(strip);
            bytes.extend_from_slice(suffix.as_bytes());
            bytes.push(0);
            body.push(bytes);
        }
        let index = Index::parse(build(4, &body, &[]), 20).unwrap();
        let paths: Vec<_> = index.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["dir/alpha", "dir/beta", "z"]);
    }

    #[test]
    fn keeps_only_the_cache_tree() {
        let tree = extension(b"TREE", b"tree-data");
        let mut extensions = tree.clone();
        extensions.extend(extension(b"UNTR", b"untracked"));
        extensions.extend(extension(b"FSMN", b"token"));
        let index =
            Index::parse(build(2, &[entry("a", 0o100644, None)], &extensions), 20).unwrap();
        let out = index.to_worktree_bytes().unwrap();
        let reread = Index::parse(out.clone(), 20).unwrap();
        assert_eq!(reread.entries.len(), 1);
        assert!(out.windows(4).any(|bytes| bytes == b"TREE"));
        assert!(!out.windows(4).any(|bytes| bytes == b"UNTR"));
        assert!(!out.windows(4).any(|bytes| bytes == b"FSMN"));
        let (body, trailer) = out.split_at(out.len() - 20);
        assert_eq!(Sha1::digest(body).as_slice(), trailer);
    }

    #[test]
    fn set_stat_keeps_mode_and_path() {
        let mut index = Index::parse(build(2, &[entry("a", 0o100644, None)], &[]), 20).unwrap();
        let stat = Stat {
            mtime_sec: 42,
            ino: 9,
            size: 100,
            ..Stat::default()
        };
        index.set_stat(0, &stat);
        let reread = Index::parse(index.to_worktree_bytes().unwrap(), 20).unwrap();
        assert_eq!(reread.entries[0].mode, 0o100644);
        assert_eq!(reread.entries[0].size, 100);
        assert_eq!(reread.entries[0].path, "a");
    }

    #[test]
    fn rejects_split_index() {
        let data = build(
            2,
            &[entry("a", 0o100644, None)],
            &extension(b"link", &[0; 20]),
        );
        assert!(Index::parse(data, 20).unwrap_err().is_unsupported());
    }
}
