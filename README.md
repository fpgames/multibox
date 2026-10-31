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

Copy mode needs no setup. On Windows, `git multibox setup` creates a Dev Drive
inside a new VHDX file and `git multibox migrate` moves a project onto it.

## Commands

```
git multibox add [<options>] <path> [<commit-ish>]
git multibox remove [-f] <worktree>
git multibox doctor
git multibox setup [--vhdx <file>] [--size <GiB>] [--letter <X> | --folder <dir>]
git multibox migrate <destination> [--exclude <path>]...
git multibox migrate --switch <destination>
```

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
`Logs/`, `obj/`, editor lock files and Multiplayer Play Mode's virtual players
stay behind.

Everything else is opt-in through a `.worktreeinclude` file at the repository
root. It uses gitignore syntax and only applies to ignored files:

```gitignore
# Carry these ignored paths into new worktrees
client/Bundles/
node_modules/
!client/Bundles/iOS/
```

## As a library

`multibox-core` exposes everything the CLI does, and `multibox-clone` is a
standalone parallel tree cloner. Both are plain Rust crates; `Observer` receives
progress, and `build_plan` reports the cost of a worktree before creating it.

## Status

Early. Tested end to end on Windows with Unity 6. macOS and Linux compile but
have not been run against a real project yet. Unreal Engine is planned.

## License

MIT or Apache-2.0, at your option.
