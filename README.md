# Multibox

**git worktrees built for game development.** One machine, many agents, zero reimports.

`git multibox` is a drop-in `git worktree`. It takes the same arguments and
produces a normal worktree, but the new worktree also receives what native git
leaves behind: the engine's import cache, generated files and IDE projects.
Opening it in Unity takes as long as opening the original project, not as long
as importing it from scratch.

```bash
git worktree add ../feature -b feature
git multibox add ../feature -b feature   # same arguments, ready to open
```

Any git repository works. Game projects gain the most, because their caches are
the slowest thing to rebuild.

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
| NTFS, ext4 | copy | A full copy, still without a reimport |

Copy mode needs no setup. To get reflinks on Windows, run `git multibox init`.
It creates a Dev Drive inside a new VHDX file, without touching any existing
volume, and moves the project onto it, since reflinks only work within a volume.
Plan for the file's full size: it is sized at 1.5 times the project, and although
it starts small, it fills towards that size with use and never shrinks.
The copy happens while you keep working. When it is done, close the editor and
run `git multibox init --project <path>` from outside the project folder. It
brings across what changed and redirects the old path to the new location. The original stays on disk until you delete it. Run `init` as
many times as you like: it does whatever is left. On macOS it has nothing to do.

## Never worse than `git worktree`

Multibox only adds to what a native worktree gets, and only where that pays
off. The measurements behind these rules are in
[docs/benchmarks.md](docs/benchmarks.md):

- **Without an adapter, a worktree is a native one.** Only tracked files are
  cloned, plus what `.worktreeinclude` names. A Python, Go or iOS project gets
  exactly what `git worktree add` would give it.
- **A cache is carried only if cloning it beats rebuilding it.** Where the
  project's own tooling restores a cache faster, it stays behind.
- **Nothing an adapter carries reaches back into the source.** Links into the
  source tree follow the clone, files that record the project's path are
  rewritten, and build output compiled from the project's own sources is rebuilt.
- **Copy mode is frugal.** Without reflinks every carried byte costs disk space,
  so large caches stay behind unless `.worktreeinclude` asks for them. Caches a
  reinstall restores faster stay behind even then, with a warning.

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
with everything that identifies the source editor (see below).

**Rust** workspaces are found by a tracked `Cargo.toml` with `Cargo.lock` beside
it. Compiled dependencies in `target/` come along: `deps/`, `build/` and
`.fingerprint/` of every profile and target triple. Only the workspace's own
packages compile again, and they always do: `env!("CARGO_MANIFEST_DIR")` bakes
the source path into them, and Cargo would otherwise call the copies fresh, so
tests in the new worktree would read and write the original's files. Every
package without a `source` in `Cargo.lock` counts as the workspace's own, path
dependencies included. Target directories elsewhere are found too, up to two
levels below the workspace, by the `.rustc_info.json` Cargo writes into every
one it uses, so a `CARGO_TARGET_DIR` such as `.tmp/target` is handled the same
way. In copy mode they stay behind; name them in `.worktreeinclude` to carry
them anyway.

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

## Status

Early. Tested end to end on Windows and macOS with real Unity 6 projects, a
Rust workspace and a pnpm monorepo. Linux passes the automated
tests but has not been run against a real project. Unreal Engine is planned.

## License

MIT or Apache-2.0, at your option.
