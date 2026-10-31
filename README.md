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

## Commands

```
git multibox add [<options>] <path> [<commit-ish>]
git multibox remove [-f] <worktree>
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

## What gets carried

Adapters recognise engine projects from the tracked files.

**Unity** projects are found by `ProjectSettings/ProjectVersion.txt`, at any depth.
`Library/` is only valid together with the files it was built from, so everything
in `Assets/`, `Packages/` and `ProjectSettings/` is carried, including ignored
generated assets. `UserSettings/` and IDE projects come along too. `Temp/`,
`Logs/`, `obj/`, and Multiplayer Play Mode's virtual players stay behind, along
with everything that identifies the source editor (see below).

Everything else is opt-in through a `.worktreeinclude` file at the repository
root. It uses gitignore syntax and only applies to ignored files:

```gitignore
# Carry these ignored paths into new worktrees
client/Bundles/
node_modules/
!client/Bundles/iOS/
```

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

Early. Tested end to end on Windows with Unity 6. macOS and Linux compile but
have not been run against a real project yet. Unreal Engine is planned.

## License

MIT or Apache-2.0, at your option.
