# 在浏览器里复用这份语言服务

编辑器不在这个仓库里。换前端时保留这里编出的 wasm，按下面的宿主契约接上即可。`.cppls.toml` 的字段仍以 [`configuration.md`](configuration.md) 为准；这一份只写当前工作区实际启用的配置，以及页面必须提供的运行环境。

服务器只分析客户端给出的第一个 workspace folder。

## 构建

父仓库的 `ui/scripts/build-cpp-ls-wasm.sh` 在本目录执行：

```bash
rustup target add wasm32-wasip1
cargo rustc -p cpp_ls --target wasm32-wasip1 --release --lib --crate-type cdylib --no-default-features -- \
  -C panic=abort \
  -C link-arg=--export-memory \
  -C link-arg=--max-memory=536870912 \
  -C link-arg="$(rustc --print target-libdir --target wasm32-wasip1)/self-contained/crt1-reactor.o"
```

产物是 `target/wasm32-wasip1/release/cpp_ls.wasm`。这是一个 wasip1 reactor：模块自己定义内存，不共享，上限 512 MiB（8192 页），没有 start 段。链接参数只写在这条命令上，不进 `.cargo/config.toml`。

相对本机二进制，wasm 构建做了这些取舍：

- 不链接 `notify` 和 `mimalloc`。目录变化没有 inotify，由客户端的 LSP 通知推进。
- `tokio` 只留 `macros` 和 `sync`。没有运行时，睡眠、生成任务和让出都走本 crate 的 `rt`，并且只能在 `drive` 里调用。
- 分析缓存的行不经 zstd，按原文写入。

## wasm 上不能调用的 std 函数

wasi 的 std 里有些函数不是返回错误，而是直接 panic。`std::env::temp_dir()` 会以 `no filesystem on wasm` panic。会在 wasm 上执行到的代码路径不能调用它；索引跑完后写 `cpp_ls-memory.txt` 的那一处已经按 `target_arch = "wasm32"` 跳过。新增调用时同样处理。

## 导入与导出

导出 `_initialize`、`memory`、`cpp_ls_alloc`、`cpp_ls_send`、`cpp_ls_poll`。

`wasi_snapshot_preview1` 导入：`environ_get`、`environ_sizes_get`、`clock_time_get`、`random_get`、`proc_exit`、`fd_close`、`fd_fdstat_get`、`fd_fdstat_set_flags`、`fd_filestat_get`、`fd_filestat_set_size`、`fd_pread`、`fd_pwrite`、`fd_read`、`fd_write`、`fd_readdir`、`fd_sync`、`fd_prestat_get`、`fd_prestat_dir_name`、`path_create_directory`、`path_filestat_get`、`path_open`、`path_readlink`、`path_rename`、`path_unlink_file`。

另外一个导入是 `cppls.host_emit(ptr, len)`。没有 `args_get`、`poll_oneoff`、`sched_yield`、`thread-spawn`，也不导入内存。

实例化之后先调用一次 `_initialize`。内存要等 `exports.memory` 出现再读：`cpp_ls_alloc` 可能把线性内存 grow 掉，旧的 `ArrayBuffer` 会 detach，所以拷贝必须发生在 `cpp_ls_alloc` 返回之后。

## 三个调用

页面只有一条 Worker，wasm 同时只进入一层。`host_emit` 返回之前不能再调用 `cpp_ls_*`。

`cpp_ls_alloc(len)` 分配 `len` 字节并返回指针。`len <= 0` 或分配失败时返回 `0`。调用方拥有这块缓冲，直到把它交给 `cpp_ls_send`。

`cpp_ls_send(ptr, len)` 拿走 `cpp_ls_alloc` 返回的缓冲。`ptr` 为 0 或 `len` 为 0 会 trap。

`cpp_ls_poll()` 继续已经在等的任务。多 poll 一次没有副作用。

两者都返回：

| 值 | 含义 |
| --- | --- |
| `-2` | 会话结束 |
| `-1` | 空闲，不必再进 |
| `0` | 下一个宏任务再 poll |
| `n > 0` | 至少过 `n` 毫秒再 poll |

其它负数、trap，以及 `cpp_ls_alloc` 返回 `0`，都是宿主崩溃。重启就是丢掉这个 Worker 再新建一个。

`host_emit` 把 `[ptr, ptr+len)` 拷进一块新的 `ArrayBuffer` 再交给页面。拷贝结束之前不能改这块线性内存，函数返回后 wasm 可以复用它。页面不在 Worker 里解析这些字节。

第一条消息必须是 `initialize`。参数不合法时先 `host_emit` 一条 `-32602`，再让会话以 `-2` 结束。合法的 `initialize` 发出结果（`typeHierarchyProvider: true`），然后进入主循环。

反应器走 `CmdArgs::browser()`，不读 `argv`。不需要 `CPPLS_NO_WATCH` 或 `HOME`。`environ` 可以为空；可选的 `CPPLS_*` 探针仍然从环境变量读。配置路径固定是 `/cppls/cppls.toml`。文件不存在时服务器记一笔，会话照样打开。

## 进程

预打开的文件描述符：

| fd / 路径 | 内容 |
| --- | --- |
| 0 | stdin。语言服务不从这里读请求；读到的是文件结束 |
| 1 | stdout，按行打到 Worker 的 `console.log` |
| 2 | stderr，按行打到 Worker 的 `console.error` |
| `/home/web_user` | 工作区根目录 |
| `/sysroot` | 标准库头文件 |
| `/cppls` | 配置文件，不在工作区里 |

`fd_write(1/2)` 用 `TextDecoder` 的流式解码，攒到换行再输出，避免一条日志被拆成多行或切断 UTF-8。

配置放在工作区之外，用户工作区里同名的 `.cppls.toml` 只是普通文件。服务器按文件名在任意层级把 `.cppls.toml` 与 `compile_commands.json` 当作配置，客户端不要对工作区里这两个文件名发 `didChangeWatchedFiles`。

## 配置

```toml
[compile]
standard = "c++17"
args = ["-std=c++17", "-D__STDC_HOSTED__=1", "-isystem/sysroot/c++/<ver>", "-isystem/sysroot/<triple>/c++/<ver>", "-isystem/sysroot/gcc", "-isystem/sysroot/include/<triple>", "-isystem/sysroot/include"]

[index]
modules = false
```

`args` 会整个替换编译数据库，所以标准库路径必须写在这里。`-isystem` 和路径之间没有额外的分隔符，拼出来是 `-isystem/sysroot/...`。路径列表来自下面 sysroot 包里的 `includes.txt`，一行一个绝对路径。`__STDC_HOSTED__` 不设的话，`<bits/stdc++.h>` 会跳过 `<iostream>` 那一整段，`cin` 和 `cout` 没有声明。

`modules = false` 是有意关掉的。`import std;` 要读一整份标准模块，冷启动要数分钟。题目源码走头文件，不走这条路径。

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

## Worker 与页面

页面发给 Worker：

- `start`：`module`、`sysrootUrl`、初始文件。每个 Worker 只收一次。解包 sysroot，写 `/cppls/cppls.toml` 和 `/home/web_user` 下的初始文件，实例化，调用 `_initialize`，再回 `ready`。`ready` 之前页面不发 `lsp`，先到的 `lsp` 是页面的 bug，Worker 直接进入崩溃。取 sysroot 期间到达的 `fs` 写进同一份内存文件系统。
- `fs`：`write` 或 `delete`，路径是 guest 绝对路径。只改内存文件系统，不进入 wasm，也不改已经安排好的醒来时间。写入按需建父目录。
- `lsp`：一条 JSON 消息。`JSON.stringify` 之后编码，`cpp_ls_alloc`，再按返回的指针拷入，然后 `cpp_ls_send`。

Worker 发给页面：

- `ready`
- `out`：一块 `ArrayBuffer`，总是 transferable。Worker 不解析内容。
- `ended`：会话正常或异常结束，返回值是 `-2`。
- `crashed`：`reason` 是 trap、分配失败或其它负数。

醒来只保留最早的截止。`0` 用一个常驻的 `MessageChannel` 排到下一个宏任务，正数用 `setTimeout`。已经发出的通道消息不必作废，多 poll 一次没有关系。

`out` 的缓冲总是移交。`start` 和 `fs` 里的字节只有在独占整块底层 `ArrayBuffer` 时才放进 transfer 列表。

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
- `workspace/semanticTokens/refresh`：先通知编辑器刷新，再回 `null`。
- 其它带 `id` 的请求回 `null`。带 `method` 的消息不是对页面调用的应答。

启动时 `/home/web_user` 里放好全部要分析的源文件。之后新建、修改、删除没有打开的文件，先 `fs` 再发 `didChangeWatchedFiles`（`CREATED` / `CHANGED` / `DELETED`）。同一条 Worker 上的消息按到达顺序处理，所以服务器读文件时已经看见新字节。服务器对已经打开的文档忽略 watched 事件，打开的文档以 `didChange` 的内容为准。

诊断从 `textDocument/publishDiagnostics` 来。服务器的 LSP 方法不用改宿主就能调用。定义类请求的结果可能是 `Location`，也可能是 `LocationLink`。

诊断是服务器自己的检查，不是 g++ 的诊断。编辑器里的标记和之后真正编译的结果可以不一致。

## 实测耗时

待复测。

## 父仓库的宿主

父仓库的实现放在 `ui/src/pages/ide/lsp/`，不随这个子模块发布：

- `wasiHost.ts`：上面列出的 WASI 导入和内存文件系统
- `wasmDriver.ts`：把 `fs` 和 `lsp` 送进反应器，并按返回值安排下一次 `cpp_ls_poll`
- `cppls.worker.ts`：解包 sysroot、写入初始文件与配置、实例化、调用 `_initialize`
- `lspClient.ts`：创建这一个 Worker，跑上面的 LSP 会话
- `ui/scripts/pack-cppls-sysroot.sh`：从评测沙盒 rootfs 打 sysroot 包
