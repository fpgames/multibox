# Benchmarks

Measured on a Windows 11 workstation (32 threads, Gen4 NVMe, NTFS) against a
Unity 6 project with 104,330 tracked files, 44,747 of them in Git LFS, and a
71 GiB `Library/`. Each row ran once, with the file cache warm from earlier
reads, so treat the numbers as indicative rather than precise.

## Copy mode on NTFS

| Operation | Native `git worktree` | Multibox |
| --- | --- | --- |
| Create, tracked files only (10.3 GiB) | 204.8 s | 63.9 s |
| Create, with caches (266,190 files, 75.4 GiB) | not possible | 187.0 s |
| First `git status` | 0.43 s | 0.10 s |
| Remove, 104,330 files | 18.3 s | 0.30 s |
| Remove, 611,067 files | not measured | 0.37 s |

The native worktree has no `Library/` and is missing ignored generated assets,
so Unity must still run a full import before it is usable. A native checkout is
slower because every LFS file goes through the smudge filter, while Multibox
copies the bytes already on disk.

`remove` returns once the worktree is detached. The two removals above finished
deleting in the background about 41 seconds later.

## Still to measure

- First launch of Unity in a Multibox worktree, against a native one.
- Reflink mode on a Dev Drive: creation time and real disk usage.
- macOS and Linux.
