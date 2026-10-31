# Benchmarks

Measured on a Windows 11 workstation (32 threads, 61 GiB RAM, Gen4 NVMe) against a
Unity 6 project with 104,330 tracked files, 44,747 of them in Git LFS, 56,000
assets and a 71 GiB `Library/`. The main editor stayed open throughout, as it does
in daily work. Most rows ran once or twice with the file cache warm, so treat them
as indicative rather than precise. The Dev Drive sits in a dynamic VHDX on NVMe.

## From nothing to a usable worktree

| | Native `git worktree` | Multibox on NTFS | Multibox on Dev Drive |
| --- | --- | --- | --- |
| Create | 204.8 s | 62.0 s | 45 s |
| First Unity launch | 1127.3 s | 25 to 52 s | 25 to 52 s |
| **Total** | **22 minutes** | **2 minutes** | **73 seconds** |
| Assets imported | 56,960 | 0 to 1,076 | 0 to 1,076 |
| Disk per worktree | 52 GiB | 56 GiB | 2.1 to 2.7 GiB |
| Ignored generated assets | missing | present | present |
| Reopening later | not measured | not measured | 14.1 s |

The native worktree also needs a generation step for what it is missing. On the
Dev Drive, the worktree grew by 0.07 GiB after Unity imported and compiled in it.

Multibox's imports depend on the source project, not on cloning. An editor that has
been left open has not seen the latest commits, and its clones catch up on them:

| First launch of a clone | Time | Imports |
| --- | --- | --- |
| Source `Library/` behind the working tree | 52 s | 1,076 |
| Source `Library/` up to date | 25 s | 0 |
| Reopening the same worktree (baseline) | 14 s | 0 |

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

`git multibox init` created a 500 GiB Dev Drive and copied 942,388 files (315.8 GiB)
in 690 seconds with the editor open. The first `git status` in the copy took 0.16
seconds. The VHDX ended at 450 GiB for 326 GiB of data, about 1.4 times the data.

Finishing took 11.9 seconds with the editor closed, bringing across 212 files that
had changed since the copy. The editor reopened on the old path, ready in 43 seconds.

The overhead above is why the drive is sized to the project. ReFS spreads its writes and
a dynamic VHDX never gives a block back: on an earlier 1 TiB drive, repeated test
runs left a 923 GiB file holding 332 GiB of data, and neither retrim nor `compact
vdisk` recovered any of it.

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

A real Unity 6000.3 project on an M4 Mac mini with APFS: 202,929 files and 47 GiB,
including a 39 GiB `Library/`, with 41,164 files in Git LFS.

| | Native `git worktree` | Multibox |
| --- | --- | --- |
| Create | 48.8 s | 15.2 s |
| Disk per worktree | 8.4 GiB, no `Library/` | 110 MiB, `Library/` included |
| First Unity launch | not measured | 82.3 s, 1 import |

A synthetic repository of 120,005 files and 5 GB on a Mac Studio took 5.3 seconds
to create, and removal returns in 0.05 seconds. The native first launch and a reopen
could not be measured: the editor on that machine lost its license after one run.

## Rust

Multibox's own workspace on the same Windows workstation: 37 crates, 3 of them
local, with `target/` already built in the source. Each row is the time to
create the worktree and run `cargo test --no-run --offline` in it, over two runs.

| | Create | Build | Crates compiled |
| --- | --- | --- | --- |
| Native `git worktree`, NTFS | 0.06 s | 5.0 to 5.1 s | 37 |
| Multibox on NTFS | 0.41 to 0.48 s | 4.9 to 5.0 s | 37 |
| Multibox on NTFS, `target/` in `.worktreeinclude` | 0.41 to 0.42 s | 2.5 to 2.6 s | 3 |
| Native `git worktree`, Dev Drive | 0.06 s | 4.5 to 4.6 s | 37 |
| Multibox on Dev Drive | 0.42 s | 2.0 to 2.1 s | 3 |

On a repository this small Multibox takes about 0.35 seconds longer to create a
worktree than git does. A one-crate workspace depending on `serde_json` printed
`env!("CARGO_MANIFEST_DIR")`: in every worktree it named the worktree itself.
Before the Rust adapter, a carried `target/` made Cargo reuse the source build,
and the path still named the original.

## pnpm

The Cindy monorepo: 38 workspace projects and 2,328 packages, pnpm 10.33.2 with
`node-linker=hoisted`, on the same workstation. Each row runs from creating the
worktree until `pnpm install --frozen-lockfile` has finished, over two or three
runs with the store already filled.

| | Create | Install | Total | First `git status` |
| --- | --- | --- | --- | --- |
| Native `git worktree`, NTFS | 3.4 to 3.6 s | 26 s | 30 s | 0.13 to 0.26 s |
| Multibox on NTFS | 3.4 to 3.8 s | 24 to 25 s | 28 s | 0.05 s |
| Native `git worktree`, Dev Drive | 3.1 to 3.2 s | 22.6 s | 25.8 s | 0.12 to 0.43 s |
| Multibox on Dev Drive | 3.1 s | 22.5 to 22.9 s | 25.7 s | 0.05 to 0.06 s |
| Multibox on Dev Drive, `node_modules/` carried | 19.4 to 19.6 s | 5.8 s | 25.3 s | 0.05 s |

Copying the 135,672 files of `node_modules/` on NTFS took 36 seconds and 7.0 GiB,
against 1.8 GiB for a native worktree after its install. That is why copy mode
refuses it: with the refusal, a forced copy of the same project with the same
`.worktreeinclude` finished in 3.3 seconds on the Dev Drive. Cloned
on the Dev Drive, `node_modules/` saves about as much install time as the clone
costs. Before its state files were rewritten, pnpm refused to use a carried
`node_modules/` and asked to reinstall from scratch.

`git worktree remove` failed on NTFS with "Filename too long" both times it met
`node_modules/`. `git multibox remove` deleted every worktree.

The first Multibox run after a fresh checkout took 11 to 12 seconds to create a
worktree, on both drives; every later run matched git. This was not investigated.

## Still to measure

- pnpm's `node_modules/` cloned on APFS.
- Native import time and reopening on macOS.
- Worktree editors with a window, rather than batch mode.
- Linux, beyond the automated tests.
