# Benchmarks

Measured on a Windows 11 workstation (32 threads, 61 GiB RAM, Gen4 NVMe) against a
Unity 6 project with 104,330 tracked files, 44,747 of them in Git LFS, 56,000
assets and a 71 GiB `Library/`. The main editor stayed open throughout, as it does
in daily work. Most rows ran once or twice with the file cache warm, so treat them
as indicative rather than precise. The Dev Drive sits in a dynamic VHDX on NVMe.

## From nothing to a usable worktree

| | Native `git worktree` | Multibox on NTFS | Multibox on Dev Drive |
| --- | --- | --- | --- |
| Create | 204.8 s | 62.0 s | 44 to 46 s |
| First Unity launch | 1127.3 s | 51.7 s | 51.9 s |
| **Total** | **22 minutes** | **2 minutes** | **1.6 minutes** |
| Assets imported | 56,960 | 1,076 | 1,076 |
| Disk per worktree | 52 GiB | 56 GiB | 2.1 to 2.7 GiB |
| Ignored generated assets | missing | present | present |
| Reopening later | not measured | not measured | 14.1 s |

The native worktree also needs a generation step for what it is missing. On the
Dev Drive, the worktree grew by 0.07 GiB after Unity imported and compiled in it.

Multibox's 1,076 imports came from the source project, not from cloning: the
long-running editor's `Library/` had not caught up with the latest commits. Cloning
from a project whose `Library/` is up to date removes them:

| First launch of a clone | Time | Imports |
| --- | --- | --- |
| Source `Library/` behind the working tree | 52 s | 1,076 |
| Source `Library/` up to date | 29.8 s | 0 |
| Reopening the same worktree (baseline) | 13.7 s | 0 |

The difference between the last two rows is one rebuild of the script build graph,
which embeds absolute paths.

## Why it is shaped this way

File creation dominates, not data. Measured on 93,065 files (37.8 GiB):

| Method | Files per second | Disk used |
| --- | --- | --- |
| `CopyFileExW` on Dev Drive, Windows 11 24H2 and later | 7,216 | 0.53 GiB |
| `FSCTL_DUPLICATE_EXTENTS_TO_FILE` on Dev Drive | 3,321 | shared |

And on 39,749 small files on NTFS:

| Method | Files per second |
| --- | --- |
| `CopyFileExW` | 8,589 to 9,050 |
| Hand-written read and write loop | 1,413 to 2,670 |

So Multibox calls the system copy, which clones by itself where it can. Copying
small files instead of cloning them does not help: on a Dev Drive it is slower.
Half the files hold under 1% of the data, but they still have to exist.

## Individual operations

| Operation | Native | Multibox |
| --- | --- | --- |
| Create, tracked files only (10.3 GiB) | 204.8 s | 63.9 s before the switch to `CopyFileExW` |
| First `git status` | 0.43 s | 0.10 s |
| Remove, 104,330 files | 18.3 s | 0.30 s |
| Remove, 611,067 files | not measured | 0.37 s |

A native checkout is slower because every LFS file goes through the smudge filter,
while Multibox copies the bytes already on disk. `remove` returns once the worktree
is detached; the actual deletion of 715,000 files took about 41 seconds.

## Moving a project onto the Dev Drive

664,711 files (102.6 GiB) took 498.6 seconds with the editor open, before the
switch to `CopyFileExW`.

## Several worktrees next to the main editor

All with the main editor open and unaffected:

| Scenario | Result |
| --- | --- |
| Create two worktrees at once | 350 s for both on NTFS, before `CopyFileExW` |
| Open two worktree editors at once | 59 s each, identical results |
| Three editors running | Separate ports, each CLI reaches its own |
| Edit and compile in two worktrees at once | 18 s each, 10 build items, fully isolated |
| Clone while the source editor reimports 979 assets in a loop | 215 s, opens with 0 imports |

The last row is one trial, not a guarantee. Multibox warns when the database will
not hold still long enough to be copied.

## macOS

On an arm64 Mac Studio with APFS, using a synthetic repository of 120,004 files and
5 GB of data: 5.6 seconds to create, no measurable disk use, 0.05 seconds to remove.
No real Unity project has been measured on macOS yet.

## Still to measure

- A real Unity project on macOS.
- Worktree editors with a window, rather than batch mode.
- Linux, beyond the automated tests.
