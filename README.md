# MiniMemFS：课程演示用内存文件系统

基于 Rust 和 `fuser = "=0.18.0"` 实现的用户态内存文件系统，用于大学操作系统课程的案例展示。支持通过 `ls`、`cat`、Shell 重定向、`truncate` 和 `rm` 等普通 Linux 命令操作挂载目录。

文件内容保存在内存中的 `Vec<u8>`，卸载退出后不保留；重新启动恢复预置的 `hello.txt` 和 `notes.txt`。

## 实验目的与基本原理

本实验对应 [任务要求](assignment.txt) 中“学习 Rust 与 FUSE 的融合，开发用户态文件系统应用案例”的主题，主要学习目标如下：

- 理解普通文件操作如何经内核转发到用户态文件系统。
- 使用 Rust 的结构体、集合、所有权和互斥锁管理文件内容及共享状态。
- 区分文件名、inode、打开句柄和内核查找引用，理解“删除文件名”与“回收节点”的区别。
- 通过实际挂载验证读写、截断、删除及错误处理，而不只调用内部函数。

请求处理过程：

```text
ls / cat / 文件写入 / rm
          ↓ 系统调用
Linux VFS（虚拟文件系统）
          ↓
FUSE 内核驱动
          ↓ /dev/fuse
fuser（接收请求并分发回调）
          ↓
MemFs 的 Filesystem 回调
          ↓ 持有状态锁
FsState（目录项、节点、句柄）→ Node（属性和内存字节）
          ↓
通过 reply 返回数据、属性或错误，沿原路径交还给应用程序
```

FUSE 提供内核与用户态文件系统之间的通信机制，fuser 负责 Rust 侧的协议处理和回调分发，MiniMemFS 实现具体的文件操作。作业提到的 fuse-rs 对应原 `fuse` crate；fuser 从该 crate 分支发展而来(https://github.com/cberner/fuser#about)。本项目使用固定版本 fuser 0.18.0 实践这一主题，代码接口以该版本文档为准。

## 功能与项目结构

| 功能 | 当前行为 |
| --- | --- |
| 目录与属性查询 | 列出根目录文件，返回 inode、大小、权限和时间等属性 |
| 文件创建与读写 | 创建普通文件，支持按偏移读写、追加和覆盖写入 |
| 文件截断 | 缩短删除尾部，扩长时补零，越过末尾写入时空隙补零 |
| 文件删除 | 立即移除文件名，已有打开句柄仍可访问旧节点 |
| 同名重建 | 新路径指向新 inode，不影响尚未关闭的旧句柄 |
| 节点回收 | 名称、打开句柄和内核查找引用全部释放后回收节点 |
| 错误与边界 | 检查缺失文件、重名创建、无效句柄及文件大小、节点数限制 |

```text
项目根目录/
├── assignment.txt            # 原始课程任务要求
├── README.md                 # 项目说明、运行和验证步骤
├── Cargo.toml                # 包配置与固定的 fuser 版本
├── Cargo.lock                # 依赖锁定文件，随源码提交
├── src/
│   ├── main.rs               # 参数检查与挂载入口
│   ├── filesystem.rs         # FUSE 操作回调
│   └── state.rs              # 内存状态与回收单元测试
├── scripts/
│   ├── smoke-test.sh         # 独立挂载与功能检查入口
│   └── lifecycle-test.py     # 删除、句柄及目录续读检查
└── artifacts/
    └── hello-fs.log          # 文件系统回调日志，测试时覆盖生成
```

`target/` 是编译输出目录，`presentation/` 是本地演示材料目录，两者已由 `.gitignore` 忽略。源码提交包不依赖这两个目录；PPT 和视频等支撑附件另附。

## 环境与运行

### 1. 准备 Linux 环境

以下命令在 **WSL2 的 Ubuntu 终端**中执行，建议使用 Ubuntu 24.04；代码使用 Unix 接口，本文提供 Linux 环境下的运行与验证步骤。两个演示终端应属于同一个 Ubuntu 发行版，并使用同一普通用户。

在 Ubuntu 中安装基础依赖：

```bash
sudo apt update
sudo apt install -y build-essential curl ca-certificates fuse3 python3
```

其中 `build-essential` 提供编译、链接工具，`fuse3` 提供 `fusermount3`，Python 3 用于运行验证脚本。当前使用 [fuser 0.18.0 的默认特性](https://docs.rs/crate/fuser/0.18.0/features)，不启用 libfuse 链接功能，因此本配置无需额外安装 libfuse 开发头文件。

如果尚未安装 Rust，按 [Rust 官方安装说明](https://rust-lang.org/tools/install/) 在 Ubuntu 中执行：

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

安装时选择 stable 工具链；已有工具链过旧时可运行 `rustup update stable`。本项目使用 Rust 2024 edition，请使用支持该 edition 及锁定依赖的工具链。

检查环境：

```bash
rustc --version
cargo --version
cc --version
python3 --version
fusermount3 --version
command -v mountpoint
ls -l /dev/fuse
test -r /dev/fuse && test -w /dev/fuse && echo 'FUSE device accessible'
```

应能找到各工具，且最后显示 `FUSE device accessible`。`/dev/fuse` 不存在或当前用户无法访问时，需要先解决 Linux/WSL 的 FUSE 环境问题再挂载。

### 2. 编译并挂载（终端 A）

解压源码提交包后，进入包含 `Cargo.toml` 的项目根目录。下面的 `/path/to/MiniMemFS` 是占位路径，请替换为实际解压位置；如果文件位于 Windows 的 C 盘，WSL 中对应路径以 `/mnt/c/` 开头。

```bash
cd /path/to/MiniMemFS
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR="$HOME/.cache/os-task1-target"
cargo build --locked
mkdir -p "$HOME/mnt/os-task1"
cargo run --locked -- "$HOME/mnt/os-task1"
```

首次构建需要联网下载锁定依赖。保留 `Cargo.lock` 并使用 `--locked`，避免构建时重新解析依赖版本。设置 `CARGO_TARGET_DIR` 后，编译产物位于 Linux 用户目录下的缓存目录，而不是源码目录的 `target/`。

挂载点必须是已存在的空目录。成功后终端出现 `Mounting writable MemFs at ...`，程序留在前台处理请求，日志写入终端；还应在终端 B 执行下述命令确认挂载可用。`export` 仅影响当前终端及其子进程，新终端运行 Cargo 时需要重新设置构建目录。

### 3. 查询和读写演示（终端 B）

```bash
ls -la "$HOME/mnt/os-task1"
stat "$HOME/mnt/os-task1/hello.txt"
cat "$HOME/mnt/os-task1/hello.txt"
cat "$HOME/mnt/os-task1/notes.txt"
cat "$HOME/mnt/os-task1/missing.txt"
```

`hello.txt` 应显示 `Hello from Rust FUSE!`，大小为 21 字节（包括换行）。最后一条命令应报告文件不存在。

继续在终端 B 演示读写（不必进入挂载目录）：

```bash
printf 'hello' > "$HOME/mnt/os-task1/test.txt"
cat "$HOME/mnt/os-task1/test.txt"
printf ' world' >> "$HOME/mnt/os-task1/test.txt"
cat "$HOME/mnt/os-task1/test.txt"
printf 'xy' > "$HOME/mnt/os-task1/test.txt"
cat "$HOME/mnt/os-task1/test.txt"
stat "$HOME/mnt/os-task1/test.txt"
truncate -s 1 "$HOME/mnt/os-task1/test.txt"
cat "$HOME/mnt/os-task1/test.txt"
```

内容依次为 `hello`、`hello world`、`xy`、`x`（printf 未写换行）。覆盖写入后大小必须为 2，不能残留旧尾部。

### 4. 删除与同名重建演示（终端 B）

普通删除：

```bash
rm "$HOME/mnt/os-task1/test.txt"
ls -la "$HOME/mnt/os-task1"
```

打开后删除与同名重建（请使用尚不存在的 `delete-demo.txt`）：

```bash
python3 - <<'PY'
from pathlib import Path
p = Path.home() / 'mnt/os-task1/delete-demo.txt'
with p.open('x+b', buffering=0) as old:
    old.write(b'old content')
    p.unlink()
    print('路径存在:', p.exists())
    p.write_bytes(b'new content')
    old.seek(0)
    print('旧描述符:', old.read())
    print('新路径:', p.read_bytes())
p.unlink()
PY
```

预期显示 `False`、`old content`、`new content`。最后一个句柄关闭且内核查找引用释放后，终端 A 显示旧节点的回收日志；卸载会释放整个内存状态。

### 5. 正常卸载（终端 B）

完成后在挂载目录外卸载。如果终端当前位于挂载目录内，先运行 `cd "$HOME"`，并关闭仍在使用挂载目录的文件或程序。

```bash
fusermount3 -u "$HOME/mnt/os-task1"
```

终端 A 应输出 `MemFs unmounted.` 并退出。挂载期间保持终端 A 运行，以正常卸载作为本阶段的结束方式。

## 关键实现设计

### 状态管理与并发访问

状态分成三层：`Node` 保存属性、内容和内核查找引用数；`FsState` 保存根目录属性、目录项表、节点表、打开句柄表及编号分配状态；`MemFs` 使用 `Mutex<FsState>` 保护统一状态。状态锁离开作用域时自动释放；锁中毒返回 `EIO`。当前预置文件由初始化专用的 `add_file` 插入，添加更多预置文件无需修改 FUSE 回调。

### 文件名、句柄与节点回收

`unlink` 立即移除名称、减少链接数并更新时间；旧节点暂时可能仍在内存中。`open` 和 `create` 分配独立句柄，句柄保存 inode 和访问模式。读写校验句柄与节点的对应关系；删除后已有句柄仍可读写和截断，同名重建指向独立节点。

成功的 `lookup` 和 `create` 回复各增加一个 `nlookup`，内核通过 `forget` 减少查找引用（批量通知沿用 fuser 的默认转发）。只有 **nlink=0、没有打开句柄、nlookup=0** 同时成立才回收节点。`release` 和 `forget` 都尝试回收，不依赖它们的到达顺序。`flush` 不释放句柄：dup 产生的描述符可多次关闭，但同一次打开只在最后关闭时触发一次 `release`。日志中的 `reclaim: inode=...` 是实际回收的证据。

### 目录遍历与续读

目录按单调分配的 inode 排序，`.` / `..` 使用 cookie 1 / 2，普通文件使用 inode+1；下次返回 cookie 大于请求 offset 的条目。cookie 是用于恢复遍历位置的标记，删除前面的条目不会移动后续条目的 cookie。inode 不复用，同名重建也会获得新的 inode。遍历期间新建的文件可能出现在后续结果中，此实现不是目录快照。

### 权限、写入与属性更新

挂载为读写模式，保留 `DefaultPermissions`，根目录权限为 `0755`，预置文件为 `0644`。新文件使用创建请求的 UID/GID 和经 umask 过滤的普通权限位。`create` 在持锁期间同时更新目录项、节点表和父目录时间。

`write` 检查范围并按偏移写入，空隙补零；`setattr` 支持文件大小、访问时间和修改时间，其他未支持的属性修改明确返回错误。缩短删除尾部，扩长补零，写入和截断同步更新大小、块数、mtime 与 ctime。当前读取不会自动更新 atime。

为便于教学观察，属性缓存 TTL 为 0，文件打开使用 `FOPEN_DIRECT_IO`，不启用 writeback cache；内容读写直接进入回调。未请求 `ATOMIC_O_TRUNC`，Linux 在覆盖打开前通过 `setattr(size=0)` 截断，追加的偏移由内核提供。此方案不支持 mmap，且没有针对高并发优化。

## 阅读代码的顺序

代码按职责拆成三个模块，保留现有回调流程与学习注释：

| 文件 | 职责 | 重点入口 |
| --- | --- | --- |
| `src/main.rs` | 参数检查、挂载配置与启动 | `main` |
| `src/state.rs` | 内存数据结构、内容扩容、句柄管理与回收；末尾保留回收单元测试 | `Node`、`FsState` |
| `src/filesystem.rs` | 持锁处理 FUSE 请求，将结果或错误回复给内核 | `MemFs`、`impl Filesystem` |

`main` 创建 `MemFs`，`MemFs` 持有 `Mutex<FsState>`。回调在锁内检查请求并修改状态；节点大小和时间由 `Node` 方法维护，句柄检查及回收由 `FsState` 方法处理。状态类型和必要成员仅向父模块开放，供同一程序中的 FUSE 适配层使用。

展示时可以围绕两条主线讲解：

- **名称到内容**：`lookup` 用文件名查询 `entries`，得到 inode；`read` 校验句柄后，用 inode 查询 `nodes` 并读取字节。
- **删除到回收**：`unlink` 移除名称；`release` 释放打开句柄，`forget` 减少内核查找引用；三种引用全部消失后，`reclaim` 移除节点。

具体阅读顺序如下，除标注的状态方法外，文件操作回调均在 `filesystem.rs`：

1. `main`：读取挂载点、验证空目录，设置读写挂载选项并启动服务。
2. `FsState::new` / `add_file`：初始化根目录（inode 1），分配两个文件的 inode（2、3），同时填入目录项与节点表。UID/GID 使用挂载目录的所有者。
3. `lookup`：通过父目录和名称找到文件。
4. `getattr`：通过 inode 返回类型、大小、权限和时间。
5. `readdir`：列出 `.`、`..` 和目录项表中的所有文件，正确处理遍历位置和缓冲区满的情况。
6. `read`：在文件边界内按 offset 和 size 返回字节。
7. `create` / `open`：创建空节点或打开已有节点，设置 direct I/O。
8. `write` / `Node::resize` / `Node::changed`：写入、扩容补零、同步属性。
9. `setattr`：响应截断及时间更新，拒绝尚未支持的属性修改。
10. `unlink` / `release` / `forget` / `FsState::reclaim`：区分名称、打开句柄和内核引用的生命周期。

普通命令经 Linux VFS、FUSE 内核驱动和 fuser 分发到这些回调。日志中的回调顺序和次数受缓存影响，不保证每次命令都触发全部回调。

## 测试与结果判定

### 状态回收单元测试

在项目根目录运行：

```bash
export CARGO_TARGET_DIR="$HOME/.cache/os-task1-target"
cargo test --locked
```

`src/state.rs` 中的单元测试检查：存在目录链接、任一打开句柄或内核查找引用时，节点不能提前回收；最后一种引用消失后才能回收，且不依赖句柄释放与查找引用释放的先后顺序。成功时应显示 `test result: ok`。此测试无需实际挂载，但不能代替下述 FUSE 集成检查。

### 独立挂载集成检查

在项目根目录运行；同一时间只运行一次检查，以免多个测试覆盖同一日志：

```bash
export CARGO_TARGET_DIR="$HOME/.cache/os-task1-target"
cargo build --locked
bash scripts/smoke-test.sh
```

脚本读取上述构建目录中的 `debug/mini_memfs`，不会自动重新编译，因此每次修改源码后都应先执行 `cargo build --locked`。脚本会使用 Linux 临时空目录独立挂载，无需手动启动文件系统，也不会使用前面演示的挂载点。

| 检查内容 | 验证方式与预期行为 |
| --- | --- |
| 预置文件与读取边界 | 内容、大小与 inode 正确；到达文件末尾后读取返回空字节 |
| 创建和权限 | 按 umask 设置权限，UID 与创建用户一致 |
| 写入与截断 | 偏移写入、补零、追加、覆盖及缩短/扩长结果正确 |
| 时间与错误处理 | 修改时间正确；缺失文件、排他重名创建、超过 16 MiB 的操作失败 |
| 文件隔离 | 修改一个文件不影响其他文件 |
| 删除生命周期 | 多次打开、dup、删除后读写与截断、同名重建均符合预期，旧 inode 最终回收 |
| 目录续读 | 删除已遍历条目并插入新文件后，按保存的 cookie 续读，无重复或遗漏 |
| 正常卸载 | 成功卸载且文件系统进程正常退出 |

生命周期和目录续读由脚本调用 `scripts/lifecycle-test.py` 完成。全部通过时，终端应显示以下四条成功信息，脚本退出码为 0：

```text
PASS: read/create/write/append/truncate, zero fill, permissions, isolation, errors, limits
PASS: unlink, multiple opens, dup, ftruncate after unlink, name reuse, inode reclamation
PASS: directory resume cookie survives deletion and insertion
PASS: clean unmount and process exit
```

以上是预期成功输出。文件系统回调日志写入 `artifacts/hello-fs.log`，每次检查会覆盖该文件；四条 PASS 信息由测试脚本输出到终端，不会自动写入该日志。提交包中的已有回调日志可作示例，当前环境是否通过应以本次实际运行结果为准。

脚本会在退出时尝试清理挂载和临时目录。若出现 Python 断言错误、挂载失败或清理错误，应结合终端报错与日志定位原因，不能只凭某一条 PASS 判断全部通过。

## 常见问题

| 现象 | 检查与处理 |
| --- | --- |
| 找不到 `cargo` 或 Rust 版本过旧 | 在 Ubuntu 内安装 Rust，运行 `source "$HOME/.cargo/env"`；必要时更新 stable 工具链 |
| 找不到 `cc`、`fusermount3` 或 `python3` | 按环境准备步骤安装对应 Ubuntu 软件包 |
| `/dev/fuse` 不存在或没有访问权限 | 确认正在使用支持 FUSE 的 Linux/WSL2 环境，并检查设备及当前用户权限 |
| 提示挂载点必须是已有空目录 | 先创建目录，确认其中没有文件，且没有被其他实例挂载 |
| 测试提示 `Build first: cargo build --locked` | 在同一终端设置 `CARGO_TARGET_DIR` 并编译；只执行默认目录下的构建与脚本约定的路径可能不一致 |
| 卸载提示目录忙（busy） | 所有终端先离开挂载目录，关闭仍在使用其中内容的文件或程序，再执行 `fusermount3 -u` |
| 程序异常退出后访问目录提示 `Transport endpoint is not connected` | 在挂载目录外执行 `fusermount3 -u <实际挂载路径>` 清理残留挂载，再重新启动 |

挂载和文件操作使用普通用户执行。卸载后文件内容消失属于本项目的内存存储设计；重命名、创建子目录和修改权限等未支持的操作失败，也属于下述实验边界。

## 实验总结与局限

本项目完成了一个单层内存文件系统案例：将普通 Linux 文件操作映射为 Rust 回调，使用统一状态管理目录项、内容和打开句柄，并通过引用条件区分删除名称与回收节点。测试脚本覆盖基本文件操作、典型错误及删除生命周期，便于课程展示和复现。

当前边界如下：

- 仅支持根目录下的普通文件；尚未实现重命名、子目录、硬链接、符号链接、chmod/chown 或持久化，不是完整 POSIX 文件系统。
- 单文件最多 16 MiB，最多 256 个存活文件节点，包括预置文件和已删除但尚未回收的节点；超限分别返回 `EFBIG`、`ENOSPC`，内容扩容分配失败返回 `ENOMEM`。回收节点会释放名额。
- 上述限制不是严格的进程总内存配额；`Vec` 截短后可能保留分配容量，节点回收时其内容缓冲区才随之释放。
- 单元测试聚焦回收条件，挂载脚本覆盖上表列出的场景；尚未提供针对 256 节点上限、内存分配失败或高并发负载的完整自动化验证。
- 使用统一互斥锁、零属性缓存 TTL 和 direct I/O 便于观察行为，未针对性能优化；不支持 mmap，读取不自动更新 atime。

后续可扩展目录层级、重命名和持久化，并进一步完善容量、并发与异常恢复验证。课程展示 PPT、视频等支撑材料与源码包分别提交，README 提供源码包内的项目说明及复现步骤。

## 参考资料与实现说明

- [作业要求](assignment.txt)
- [fuser 项目与原 fuse crate 的关系](https://github.com/cberner/fuser#about)
- [fuser 0.18.0 API 文档](https://docs.rs/fuser/0.18.0/fuser/)
- [fuser 0.18.0 特性配置](https://docs.rs/crate/fuser/0.18.0/features)
- [官方 Hello 示例](https://docs.rs/crate/fuser/0.18.0/source/examples/hello.rs)
- [Rust 官方安装说明](https://rust-lang.org/tools/install/)
- [Ubuntu 24.04 fuse3 软件包](https://packages.ubuntu.com/noble/fuse3)

代码参考官方 HelloFS 的基本回调结构，在本实验中扩展了多文件状态、创建与写入、截断、删除、句柄管理、节点回收和目录续读；按入口、FUSE 适配层与状态层拆分代码，并配有验证脚本。
