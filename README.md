# Multibox

简体中文 | [English](README.en.md)

**用法和 `git worktree` 一样，但新工作区一建好就能直接干活。**

`git multibox add` 的参数和 `git worktree add` 完全相同，建出来的也是普通的 git 工作区。区别在于里面有什么：原生工作区是一份干净的检出，Unity 得把所有资源重新导入一遍，Rust 得把所有依赖重新编译一遍，然后才能开始干活。Multibox 会把当前工作区里能安全复用的缓存一起带过去，不能复用的留给工具自己重建。

## 用来干什么

- **一台机器上多个代理或多个人同时干活**：每人一个工作区，不用每个都等一次重新导入或完整编译。
- **在旁边试一个分支**：你的主工作区和开着的编辑器都不受影响。

它认识 Unity、Rust（Cargo）、pnpm 和 TypeScript 工程。其他仓库也能用，得到的和 `git worktree add` 完全一样。

## 安装

Windows，在 PowerShell 里运行：

```powershell
irm https://raw.githubusercontent.com/fpgames/multibox/main/install.ps1 | iex
```

macOS：

```bash
curl -fsSL https://raw.githubusercontent.com/fpgames/multibox/main/install.sh | sh
```

两条命令都会下载最新的[发布版本](https://github.com/fpgames/multibox/releases)，核对 `SHA256SUMS` 之后再安装。Windows 会自动加进 PATH，新开的终端就能用；macOS 装在 `~/.local/bin`，如果这个目录不在 PATH 里，安装结束时会打印一行加 PATH 的命令。其他系统，或者想用 Rust 自己编译：

```bash
cargo install --locked --git https://github.com/fpgames/multibox git-multibox
```

## 怎么用

不需要任何配置。在任意仓库里：

```bash
git multibox add ../feature -b feature   # 代替 git worktree add
# ……在 ../feature 里干活、提交……
git multibox finish ../feature           # 合并回当前分支，再删掉工作区和分支
git multibox remove ../feature           # 或者直接丢弃
```

`git multibox doctor` 会告诉你这台机器和这个仓库能用上什么，`git multibox -h` 列出全部命令。

**Windows 上建议把工程放到 Dev Drive。** 在 NTFS 上，每个工作区都是一份完整的副本；在 Dev Drive 上（macOS 也一样），文件在被修改之前是共享的，建一个工作区只要几秒，几乎不占空间。`git multibox init` 会创建 Dev Drive 并把工程搬过去，只需做一次，搬的过程中不耽误你继续干活。

## 对比效果

在一台 Windows 工作站上和原生 `git worktree` 对比，没有做任何配置。时间是从创建工作区开始、跑完所列步骤的总耗时。更多数据见 [docs/benchmarks.md](docs/benchmarks.md)（英文）。

| 工程与步骤 | NTFS 原生 | NTFS Multibox | Dev Drive 原生 | Dev Drive Multibox |
| --- | --- | --- | --- | --- |
| Unity 6 游戏，104,330 个文件：创建并首次打开 | 1,112 秒 | **112 秒** | 873 秒 | **80 秒** |
| 之后占用的磁盘 | 49.5 GiB | 53.3 GiB | 47.8 GiB | **2.9 GiB** |
| 带 Rust 组件的 pnpm monorepo：创建、装依赖、类型检查、开发构建、打安装包 | 485 秒 | **384 秒** | 458 秒 | **319 秒** |
| 之后占用的磁盘 | 4.9 GiB | 4.9 GiB | 4.6 GiB | **1.9 GiB** |
| Rust 工作区，970 个包：创建并编译测试 | 242 秒 | 236 秒 | 231 秒 | **180 秒** |
| 之后占用的磁盘 | 30.2 GiB | 30.2 GiB | 30.5 GiB | **23.6 GiB** |

- Unity 工作区首次打开只导入了 1 个资源，原生工作区是 56,961 个。
- 合并的结果和用原生工作区完全一样，连提交 id 都相同。`finish` 一步完成合并、删除工作区和删除分支。
- 在那个 monorepo 上，`git worktree remove` 因为 `node_modules/` 里的路径太长而失败，`git multibox remove` 没有问题。

没有做得更好的地方：

- NTFS 上，Unity 工作区比原生的多占约 8% 磁盘：它带上了源工程完整的 `Library/`，而重新导入只写需要的内容。
- Dev Drive 上，创建 Unity 工作区比原生检出慢：47 到 103 秒，原生是 30 到 67 秒，因为还要克隆 `Library/`。不过首次打开能省下十分钟以上。
- NTFS 上，为了省磁盘，Rust 和 pnpm 的缓存默认不带，所以编译耗时和原生一样。

## 设置

默认不需要任何配置文件。想调整细节，比如多带一些路径、在 NTFS 上也复制大缓存、关掉某个引擎，就在仓库根目录放一个 `.multibox.toml`。[使用指南](docs/guide.md)（英文）里有它的写法，也讲了 Multibox 的原理、每种引擎会带哪些东西和全部命令。

## 状态

早期版本。已在 Windows 和 macOS 上用于真实的 Unity 6 工程、一个 Rust 工作区和一个 pnpm monorepo；每次改动都会在一台 Windows 机器（NTFS 和 Dev Drive 各一遍）和一台 Apple 芯片的 Mac 上跑测试。Linux 能编译，但没有测试。Unreal Engine 在计划中。

## 许可证

MIT 或 Apache-2.0，任选其一。
