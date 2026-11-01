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
local, with `target/` already built in the source. macOS resolves one more
dependency than Windows. Each row is the time to
create the worktree and run `cargo test --no-run --offline` in it, over two runs.

| | Create | Build | Crates compiled |
| --- | --- | --- | --- |
| Native `git worktree`, NTFS | 0.06 s | 5.0 to 5.1 s | 37 |
| Multibox on NTFS | 0.41 to 0.48 s | 4.9 to 5.0 s | 37 |
| Multibox on NTFS, `target/` in `.worktreeinclude` | 0.41 to 0.42 s | 2.5 to 2.6 s | 3 |
| Native `git worktree`, Dev Drive | 0.06 s | 4.5 to 4.6 s | 37 |
| Multibox on Dev Drive | 0.42 s | 2.0 to 2.1 s | 3 |
| Native `git worktree`, Mac Studio (M5 Max, APFS) | 0.03 to 0.04 s | 2.2 s | 38 |
| Multibox on the Mac Studio | 0.35 to 0.73 s | 1.0 s | 3 |

On a repository this small Multibox takes about 0.35 seconds longer to create a
worktree than git does. A one-crate workspace depending on `serde_json` printed
`env!("CARGO_MANIFEST_DIR")`: in every worktree it named the worktree itself.
Before the Rust adapter, a carried `target/` made Cargo reuse the source build,
and the path still named the original.

## pnpm

The Cindy monorepo: 38 workspace projects and 2,328 packages, pnpm 10.33.2 with
`node-linker=hoisted`, on the same workstation and on a Mac Studio (M5 Max,
64 GiB, APFS, Node 24). Each row runs from creating the
worktree until `pnpm install --frozen-lockfile` has finished, over two or three
runs with the store already filled.

| | Create | Install | Total | First `git status` |
| --- | --- | --- | --- | --- |
| Native `git worktree`, NTFS | 3.4 to 3.6 s | 26 s | 30 s | 0.13 to 0.26 s |
| Multibox on NTFS | 3.4 to 3.8 s | 24 to 25 s | 28 s | 0.05 s |
| Native `git worktree`, Dev Drive | 3.1 to 3.2 s | 22.6 s | 25.8 s | 0.12 to 0.43 s |
| Multibox on Dev Drive | 3.1 s | 22.5 to 22.9 s | 25.7 s | 0.05 to 0.06 s |
| Multibox on Dev Drive, `node_modules/` carried | 19.4 to 19.6 s | 5.8 s | 25.3 s | 0.05 s |
| Native `git worktree`, Mac Studio | 2.0 s | 9.8 to 10.3 s | 11.8 to 12.4 s | 0.34 to 0.44 s |
| Multibox on the Mac Studio | 1.4 to 1.7 s | 10.1 to 10.4 s | 11.8 s | 0.07 to 0.08 s |
| Multibox on the Mac Studio, `node_modules/` carried | 7.3 to 7.6 s | 3.1 s | 10.4 to 10.7 s | 0.08 s |

Copying the 135,672 files of `node_modules/` on NTFS took 36 seconds and 7.0 GiB,
against 1.8 GiB for a native worktree after its install. That is why copy mode
refuses it: with the refusal, a forced copy of the same project with the same
`.worktreeinclude` finished in 3.3 seconds on the Dev Drive. Cloned
on the Dev Drive, `node_modules/` saves about as much install time as the clone
costs. On APFS it saved about 1.5 seconds, 13 percent, and disk too: after the
install a worktree took 1.5 GiB natively, 1.1 GiB with Multibox and 0.8 GiB
with `node_modules/` carried. In every carried copy, no link and no pnpm state
file pointed at the source. Before its state files were rewritten, pnpm refused to use a carried
`node_modules/` and asked to reinstall from scratch.

`git worktree remove` failed on NTFS with "Filename too long" both times it met
`node_modules/`. `git multibox remove` deleted every worktree.

The first Multibox run after a fresh checkout took 11 to 12 seconds to create a
worktree, on both drives; every later run matched git. This was not investigated.

## Where a new Cindy worktree spends its time

The same Cindy checkout on NTFS, at commit 1eb2eb388d: from creating a worktree
to a working development build, a type check and an installer. Run twice in
the same worktree, a step shows what a cache could save at most.

| | Native, first run | Native, second run | Multibox, first run |
| --- | --- | --- | --- |
| Create and `pnpm install` | 4.6 s + 24.8 s | | 4.4 s + 23.6 s |
| Type check | 106 s | 106 s | nothing to carry |
| Type check, incremental | 107 s | 19 s | 19 s |
| Development build until the UI responds | 72 s | 65 s | 68 s |
| Installer (`pnpm build`) | 274 s | 192 s | 224 s, `target/` carried |

- **Type check.** Cindy checked without `--incremental`, so every run started
  over. With it, the build info carried by Multibox made the first check in a
  new worktree as fast as a repeat, and a type error added in that worktree was
  still reported.
- **Installer.** Seven Rust crates compile in it. A new worktree compiled 425
  crates, one with `target/` carried 39: the workspaces' own and one helper
  that builds into a fresh directory every time. Copying the 2.4 GiB of
  `target/` on NTFS left creation time unchanged.
- **Development build.** Most of it is Vite bundling the main process and its
  workers, which keeps no cache between runs. Nothing carried can help there.

## Windows: three real projects against native worktrees

The same workstation, on NTFS (copy mode) and on the Dev Drive (reflinks), with
Multibox's defaults: nothing configured in any repository. Each project was
checked out as a test copy with its caches built once, as a main checkout is
in daily use. Rows average two rounds; the native Unity import ran once.
Native worktrees were removed with `git worktree remove`.

**Cindy**, a pnpm monorepo with seven Rust helpers: create, `pnpm install`,
incremental type check, development build until the UI responds, installer.

| | NTFS native | NTFS Multibox | Dev Drive native | Dev Drive Multibox |
| --- | --- | --- | --- | --- |
| Type check | 106.7 s | 19.7 s | 103.6 s | 17.1 s |
| Installer | 274.6 s | 271.5 s | 262.3 s | 210.4 s |
| All steps | 485 s | 384 s | 458 s | 319 s |
| Disk after all steps | 4.9 GiB | 4.9 GiB | 4.6 GiB | 1.9 GiB |
| Removal | failed twice | 9.4 s | failed twice | 5.1 s |

`git worktree remove` failed on every Cindy worktree, on both drives: paths in
`node_modules/` are too long for it. In copy mode the Rust helpers' `target/`
stays behind by default, which is why the installer saves nothing on NTFS.

**A Rust workspace** of 970 packages, 23 of them local, built through its own
script into `.tmp/target`: create, then `cargo test --no-run`.

| | NTFS native | NTFS Multibox | NTFS, `.tmp/target/` in `.worktreeinclude` | Dev Drive native | Dev Drive Multibox |
| --- | --- | --- | --- | --- | --- |
| Create | 6.4 s | 2.0 s | 11.6 s | 6.3 s | 2.3 s |
| Build | 235.6 s | 234.0 s | 190.7 s | 225.0 s | 177.6 s |
| Crates compiled | 581 | 581 | 52 | 581 | 52 |
| Disk after the build | 30.2 GiB | 30.2 GiB | 30.2 GiB | 30.5 GiB | 23.6 GiB |
| Removal | 7.1 s | 3.7 s | 4.4 s | 24.6 s | 6.7 s |

All 22 local packages built were rebuilt every time. Four of the 101 test
binaries built over carried dependencies still contain the source's path:
panic locations of code cranelift generates into its `OUT_DIR` and includes.
They name files, nothing opens them; a native build names its own target
directory there instead.

**A Unity 6 game** with 104,330 tracked files, the main editor open on the Dev
Drive copy throughout: create, then the first batch-mode launch.

| | NTFS native | NTFS Multibox | Dev Drive native | Dev Drive Multibox |
| --- | --- | --- | --- | --- |
| Create | 68 s | 60 s | 67 s | 47 s |
| First launch | 1,044 s, 56,961 imports | 54 s, 1 import | 806 s, 56,961 imports | 33 s, 1 import |
| Disk after launch | 49.5 GiB | 56.1 GiB | 47.8 GiB | 2.9 GiB |
| Removal | 14.6 s | 8.2 s | 19.5 s | 11.9 s |

On NTFS a Multibox worktree took 13% more disk than a native one after its
import: the carried `Library/` holds what the source accumulated, superseded
artifacts and a 2.8 GiB search index among them, and a fresh import writes
only what it needs. Copy mode now leaves the search index behind. One more
round on NTFS, cloned from the Dev Drive copy, took 59 seconds to create and
53 to launch, still with one import, and 53.3 GiB afterwards: 7.7% more than
native. In repeated cycles without the launch, creating a Unity worktree on
the Dev Drive took 48 to 85 seconds against 30 to 53 natively, because the
clone carries 287,000 files to git's 104,000. A later one took 103 seconds,
by then with 102 GiB of caches in the source; it still used 1.8 GiB.

The first `git status` took 0.07 to 0.13 seconds in every Multibox worktree and
0.37 to 0.80 seconds in native ones.

**Merging.** In throwaway clones of all three projects, the same change, a clean
merge and a conflicting merge with its resolution went through a native
worktree and a Multibox one, with fixed identities and dates. Every commit id
matched. `git multibox finish` produced the same commit ids as the native
sequence of merge, removal and branch deletion, in one command instead of
three, and stopped on conflicts as `git merge` does.

**Stability.** Five create-and-remove cycles per project and drive (three for
Unity) all succeeded for both, with clean status and nothing left registered.
Three worktrees created at once succeeded every time. Removing three at once
first failed: each removal swept up the others' trash and they collided. After
the fix, 41 of 42 concurrent removals reported success; the one that did not
had still deleted everything, and the failure did not recur in 24 attempts.

## Write-once caches and the project's own Rust units

Two changes, then the Rust workspace, the Unity game and Cindy measured again
on the same workstation, two rounds each unless noted.

**What stays behind.** A Rust worktree used to carry the workspace's own
compiled outputs, which its rebuild then overwrote: in the Rust workspace,
5.3 GiB of its own libraries and about 17 GiB of test binaries and debug
information, against 3.4 GiB of dependencies' libraries. They now stay behind.
On the Dev Drive this also ended 30 rebuilds of dependencies in every new
worktree: a build compiles the 22 local packages and nothing else.

**What is written once.** Where files are copied, files that are never changed
in place are now hard-linked to the source. Before deciding which, their
timestamps were read in long-used projects:

- In a Unity project's `Library/Artifacts/`, written by the editor over its
  whole life, the last write of each of 93,065 import results came at most
  0.2 seconds after its creation. `Library/PackageCache/` keeps the times
  from the package archives, so says nothing; it was tried separately, below.
- In the Rust workspace's target directory, used for months, every library or
  metadata file changed after its creation belonged to the workspace's own
  units; no dependency's was.

Then both were tried where it could go wrong:

- **Rust.** In a worktree sharing 1,198 dependency libraries with its source,
  the fingerprints of serde, regex, memchr and itoa, 12 units in all, were
  deleted, so that Cargo rebuilt them and everything above them: 157
  packages. rustc replaced each output with a new file, leaving one link on
  each side, and the SHA-256 of all 1,239 libraries in the source was
  unchanged.
- **Unity.** A worktree on NTFS sharing 94,430 import results was launched
  three times, with two JSON files edited and six assets added in between, which
  made it import nine of them. The source's import results kept their sizes and
  write times, every one. The worktree was then removed while an editor had the
  source open and was importing into it; it went in one pass, with nothing left.
- **Unity's package cache.** A worktree also sharing the 39,749 files of
  `Library/PackageCache/` was launched after removing one package and
  downgrading another, then after restoring both. Each time the Package
  Manager extracted a new directory, and the source's files kept their sizes
  and write times. But a line added to a package's `math.cs` in the worktree
  appeared in the source's copy at once, and the editor only reported that an
  immutable package had been altered, without reverting it. The package cache
  holds C# sources that IDEs open, so it is still copied: 1.6 GiB in this game.

Hard-linking was also timed, on 10,000 import results of 420 KB on average,
16 threads:

| | Copy (NTFS) or clone (Dev Drive) | Hard link |
| --- | --- | --- |
| NTFS | 0.12 to 0.19 ms per file, data still to be written | 0.09 to 0.10 ms |
| Dev Drive | 0.15 ms | 0.09 to 0.12 ms |

On the Dev Drive the gain is too small to give up independent clones, so
reflink volumes do not share; it is on NTFS, where a copy writes the data,
that sharing pays.

**The Rust workspace**, create, then `cargo test --no-run`:

| | NTFS native | NTFS Multibox | Dev Drive native | Dev Drive Multibox |
| --- | --- | --- | --- | --- |
| Create | 6.5 s | 6.0 s | 6.2 s | 2.7 s |
| Build | 231 s | 150 s | 227 s | 145 s |
| Crates compiled | 581 | 22 | 581 | 22 |
| Disk after the build | 30.2 GiB | 27.5 GiB | 30.3 GiB | 24.5 GiB |
| Removal | 5.1 s | 3.3 s | 22.9 s | 5.3 s |

Before, on NTFS, Multibox left `target/` behind and built in 234 seconds.

**The Unity game** on NTFS, cloned from a checkout on NTFS, create, then the
first batch-mode launch:

| | Native | Multibox, before | Multibox, sharing |
| --- | --- | --- | --- |
| Create | 68 s | 59 s | 34 to 38 s |
| First launch | 1,097 s, 56,961 imports | 53 s, 1 import | 55 to 56 s, 1 import |
| Disk after launch | 49.5 GiB | 53.3 GiB | 15.3 GiB |
| Removal | 14.5 s | 8.2 s | 7.8 s |

On the Dev Drive nothing changed for Unity, but its creation time was measured
again, native and Multibox alternating, three rounds each: 49 to 68 seconds,
56.9 on average, natively, and 49.9 to 50.4, 50.2 on average, with Multibox.
The earlier comparison, which had Multibox slower, set rounds from different
times against each other; a single run right after other work took 103 seconds.

**Cindy**, the pnpm monorepo with seven Rust helpers, from the same source as
in the earlier round: create, `pnpm install`, incremental type check,
development build until the UI responds, installer. Native and Multibox
alternated, three rounds each on NTFS and two on the Dev Drive; times are
averages.

| | NTFS native | NTFS Multibox | Dev Drive native | Dev Drive Multibox |
| --- | --- | --- | --- | --- |
| Type check | 108.2 s | 19.8 s | 107.3 s | 17.3 s |
| Installer | 284.3 s | 235.5 s | 290.5 s | 214.8 s |
| Rust crates compiled | 425 | 39 | 425 | 39 |
| All steps | 497 s | 361 s | 498 s | 326 s |
| Disk after all steps | 5.2 GiB | 3.0 GiB | 4.7 GiB | 1.9 GiB |
| Removal | failed every time | 9.6 s | failed every time | 5.3 s |

Before, on NTFS, the helpers' `target/` stayed behind, and the installer
compiled all 425 crates in 271.5 seconds. Now their dependencies' libraries are
hard-linked, 936 files and 1.5 GiB.

Disk is the median of the rounds. On NTFS one Multibox round measured 9.1 GiB
and the other two 2.5 and 3.0, while native rounds measured 5.1 to 6.3: free
space on that drive also moved with other work. On the Dev Drive only the first
round of each kind counts. The worktree removed before the second went on
freeing space during it, which made its readings negative. One native
installer there took 318 seconds, the other 264, as in the earlier round.

## Still to measure

- Native import time and reopening on macOS.
- Worktree editors with a window, rather than batch mode.
- Linux, beyond the automated tests.
