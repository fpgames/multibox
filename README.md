# Multibox

**`git worktree`, but the new worktree is ready to work in.**

`git multibox add` takes the same arguments as `git worktree add` and makes a
normal git worktree. The difference is what is inside it: a native worktree is
a clean checkout, so Unity reimports every asset and Rust recompiles every
dependency before you can start. Multibox brings the caches that are safe to
reuse along from the worktree you are in, and leaves the rest to be rebuilt.

## What it is for

- **Several agents or people on one machine**, each in a worktree of its own,
  without each one waiting for a reimport or a full build.
- **Trying a branch next to your work**, while your main checkout and its
  editor stay open and untouched.

It knows Unity, Rust (Cargo), pnpm and TypeScript projects. Any other
repository works too, and gets exactly what `git worktree add` would give it.

## Install

Windows, in PowerShell:

```powershell
irm https://raw.githubusercontent.com/fpgames/multibox/main/install.ps1 | iex
```

macOS:

```bash
curl -fsSL https://raw.githubusercontent.com/fpgames/multibox/main/install.sh | sh
```

Both download the latest [release](https://github.com/fpgames/multibox/releases)
and check it against its `SHA256SUMS` before installing. Elsewhere, or to build
it yourself with Rust:

```bash
cargo install --locked --git https://github.com/fpgames/multibox git-multibox
```

## Use it

Nothing to configure. In any repository:

```bash
git multibox add ../feature -b feature   # instead of git worktree add
# ... work and commit in ../feature ...
git multibox finish ../feature           # merge it back here, then remove the worktree and branch
git multibox remove ../feature           # or throw it away
```

`git multibox doctor` shows what the machine and the repository allow, and
`git multibox -h` lists every command.

**On Windows, put the project on a Dev Drive.** On NTFS every worktree is a
full copy. On a Dev Drive, as on macOS, files are shared until they change, so
a worktree takes seconds and almost no space. `git multibox init` creates one
and moves the project onto it, once, while you keep working.

## How it compares

Measured on one Windows workstation against native `git worktree`, with nothing
configured. Each figure covers the steps named, from creating the worktree.
Details and more results are in [docs/benchmarks.md](docs/benchmarks.md).

| Project, steps | NTFS native | NTFS Multibox | Dev Drive native | Dev Drive Multibox |
| --- | --- | --- | --- | --- |
| Unity 6 game, 104,330 files: create and first launch | 1,112 s | **112 s** | 873 s | **80 s** |
| Disk afterwards | 49.5 GiB | 53.3 GiB | 47.8 GiB | **2.9 GiB** |
| pnpm monorepo with Rust helpers: create, install, type check, dev build, installer | 485 s | **384 s** | 458 s | **319 s** |
| Disk afterwards | 4.9 GiB | 4.9 GiB | 4.6 GiB | **1.9 GiB** |
| Rust workspace, 970 packages: create and build the tests | 242 s | 236 s | 231 s | **180 s** |
| Disk afterwards | 30.2 GiB | 30.2 GiB | 30.5 GiB | **23.6 GiB** |

- The Unity worktree opened after one import instead of 56,961.
- Merging gives the same commits as with native worktrees, to the commit id.
  `finish` does merge, removal and branch deletion in one step.
- `git worktree remove` failed on the monorepo, whose `node_modules/` paths are
  too long for it; `git multibox remove` did not.

Where it is not better:

- On NTFS, a Unity worktree takes about 8% more disk than a native one: it
  carries all of the source's `Library/`, while a fresh import writes only what
  it needs.
- On a Dev Drive, creating a Unity worktree takes longer than a native
  checkout, 47 to 103 seconds against 30 to 67, because it clones the
  `Library/` as well. The first launch then saves more than ten minutes.
- On NTFS, Rust and pnpm caches stay behind by default to save disk, so builds
  take as long as in a native worktree.

## Settings

The defaults need no file. To change the details, such as carrying more paths,
copying large caches on NTFS too, or turning an engine off, add a
`.multibox.toml` to the repository root. The [guide](docs/guide.md) describes
it, along with how Multibox works, what it carries for each engine and every
command.

## Status

Early. Used on Windows and macOS with real Unity 6 projects, a Rust workspace
and a pnpm monorepo, and tested on every change on a Windows machine, on NTFS
and on a Dev Drive, and on an Apple Silicon Mac. Linux builds but is untested.
Unreal Engine is planned.

## License

MIT or Apache-2.0, at your option.
