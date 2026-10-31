# Benchmarks

Measured on a Windows 11 workstation (32 threads, 61 GiB RAM, Gen4 NVMe, NTFS)
against a Unity 6 project with 104,330 tracked files, 44,747 of them in Git LFS,
56,000 assets and a 71 GiB `Library/`. The main editor stayed open throughout, as
it does in daily work. Most rows ran once with the file cache warm, so treat them
as indicative rather than precise.

## Time to a usable worktree (copy mode)

| Step | Native `git worktree` | Multibox |
| --- | --- | --- |
| Create | 204.8 s | 187.0 s |
| First Unity launch | 1127.3 s (56,960 imports) | 59 to 167 s (1,076 imports) |
| **Total** | **22 minutes** | **4 to 6 minutes** |
| Ignored generated assets present | no | yes |

The native worktree also lacks ignored files such as generated configs and
`StreamingAssets`, so even after the import it needs a generation step.

Multibox's 1,076 imports came from the source project, not from cloning: the
long-running editor's `Library/` had not caught up with the latest commits. Cloning
from a project whose `Library/` is up to date removes them:

| First launch of a clone | Time | Imports |
| --- | --- | --- |
| Source `Library/` behind the working tree | 59 to 167 s | 1,076 |
| Source `Library/` up to date | 29.8 s | 0 |
| Reopening the same worktree (baseline) | 13.7 s | 0 |

The difference between the last two rows is one rebuild of the script build graph,
which embeds absolute paths.

## Individual operations

| Operation | Native | Multibox |
| --- | --- | --- |
| Create, tracked files only (10.3 GiB) | 204.8 s | 63.9 s |
| First `git status` | 0.43 s | 0.10 s |
| Remove, 104,330 files | 18.3 s | 0.30 s |
| Remove, 611,067 files | not measured | 0.37 s |

A native checkout is slower because every LFS file goes through the smudge filter,
while Multibox copies the bytes already on disk. `remove` returns once the worktree
is detached; the actual deletion of 715,000 files took about 41 seconds.

## Several worktrees next to the main editor

All with the main editor open and unaffected:

| Scenario | Result |
| --- | --- |
| Create two worktrees at once | 350 s for both, no failures |
| Open two worktree editors at once | 59 s each, identical results |
| Three editors running | Separate ports, each CLI reaches its own |
| Edit and compile in two worktrees at once | 18 s each, 10 build items, fully isolated |
| Clone while the source editor reimports 979 assets in a loop | 215 s, opens with 0 imports |

The last row is one trial, not a guarantee. Multibox warns when the database will
not hold still long enough to be copied.

## Still to measure

- Reflink mode on a Dev Drive: creation time and real disk usage.
- Worktree editors with a window, rather than batch mode.
- macOS and Linux.
