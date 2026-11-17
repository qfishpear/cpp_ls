# 在浏览器里复用这份语言服务

编辑器不在这个仓库里。换前端时保留这里编出的 wasm，按下面的宿主契约接上即可。`.cppls.toml` 的字段仍以 [`configuration.md`](configuration.md) 为准；这一份只写当前工作区实际启用的配置，以及页面必须提供的运行环境。

服务器只分析客户端给出的第一个 workspace folder。

## 构建

```bash
rustup target add wasm32-wasip1-threads
cargo build -p cpp_ls --target wasm32-wasip1-threads --release
```

产物是 `target/wasm32-wasip1-threads/release/cpp_ls.wasm`，约 10.8 MB。链接参数已经写在 [`.cargo/config.toml`](../.cargo/config.toml) 的 `wasm32-wasip1-threads` 里：原子操作、共享内存、导入并导出 memory、导出 `wasi_thread_start`，线性内存上限 512 MiB。页面要能使用 `SharedArrayBuffer`，因此响应需要跨源隔离。

相对本机二进制，wasm 构建做了这些取舍：

- 不链接 `notify` 和 `mimalloc`。目录变化没有 inotify，由客户端的 LSP 通知推进。
- `tokio` 只用 current-thread，不启用进程、信号、网络和文件系统特性。
- 分析缓存的行不经 zstd，按原文写入。

## wasm 上不能调用的 std 函数

wasi 的 std 里有些函数不是返回错误，而是直接 panic。`std::env::temp_dir()` 会以 `no filesystem on wasm` panic。会在 wasm 上执行到的代码路径不能调用它；索引跑完后写 `cpp_ls-memory.txt` 的那一处已经按 `target_arch = "wasm32"` 跳过。新增调用时同样处理。

## 导入与导出

模块导入：

- `env.memory`：共享内存。`initial` / `maximum` 读 wasm 里 memory import 的 limits（当前 26 页与 8192 页），用同一块 `WebAssembly.Memory` 交给主实例和全部线程实例。
- `wasi.thread-spawn(startArg) -> tid`。
- `wasi_snapshot_preview1` 的这些函数：`args_get`、`args_sizes_get`、`environ_get`、`environ_sizes_get`、`clock_time_get`、`random_get`、`sched_yield`、`proc_exit`、`poll_oneoff`、`fd_close`、`fd_fdstat_get`、`fd_fdstat_set_flags`、`fd_filestat_get`、`fd_filestat_set_size`、`fd_pread`、`fd_pwrite`、`fd_read`、`fd_write`、`fd_readdir`、`fd_sync`、`fd_prestat_get`、`fd_prestat_dir_name`、`path_create_directory`、`path_filestat_get`、`path_open`、`path_readlink`、`path_rename`、`path_unlink_file`。没有 `fd_seek`；文件读写都带偏移。

模块导出 `memory`、`_start`、`wasi_thread_start`。

## 进程

实例化时的参数与环境：

```text
argv: cpp_ls --log-path none --config /cppls/cppls.toml
env:  CPPLS_NO_WATCH=1
      HOME=/home/web_user
```

`--log-path none` 避免往虚拟文件系统里写日志。`CPPLS_NO_WATCH=1` 关掉原生监视器；wasm 上的监视器本身是空实现，这个变量让启动路径不再尝试安装它。

预打开的文件描述符：

| fd / 路径 | 内容 |
| --- | --- |
| 0 | stdin，LSP 请求的字节流 |
| 1 | stdout，LSP 响应和通知的字节流 |
| 2 | stderr，日志文本 |
| `/home/web_user` | 工作区根目录 |
| `/sysroot` | 标准库头文件 |
| `/cppls` | 配置文件，不在工作区里 |

配置放在工作区之外，用户工作区里同名的 `.cppls.toml` 只是普通文件。服务器按文件名在任意层级把 `.cppls.toml` 与 `compile_commands.json` 当作配置，客户端不要对工作区里这两个文件名发 `didChangeWatchedFiles`。

## 配置

```toml
[compile]
standard = "c++17"
args = ["-std=c++17", "-isystem/sysroot/c++/<ver>", "-isystem/sysroot/<triple>/c++/<ver>", "-isystem/sysroot/gcc", "-isystem/sysroot/include/<triple>", "-isystem/sysroot/include"]

[index]
modules = false
```

`args` 会整个替换编译数据库，所以标准库路径必须写在这里。`-isystem` 和路径之间没有额外的分隔符，拼出来是 `-isystem/sysroot/...`。路径列表来自下面 sysroot 包里的 `includes.txt`，一行一个绝对路径。

`modules = false` 是有意关掉的。`import std;` 要读一整份标准模块，冷启动要数分钟。题目源码走头文件，不走这条路径。

改配置时写入 `/cppls/cppls.toml`（经下面的 VFS 队列），再对它发 `didChangeWatchedFiles`，服务器防抖后重载。

## 标准库包

服务器不内嵌 libstdc++。没有 `/sysroot` 时，`std::` 解析不到。包是一个 ustar，挂到 `/sysroot` 之后目录是这样：

```text
includes.txt
c++/<gcc-ver>/...                  来自 /usr/include/c++/<ver>
<triple>/c++/<ver>/...             来自 /usr/include/<triple>/c++/<ver>，该目录存在时才有
gcc/...                            来自 /usr/lib/gcc/<triple>/*/include
include/...                        来自 /usr/include，只含上面这些头文件用到的 glibc 头文件
```

`includes.txt` 在包的根上，不作为头文件挂进文件系统。每一行是已经带 `/sysroot` 前缀的 include 目录，也就是写进 `args` 的那些 `-isystem` 路径，顺序与 g++ 的搜索顺序一致：

```text
/sysroot/c++/<ver>
/sysroot/<triple>/c++/<ver>
/sysroot/gcc
/sysroot/include/<triple>
/sysroot/include
```

glibc 头文件是需要的。没有 `include/` 时 `<cstdio>` 不报错，服务器容忍 `<stdio.h>` 缺失，但 C 库符号只停在 `<cstdio>` 的 `using` 声明上：`fopen` 悬停只显示「`std::fopen` — a declaration」，`EOF` 跳转不到任何地方。有 `include/` 时 `fopen` 跳到 `stdio.h` 并显示完整签名，`EOF`、`FILE` 都能跳转。

不要整份拷 `/usr/include`：Debian 的这棵树里大部分符号链接指向其他架构的头文件，拷出来是坏链接，体积也是只含 libstdc++ 时的四倍以上。打包脚本用 g++ 对 `bits/stdc++.h` 与 C 标准库头文件求 `-M` 依赖闭包，只收闭包里的文件。

打这个包的脚本在父仓库 `ui/scripts/pack-cppls-sysroot.sh`，来源是评测沙盒的 rootfs，输出确定性的 ustar。

## 线程与实例化顺序

所有 Worker 都由页面创建，不由 wasm 主线程所在的 Worker 创建。原因有两个：

- wasm 主线程空闲时停在 `memory.atomic.wait` 里（tokio 的 park、`lsp-server` 的 crossbeam `recv`），它所在 Worker 的 JS 事件循环不转，发给它的 `postMessage` 会一直积压。
- 浏览器里新建的 Worker 要等创建方回到事件循环才真正启动。`thread-spawn` 是 wasm 里的同步调用，主线程随后就会等这条线程送来 `initialize`，等不到。

启动顺序：

1. 页面创建主线程 Worker。它实例化模块，告诉页面已经实例化，然后等页面放行。
2. 页面创建线程池。每个 Worker 实例化同一份 module、同一块内存，然后在自己的槽位上等。
3. 页面放行，主线程 Worker 调用 `_start`。

主实例必须是第一个实例化的。模块的 start 函数 `__wasm_init_memory` 里，只有抢到初始化标志的那个实例会把 `__tls_base` 设成静态 TLS 区，其他实例的 `__tls_base` 保持 0，直到 `wasi_thread_start` 为它们设置。主实例如果不是第一个，它的线程局部变量落在地址 0，tokio 在 `block_on` 处 panic（`Cannot start a runtime from within a runtime`）。线程实例在主实例之后实例化没有问题：它们不再初始化内存，也不碰主线程的 TLS。

`thread-spawn(startArg)` 在调用方线程里同步执行：用 CAS 认领一个就绪槽位，分配线程号（从 1 递增，所有线程共用一个计数器），写入 `tid` 与 `startArg`，把槽位标成已分配并 `notify`，返回线程号。没有就绪槽位时返回负的 errno（`-6`）。线程池 Worker 被唤醒后调用 `wasi_thread_start(tid, startArg)`；它返回后，Worker 重新实例化模块，回到就绪状态。

实测服务器只派生 4 个线程：`lsp-server` 的读、写、丢弃三条 I/O 线程，加 `cpp_ls-reader`。这 4 个起不来服务器会 panic（`lsp-server` 对 `spawn` 用的是 `unwrap`）。wasi 上 `available_parallelism` 不可用，索引不开工作线程；即使开，起不来的工作线程会退回当前线程，不影响正确性。线程池至少 4 个槽位，留余量取 8。

辅助线程实测不调用 `path_open`。文件系统可以只放在主线程 Worker 里，辅助线程的路径类调用返回 `ENOTCAPABLE`（76）。

## 页面与线程之间的通道

全部通道都是 `SharedArrayBuffer` 加 `Atomics`，不经过 `postMessage`。页面这一侧用 `Atomics.waitAsync` 等待，wasm 线程那一侧用 `Atomics.wait`。下面的偏移都按 int32 下标给出，数据区按字节偏移给出。

### stdin

LSP 走 stdio 的 `Content-Length` 帧：`Content-Length: N\r\n\r\n` 加 N 个字节的 JSON。服务器用 `BufRead` 从字节流里读帧，所以通道搬运的是字节块，一帧可以分多块送达，没有单帧大小限制。

| 位置 | 含义 |
| --- | --- |
| int32[0] | `0` 空，`1` 有一块数据 |
| int32[1] | 这一块的字节数 |
| int32[2] | 读出方已经读到的偏移 |
| 字节 16 起 | 数据，容量 1 MiB |

写入方等到状态为 0，拷入至多 1 MiB，写长度，偏移清零，状态设为 1，`notify`。读出方（`fd_read(0)`）等到有未读字节，按请求的长度拷走一部分并推进偏移；读完时把状态设回 0 并 `notify`。

### stdout 与 stderr

各一个环形缓冲区，可能有多个线程写（stderr 尤其如此），页面一个读者。

| 位置 | 含义 |
| --- | --- |
| int32[0] | 写入方之间的锁 |
| int32[1] | head，写入方累计写入的字节数，无符号自由回绕 |
| int32[2] | tail，读出方累计读走的字节数 |
| 字节 16 起 | 数据，容量 1 MiB（必须是 2 的幂） |

`fd_write(1/2)` 拿锁，空间不够时在 tail 上等，按 `head & (容量 - 1)` 回绕拷入，推进 head 并 `notify`，释放锁。页面在 head 上等，读出 `head - tail` 个字节，推进 tail 并 `notify`。stdout 的字节再按 `Content-Length` 分帧交给 LSP 客户端；stderr 是日志文本。

### VFS 队列

文件系统在主线程 Worker 的 JS 堆里，只能由主线程修改。页面把修改追加到队列里，不等执行。

| 位置 | 含义 |
| --- | --- |
| int32[0] | 页面与主线程之间的锁 |
| int32[1] | 队列已用字节数，`0` 表示空 |
| int32[2] | 最后入队的序号 |
| int32[3] | 最后执行的序号 |
| int32[4] | 最近一次失败的 errno |
| int32[5] | 累计失败次数 |
| 字节 64 起 | 记录，容量 4 MiB |

每条记录是 int32 序号、int32 操作（`1` 写入，`2` 删除）、int32 路径字节数、int32 数据字节数，后接 UTF-8 路径与数据，整体补齐到 4 字节。路径是 guest 绝对路径（`/home/web_user/...`、`/cppls/...`）。移动拆成写入新路径与删除旧路径两条。写入会按需创建父目录。

主线程每次进入任意 WASI 导入时，先看 int32[1]；不为 0 就拿锁、按顺序执行全部记录、把已用字节数清零、写最后执行的序号、释放锁并 `notify` int32[3]。主线程在 `poll_oneoff` 里等待时也等在 int32[1] 上，入队后立即醒来。

页面的用法是先入队，紧接着经 stdin 发 `workspace/didChangeWatchedFiles`（`CREATED` / `CHANGED` / `DELETED`）。这样不需要 ack：服务器处理这条通知时读文件必然经过主线程的 `path_open`，而那次调用开头已经把排在前面的修改落地。服务器对已经打开的文档忽略 watched 事件，打开的文档以 `didChange` 的内容为准。

页面可以用 int32[3] 判断语言服务是否卡死：队列里最早一项入队后数秒仍未执行，而这期间 stdin 上发过消息，主线程就是卡住了。

### 生命与放行

一块 8 字节的 `SharedArrayBuffer`：int32[0] 为 `1` 表示运行，页面停止时清零并 `notify` 各通道，阻塞在 `fd_read`、`poll_oneoff` 与线程池槽位上的线程据此退出；int32[1] 是上面「页面放行」的标志。

## poll_oneoff

主线程上不能按时钟订阅一直睡到截止时间。宿主把主线程的等待截到 50 ms，并等在 VFS 队列的 int32[1] 上。实测主线程有定时器挂着时会带约 1 s 的时钟订阅进来。没有定时器时，按 tokio 的 park 实现它停在原子等待里，不进 `poll_oneoff`。VFS 队列不依赖它进来。

辅助线程在 stdin 订阅（tag 1，fd 0）上等 stdin 通道，时钟订阅（tag 0）按超时睡。stdout 与 stderr 的写订阅（tag 2）总是立即就绪。

## 编辑器要讲的会话

`initialize`：

```json
{
  "processId": null,
  "rootUri": "file:///home/web_user",
  "workspaceFolders": [{ "uri": "file:///home/web_user", "name": "workspace" }],
  "capabilities": {
    "workspace": { "configuration": true, "workspaceFolders": true },
    "textDocument": {
      "synchronization": { "didSave": true },
      "completion": {
        "completionItem": {
          "snippetSupport": true,
          "documentationFormat": ["markdown", "plaintext"]
        }
      },
      "hover": { "contentFormat": ["markdown", "plaintext"] },
      "definition": { "linkSupport": true }
    },
    "window": { "workDoneProgress": true }
  }
}
```

随后发 `initialized`。每个打开的文件发 `textDocument/didOpen`：

```json
{
  "textDocument": {
    "uri": "file:///home/web_user/<相对路径>",
    "languageId": "cpp",
    "version": 1,
    "text": "..."
  }
}
```

C 文件的 `languageId` 用 `c`。位置都是 LSP 的 0 起点。编辑发 `textDocument/didChange`，`contentChanges` 里放整篇新文本。保存发 `textDocument/didSave`，并带上 `text`。

服务器会反过来问这些，客户端按下面回答：

- `workspace/configuration`：每个 item 回 `null`。配置在 `cppls.toml` 里，不在客户端设置里。
- `workspace/workspaceFolders`：回上面那一个 folder。
- `window/workDoneProgress/create`、`client/registerCapability`：回 `null`。

启动时 `/home/web_user` 里放好全部要分析的源文件。之后新建、修改、删除没有打开的文件，走上面的 VFS 队列加 `didChangeWatchedFiles`，不需要重启服务器：新建的头文件可以 `#include`，删除后包含它的文件报找不到，修改没打开的头文件后跨文件跳转随之更新。

诊断从 `textDocument/publishDiagnostics` 来。服务器的 LSP 方法不用改宿主就能调用。定义类请求的结果可能是 `Location`，也可能是 `LocationLink`。

诊断是服务器自己的检查，不是 g++ 的诊断。编辑器里的标记和之后真正编译的结果可以不一致。

## 实测耗时

在 Node 下用这份契约的临时宿主测得（真实磁盘做文件系统，浏览器里尚未复测）：

| 项目 | 耗时 |
| --- | --- |
| 实例化主实例、建好 12 个槽位的线程池 | 约 130 ms |
| 从开始到 `initialize` 返回 | 约 190 ms |
| `#include <vector>` 加 `<cstdio>` 的首次诊断 | 约 2.0–2.2 s |
| `#include <bits/stdc++.h>` 的首次诊断 | 约 2.6 s |
| 重启后同一文件的首次诊断 | 约 2.6 s，没有持久缓存 |
| VFS 修改从入队到执行（空闲） | 约 1 ms |
| VFS 修改从入队到执行（解析 `bits/stdc++.h` 期间） | p50 1.5 ms，最大 86 ms |

## 父仓库的宿主

父仓库的实现放在 `ui/src/pages/ide/lsp/`，不随这个子模块发布：

- `wasiHost.ts`：上面列出的 WASI 导入、内存文件系统、`thread-spawn`
- `cppls.worker.ts`：主线程 Worker，解包 sysroot、写入初始文件与配置、实例化、等放行后 `_start`
- `cppls.thread.ts`：线程池 Worker
- `channels.ts`：stdin、stdout/stderr、VFS 队列、线程池槽位的共享内存布局
- `lspClient.ts`：创建全部 Worker，跑上面的 LSP 会话
- `ui/scripts/pack-cppls-sysroot.sh`：从评测沙盒 rootfs 打 sysroot 包
