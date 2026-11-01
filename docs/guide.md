# Multibox guide

The details behind the [README](../README.en.md): how a worktree is made, what
each engine gets, every command and option, and the configuration file.

## How it works

A native worktree is a clean checkout. For a Unity project that means no
`Library/`, so the first launch reimports every asset. Multibox instead clones
the current worktree, then asks git to make the clone an exact checkout:

1. `git worktree add --no-checkout` registers the worktree, so branch handling,
   hooks and locking stay git's job.
2. Tracked files, plus ignored files selected by the rules below, are cloned with
   their timestamps. Reflinks are used where the filesystem has them.
3. The source index is installed in the new worktree. Its cached stat data still
   matches, so git does not re-hash anything.
4. `git reset --hard` removes whatever differs from the target commit. Only files
   that were dirty in the source, or differ between the commits, are rewritten.

The result is indistinguishable from a native worktree to git. If an optimisation
cannot apply, for example with a split index, Multibox falls back to a native
checkout and still carries the caches.

### Reflinks and copy mode

| Filesystem | Mode | New worktree |
| --- | --- | --- |
| APFS, Btrfs, XFS | reflink | Shares data; only changed files take space |
| ReFS / Dev Drive | reflink | Shares data; only changed files take space |
| NTFS, ext4 | copy | A copy, except write-once caches, which are hard-linked |

Copy mode needs no setup. To get reflinks on Windows, run `git multibox init`.
It creates a Dev Drive inside a new VHDX file, without touching any existing
volume, and moves the project onto it, since reflinks only work within a volume.
Plan for the file's full size: it is sized at 1.5 times the project, and although
it starts small, it fills towards that size with use and never shrinks.
The copy happens while you keep working. When it is done, close the editor and
run `git multibox init --project <path>` from outside the project folder. It
brings across what changed and redirects the old path to the new location. The
original stays on disk until you delete it. Run `init` as many times as you
like: it does whatever is left. On macOS it has nothing to do.

A worktree only shares data with its source on the same volume. Created on
another one, it is a full copy, and `add` says so.

### Write-once caches

Some caches are never changed once written: a new version is a new file.
Unity's import results are named after their content and moved into place,
and rustc writes a dependency's library and metadata to a new file that
replaces the old one. Where files are copied, a worktree on the same volume
hard-links these instead: the worktree and its source then point at the same
data, which costs neither time nor space. Anything that rewrites such a file
replaces it, so the other side keeps its own.

A file type is listed only after it was seen to behave so in real projects; the
evidence is in [benchmarks.md](benchmarks.md). Where reflinks exist there is
nothing to gain, and `--copy` asks for independent copies, so neither shares
anything. `share-write-once = false` in `.multibox.toml` turns it off.

## Never worse than `git worktree`

Multibox only adds to what a native worktree gets, and only where that pays
off. The measurements behind these rules are in
[benchmarks.md](benchmarks.md):

- **Without an adapter, a worktree is a native one.** Only tracked files are
  cloned, plus what `.worktreeinclude` names. A Python, Go or iOS project gets
  exactly what `git worktree add` would give it.
- **A cache is carried only if cloning it beats rebuilding it.** Where the
  project's own tooling restores a cache faster, it stays behind.
- **Nothing an adapter carries reaches back into the source.** Links into the
  source tree follow the clone, files that record the project's path are
  rewritten, and build output compiled from the project's own sources is rebuilt.
  Hard-linked files are only those their tools never change in place and
  nobody opens to edit.
- **Copy mode is frugal.** Without reflinks every copied byte costs disk space,
  so large caches stay behind unless sharing makes them cheap, or
  `.worktreeinclude` or `.multibox.toml` asks for them. Caches a reinstall
  restores faster stay behind even then, with a warning.

## Commands

```
git multibox add [<options>] <path> [<commit-ish>]
git multibox remove [-f] <worktree>
git multibox finish [--keep-branch] <worktree> [-- <merge options>]
git multibox init [--project <path>] [--to <path>] [--exclude <path>]...
git multibox doctor
git multibox env
```

`init` is built from two commands that can also be run alone, for scripted setups:
`git multibox setup` creates the Dev Drive, and `git multibox migrate` copies and
switches a project.

`list`, `lock`, `unlock`, `move`, `prune` and `repair` go straight to `git worktree`.

`git multibox -h`, or `-h` after any command, prints the full usage.
`git multibox --help` does not: git takes it as a request for a manual page.

`add` accepts every `git worktree add` option, plus:

| Option | Meaning |
| --- | --- |
| `--from <worktree>` | Clone from this worktree instead of the current one |
| `--no-cache` | Leave ignored caches behind |
| `--copy` / `--reflink` | Force a mode; `--reflink` fails if unavailable |
| `--dry-run` | Show what would be cloned and stop |
| `--json` | Print the result as JSON on stdout, for tools and agents |

`remove` has the same safety rules as `git worktree remove`, refuses while an
editor has the project open, and returns immediately: the deletion continues in
the background. Use `--wait` to stay in the foreground.

## Bringing work back

A Multibox worktree is an ordinary git worktree, so its branch merges, rebases
and goes through pull requests exactly as with `git worktree`. `finish` does
the usual local round trip in one step, run from the worktree that receives
the work:

```bash
git multibox finish ../feature           # merge feature here, then remove it
git multibox finish ../feature -- --no-ff
```

It checks everything before changing anything: the work must be committed, the
receiving worktree clean, and no editor may have the finished worktree open. A
merge that hits conflicts stops and stays in progress, as `git merge` leaves
it; resolve and commit, then run `finish` again to clean up. Once merged, the
worktree and its branch are removed.

Adapters can tune the merge. In Unity projects `finish` passes
`-X find-renames=90%`: `.meta` files are small and alike, and git's default
50% similarity pairs unrelated ones as renames. Replaying 35 real merges of a
Unity 6 project, that caused a fifth of all conflicts and once replaced a GUID
both branches agreed on; the stricter threshold removed those conflicts and
changed none of the merges that were already clean. Unity's own UnityYAMLMerge
is not used: none of those conflicts were in scenes or prefabs, and it waits
for a graphical merge tool when it cannot decide, which blocks an agent.

## What gets carried

Adapters recognise engine projects from the tracked files.

**Unity** projects are found by `ProjectSettings/ProjectVersion.txt`, at any depth.
`Library/` is only valid together with the files it was built from, so everything
in `Assets/`, `Packages/` and `ProjectSettings/` is carried, including ignored
generated assets. `UserSettings/` and IDE projects come along too. `Temp/`,
`Logs/`, `obj/`, and Multiplayer Play Mode's virtual players stay behind, along
with everything that identifies the source editor (see below). In copy mode
`Library/BuildCache/` and the search index in `Library/Search/` stay behind
too: only player builds read the first, and the editor rebuilds the second in
the background. The import results in `Library/Artifacts/`, most of the
`Library/`, are written once and hard-linked where files are copied. The
package cache is copied all the same: it holds C# sources that IDEs open.

**Rust** workspaces are found by a tracked `Cargo.toml` with `Cargo.lock` beside
it. Compiled dependencies in `target/` come along: `deps/`, `build/` and
`.fingerprint/` of every profile and target triple. Only the workspace's own
packages compile again, and they always do: `env!("CARGO_MANIFEST_DIR")` bakes
the source path into them, and Cargo would otherwise call the copies fresh, so
tests in the new worktree would read and write the original's files. None of
their outputs is carried, since the rebuild would only overwrite them. A unit is
the workspace's own when the dep-info file rustc writes beside it names a
source in the repository: Cargo hands rustc a member's sources relative to the
workspace and everything else's as absolute paths. That also finds tests and
binaries named after their files. Every package without a `source` in
`Cargo.lock` counts as the workspace's own too, path dependencies included.
Target directories elsewhere are found too, up to two levels below the
workspace, by the `.rustc_info.json` Cargo writes into every one it uses, so a
`CARGO_TARGET_DIR` such as `.tmp/target` is handled the same way. Where files
are copied, the dependencies' libraries and metadata are hard-linked, so
`target/` is carried there too; across volumes, or with sharing turned off, it
stays behind unless `.worktreeinclude` names it.

**pnpm** projects are found by `pnpm-lock.yaml`. pnpm installs from a store
shared between projects, which leaves a clone little to save, so `node_modules/`
is not carried by default. In copy mode it stays behind even when
`.worktreeinclude` names it. With reflinks it is carried when asked for, and the
project path pnpm records in `node_modules/.modules.yaml` and its workspace
state is rewritten for the new worktree; otherwise pnpm would offer to delete
and reinstall everything.

**TypeScript** projects are found by their tracked `tsconfig*.json`. Incremental
type checking (`tsc --noEmit --incremental`) keeps its state in a
`.tsbuildinfo` file, and a new worktree reuses the source's: tsc records files
relative to it and compares their contents, so it only rechecks what differs.
Build info is looked for next to each tsconfig, in its `outDir`, and where its
`tsBuildInfoFile` points, including inside `node_modules/` as create-vite sets
it up. It is carried in every mode, but only if the run emitted nothing: tsc
trusts its record of emitted files, and the worktree would not have them.

Everything else is opt-in through a `.worktreeinclude` file at the repository
root. It uses gitignore syntax and only applies to ignored files:

```gitignore
# Carry these ignored paths into new worktrees
client/Bundles/
tools/downloads/
!client/Bundles/iOS/
```

Links in what you add are repointed at the new worktree, but other recorded
paths are not, so check that a cache does not store absolute paths before
adding it.

## Configuration

Nothing needs configuring. A `.multibox.toml` at the repository root adjusts
the details, and every key in it is optional. Commit it to share the settings,
or list it in `.git/info/exclude` to keep them to yourself; either way it
travels into new worktrees. These are the defaults:

```toml
enabled = true          # false: `git multibox add` is a plain `git worktree add` here

[carry]
include = []            # more ignored paths to carry, as in .worktreeinclude
exclude = []            # ignored paths never to carry, whatever asks for them
large-caches = "auto"   # Rust's target/, Unity's BuildCache and search index:
                        # "auto" carries them where they cost little,
                        # "always" copies them on NTFS and ext4 too, "never" leaves them
share-write-once = true # hard-link write-once caches where files are copied

[engines]               # false turns an adapter off
unity = true
cargo = true
pnpm = true
typescript = true

[finish]
merge-options = []      # passed to git merge after the engines' own
keep-branch = false
```

The patterns come after the adapters' and those of `.worktreeinclude`, so they
have the last word, except over the refusals described above: `node_modules/`
is never copied, whatever asks for it. Command-line options win over the file.

`large-caches = "always"` trades disk for time where nothing can be shared: a
worktree on another volume than its source, or Unity's build cache, which only
player builds read. Worth it where every worktree gets built; not where most
are only read.

A mistake never stops a worktree: an unknown key or a bad value is skipped with
a warning, and a file that is not valid TOML is ignored whole. `git multibox
doctor` shows what the file changes and anything it skipped.

## Working next to an open editor

Your main editor can stay open while worktrees come and go. Multibox is built
for that, and each of these rules exists because breaking it did real damage in
testing:

- **A worktree never inherits the source editor's identity.** `Library/` holds
  live process state. A new editor treats the PID in `ilpp.pid` as its own
  leftover and kills it, and `Library/Pipeline/` holds the source editor's control
  port and access token. None of that is carried, so each editor gets its own
  port, and links into the source tree are repointed at the new worktree.
- **Asset databases are cloned first.** Artifacts are only ever added, so an older
  database stays valid against a newer artifact store, but not the other way
  round. If the database changes during its copy, the copy is retried. You get a
  warning only if it never settles, and the first launch repairs what it finds.
- **Tools must be pinned to the worktree.** In a worktree without its own editor,
  a bare `unity command` falls back to any open editor, which means your main one.
  `git multibox add` reports the variable that prevents this, and
  `eval "$(git multibox env)"` sets it. Passing `--project-path` works too.
- **`remove` refuses while an editor has the worktree open.** `--force` overrides.

## As a library

`multibox-core` exposes everything the CLI does, and `multibox-clone` is a
standalone parallel tree cloner. Both are plain Rust crates; `Observer` receives
progress, and `build_plan` reports the cost of a worktree before creating it.
