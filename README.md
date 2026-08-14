# repomgr

一个极简的 TUI 程序，用来管理当前目录下的所有 git 项目。启动时
自动扫描你所在目录，交互式完成查看、更新、克隆等操作。

<img width="1470" height="887" alt="image" src="https://github.com/user-attachments/assets/e058d3ae-cf8f-4d27-8916-a6d80611c607" />


## 特性

- 双栏主界面：左侧是项目列表，右侧实时显示选中项目的信息
  （当前分支、remote、与上游的 ahead/behind、工作区状态、stash 数、
  本地分支数、最近一次提交）。
- 快捷键齐全：`u` 更新（支持 `Space` 多选并发批量更新）、`s` 看状态、
  `b` 看提交记录、`l` 看分支、`n` 克隆新项目、`o`/`O` 打开文件夹/
  远程地址、`r`/`R` 重扫/刷新。
- `h` 随时打开帮助。
- 只扫描当前目录的下一级子目录，并跳过隐藏目录（以 `.` 开头）。
- 启动后在后台线程预取所有项目的信息，切换列表即时响应；个别项目
  尚未取到时右侧会显示 "loading…"。
- 后台每 15 秒做一次轻量变更检测（只读 stat，不跑 git）：在另一个
  终端里 pull/commit/fetch/建分支等操作会自动刷新对应仓库的缓存。
  仅原地编辑文件内容、或只 `git add`（不动 refs/HEAD）不会触发，
  需要时按 `R` 手动刷新。
- 只依赖一个外部 crate（ratatui，crossterm 由它内置转发），支持
  macOS / Linux。
- 所有 git 操作都通过本机 `git` 命令完成，自动继承你的全局 git 配置。
- 本地信息查询（status/log/branch 等）有 20 秒超时保护，避免挂死的
  文件系统把界面永久冻结；`pull`/`clone` 等网络操作无超时，按需等待。

## 构建与安装

需要 Rust 工具链和 `git`：

```sh
cd scripts/repomgr
cargo build --release
# 二进制在 target/release/repomgr

# 或安装到 ~/.cargo/bin
cargo install --path .
```

## 使用

```sh
repomgr              # 扫描当前目录
repomgr ~/dev/github # 或指定任意目录
```

## 快捷键

| 按键 | 功能 |
|---|---|
| `↑ ↓` / `j k` | 在项目间移动 |
| `g` / `G` | 跳到第一个 / 最后一个项目 |
| `Space` | 标记 / 取消标记当前项目（用于批量更新，标记后自动下移一行） |
| `A` | 清除所有标记 |
| `u` | 更新所有标记的项目；无标记时更新选中项目（`git pull --ff-only`） |
| `s` / `Enter` | 查看 `git status` |
| `b` | 查看最近提交（`git log`） |
| `l` | 查看本地分支（`git branch -vv`） |
| `n` | 克隆新仓库到当前目录 |
| `o` / `O` | 在文件管理器打开 / 浏览器打开远程地址 |
| `r` / `R` | 重新扫描目录 / 重新读取选中项目信息 |
| `PgUp` / `PgDn` | 滚动信息面板或模态框 |
| `h` / `?` | 查看帮助 |
| `q` / `Esc` / `Ctrl-C` | 退出（`q`/`Esc` 在模态框中先关闭模态框，`Ctrl-C` 直接退出） |

## 架构

```
src/main.rs  应用状态（App）、事件循环、按键分发
src/view.rs  全部绘制：双栏布局、信息面板、帮助/输入/结果模态框
src/git.rs   通过 Command 调用 git CLI：发现仓库、读取信息、update/clone 等
```

每个模块职责单一，想改界面只动 `view.rs`，想加 git 操作只动 `git.rs`。
