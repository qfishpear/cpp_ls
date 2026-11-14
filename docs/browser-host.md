# 在浏览器里复用这份语言服务

编辑器不在这个仓库里。换前端时保留这里编出的 wasm，按下面的宿主契约接上即可。`.cppls.toml` 的字段仍以 [`configuration.md`](configuration.md) 为准；这一份只写当前工作区实际启用的配置，以及页面必须提供的运行环境。

服务器只分析客户端给出的第一个 workspace folder。

## 构建

```bash
rustup target add wasm32-wasip1-threads
cargo build -p cpp_ls --target wasm32-wasip1-threads --release
```

产物是 `target/wasm32-wasip1-threads/release/cpp_ls.wasm`。链接参数已经写在 [`.cargo/config.toml`](../.cargo/config.toml) 的 `wasm32-wasip1-threads` 里：原子操作、共享内存、导入并导出 memory、导出 `wasi_thread_start`，线性内存上限 512 MiB。页面要能使用 `SharedArrayBuffer`，因此响应需要跨源隔离。

相对本机二进制，wasm 构建做了这些取舍：

- 不链接 `notify` 和 `mimalloc`。目录变化没有 inotify，由客户端的 LSP 通知推进。
- `tokio` 只用 current-thread，不启用进程、信号、网络和文件系统特性。
- 分析缓存的行不经 zstd，按原文写入。

## 进程

实例化时的参数与环境：

```text
argv: cpp_ls --log-path none
env:  CPPLS_NO_WATCH=1
      HOME=/home/web_user
```

`--log-path none` 避免往虚拟文件系统里写日志。`CPPLS_NO_WATCH=1` 关掉原生监视器；wasm 上的监视器本身是空实现，这个变量让启动路径不再尝试安装它。

预打开的文件描述符：

| fd / 路径 | 内容 |
| --- | --- |
| 0 | stdin，LSP 请求帧 |
| 1 | stdout，LSP 响应和通知 |
| 2 | stderr，日志文本 |
| `/home/web_user` | 工作区根目录 |
| `/sysroot` | libstdc++ 与 gcc 的头文件 |

内存从 `env.memory` 导入，并且是共享的。`initial` / `maximum` 读 wasm 里 memory import 的 limits，用同一块 `WebAssembly.Memory` 交给主实例和后来的线程。

## 启动前放进工作区的文件

`/home/web_user` 在进程启动前就放好 `.cppls.toml` 和全部源文件，路径相对工作区根。预打开的目录在进程启动时固定。`textDocument/didChange` 更新的是已经打开的缓冲区；启动时不在这棵树上的文件，include 也找不到。之后新增文件，要重新启动服务器，或者在启动前写入这棵树。

当前工作区写入的配置：

```toml
[compile]
standard = "c++17"
args = ["-std=c++17", "-isystem/sysroot/c++/<ver>", "-isystem/sysroot/<triple>/c++/<ver>", "-isystem/sysroot/gcc"]

[index]
modules = false
```

`args` 会整个替换编译数据库，所以标准库路径必须写在这里。`-isystem` 和路径之间没有额外的分隔符，拼出来是 `-isystem/sysroot/...`。路径列表来自下面 sysroot 包里的 `includes.txt`，一行一个绝对路径。

`modules = false` 是有意关掉的。`import std;` 要读一整份标准模块，冷启动要数分钟。题目源码走头文件，不走这条路径。

## 标准库包

服务器不内嵌 libstdc++。没有 `/sysroot` 时，`std::` 解析不到。包是一个 ustar，挂到 `/sysroot` 之后目录是这样：

```text
includes.txt
c++/<gcc-ver>/...                  来自 /usr/include/c++/<ver>
<triple>/c++/<ver>/...             来自 /usr/include/<triple>/c++/<ver>，该目录存在时才有
gcc/...                            来自 /usr/lib/gcc/<triple>/*/include
```

`includes.txt` 在包的根上，不作为头文件挂进文件系统。每一行是已经带 `/sysroot` 前缀的 include 目录，也就是写进 `args` 的那些 `-isystem` 路径：

```text
/sysroot/c++/<ver>
/sysroot/<triple>/c++/<ver>
/sysroot/gcc
```

本机 g++ 打这个包的脚本在父仓库 `ui/scripts/pack-cppls-sysroot.sh`。包本身不进 git。

## stdio

LSP 走 stdio 的 `Content-Length` 帧：`Content-Length: N\r\n\r\n` 加 N 个字节的 JSON。读 stdin 的线程堵在模块里面，收不到 `postMessage`。页面和这条线程共用一块 `SharedArrayBuffer`：

| 偏移 | 类型 | 含义 |
| --- | --- | --- |
| 0 | int32 | `0` 空闲，`1` 有一帧 |
| 4 | int32 | 这一帧的字节数 |
| 8 | 字节 | 帧内容，容量 1 MiB |

写入方等到状态为 0，拷入一整帧，写上长度，把状态设为 1，再 `Atomics.notify`。读出方等到状态为 1，拷走，把状态设回 0 并 notify。超过 1 MiB 的一帧放不进这个邮箱。

stdout 用同一套帧解析之后交给页面。stderr 是日志文本。

主线程上的 `poll_oneoff` 不能按时钟订阅一直睡到截止时间。当前宿主把这类等待截到 50 ms，语言服务自己的定时器和后到的请求才能共用这条线程。辅助线程在 stdin 订阅（tag 1）上等待邮箱，时钟订阅（tag 0）可以按超时睡。

## 线程

模块导入 `wasi.thread-spawn(startArg)`，返回线程号。宿主用同一份 module、同一块共享内存和同一个邮箱再开一个 worker，调用导出的 `wasi_thread_start(tid, startArg)`。

辅助线程看不见预打开的目录，`path_open` 直接失败。它们只解析主线程已经读入的缓冲区，stdin 仍读上面那个邮箱。

## 编辑器要讲的会话

`initialize`：

```json
{
  "processId": null,
  "rootUri": "file:///home/web_user",
  "workspaceFolders": [{ "uri": "file:///home/web_user", "name": "workspace" }],
  "capabilities": {
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
    }
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

服务器会反过来问两件事，客户端按下面回答：

- `workspace/configuration`：每个 item 回 `null`。配置在 `.cppls.toml` 里，不在客户端设置里。
- `workspace/workspaceFolders`：回上面那一个 folder。

诊断从 `textDocument/publishDiagnostics` 来。当前页面实际调用的请求是 `textDocument/completion`（触发字符 `.`、`>`、`:`、`#`）、`textDocument/hover` 和 `textDocument/definition`。服务器其余的 LSP 方法不用改宿主就能调用。

诊断是服务器自己的检查，不是 g++ 的诊断。编辑器里的标记和之后真正编译的结果可以不一致。

## 现有宿主

父仓库里的一份实现，不随这个子模块发布：

- `ui/src/pages/demo/workspace/cppls.worker.ts`：WASI、解包、写入 `.cppls.toml`、实例化
- `ui/src/pages/demo/workspace/cppls.thread.ts`：辅助线程
- `ui/src/pages/demo/workspace/cpplsMailbox.ts`：帧邮箱
- `ui/src/pages/demo/workspace/clangdClient.ts`：上面的 LSP 会话
- `ui/scripts/pack-cppls-sysroot.sh`：从本机 g++ 打 sysroot 包

页面从 `/cppls/cpp_ls.wasm` 和 `/cppls/sysroot.tar` 取这两个文件。
