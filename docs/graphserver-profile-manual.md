# GraphServer profile 手工分析与验证

本文说明当 PXLogic Bridge 的离线自动分析无法识别某个 Logic 2
GraphServer 时，如何手工生成、验证和维护兼容 profile。整个流程不要求 Bridge
联网，也不应修改用户的 Logic 安装目录。

## 自动分析与手工流程的关系

Bridge 启动时先按 `platform + architecture + identity + SHA-256` 查找内置 profile。
精确命中时不会运行自动定位器。未知二进制会在本机使用 Logic 自带的 Node 执行
只读分析，并将结果写入应用配置目录下的
`compatibility-analysis.json`：

- `candidate`：找到唯一候选，可以进入界面明确标注的实验验证路径；
- `unsupported`：证据缺失或不唯一，不允许注入；
- `verified`：只来自随 Bridge 发布的内置 profile，或经过本文真机流程后人工晋级。

候选和失败缓存都绑定分析器版本。Bridge 升级分析器后，旧记录会失效并自动重试；
界面的“重新分析”也会忽略当前版本缓存强制重试。整个过程不访问网络。

开发者也可以直接运行同一个分析器：

```sh
node tools/logic2-bridge/scripts/analyze-graph-compatibility.cjs \
  "/path/to/GraphServer-binary" \
  --logic-version 2.x.y \
  --cache /tmp/compatibility-analysis.json
```

自动成功只证明定位证据唯一、文件到运行时偏移映射成立且入口 patch window 可以
安全搬移，不证明 ABI、buffer 语义和真机采集正确。分析器会记录可识别的 ABI
模式作为置信证据，但不会把它们当成所有编译版本都必须生成的固定机器码。因此它
不会自动写入仓库的内置 profile，也不会自动标记为 `verified`。自动失败或实验
捕获异常时，从下述手工流程继续。

## 安全边界

Bridge 通过修改 GraphServer 的 `LogicDeviceNode::OnDataBuffer` 函数入口，将
PXLogic 数据写入 GraphServer 已经分配的数字采样 buffer。错误的地址、函数 ABI
或 buffer 布局可能造成 GraphServer 崩溃或内存破坏。

因此必须遵守以下规则：

- Logic 版本号不能单独授权注入；
- profile 主键是平台、架构、GraphServer identity 和 SHA-256；
- 未确认的 profile 只能标记为 `candidate` 或
  `pending-live-validation`；
- 不要在用户的原始 GraphServer 文件上写入任何字节；
- 不要把“进程没有崩溃”当成真机验证成功；
- identity、SHA、入口机器码或 ABI 任一项不符时立即停止。

当前 native host 支持的组合为：

| 平台 | 架构 | 二进制 | identity |
| --- | --- | --- | --- | --- |
| macOS | arm64 | Mach-O | `LC_UUID` |
| Windows | x64 | PE | CodeView GUID + Age |
| Linux | x64 | ELF | GNU Build ID |

macOS x64 尚没有 native hook 实现。为它找到偏移并不代表 Bridge 已经支持该
平台。

## 一、保留输入和基线

记录以下信息：

- Logic 2 完整版本；
- 操作系统版本和 CPU 架构；
- Logic 安装路径；
- GraphServer 文件路径、大小和修改时间；
- Bridge 版本或 commit；
- 一个已验证 profile 作为对照。

不要直接修改 Logic 安装中的二进制。需要使用反汇编工具时，先复制 GraphServer
到独立分析目录，并确认副本 SHA-256 与原文件一致。

## 二、提取指纹

从仓库根目录运行：

```sh
node tools/logic2-bridge/scripts/fingerprint-installation.cjs \
  "/path/to/Logic-or-Logic.app"
```

也可以直接分析动态库：

```sh
node tools/logic2-bridge/scripts/fingerprint-binary.cjs \
  "/path/to/graph_server_shared"
```

输出至少应包含：

```json
{
  "format": "mach-o | pe | elf",
  "architecture": "arm64 | x64",
  "identityKind": "...",
  "identity": "...",
  "sha256": "..."
}
```

先在 `tools/logic2-bridge/compatibility/profiles.json` 中查找完全相同的
`platform + architecture + identityKind + identity + sha256`。如果完全命中，
即使外层 Logic 版本不同，也应复用已有 profile，不需要重新分析。

如果 identity 相同但 SHA 不同，仍视为未知二进制。签名、重打包或局部补丁都可能
改变代码，不能只根据 identity 继续运行。

## 三、定位 OnDataBuffer

目标函数的已知名称和签名字符串为：

```text
Saleae::Graph::LogicDeviceNode::OnDataBuffer
void __cdecl Saleae::Graph::LogicDeviceNode::OnDataBuffer(class DeviceId,struct Saleae::Buffer)
```

不同版本可能删除这些字符串。字符串不存在不代表函数一定不存在，但自动定位证据
会明显变弱，此时必须使用已验证版本做二进制差分并人工检查调用链。

### Windows x64

仓库提供 PE 候选定位器：

```powershell
node tools/logic2-bridge/scripts/locate-windows-on-data-buffer.cjs `
  'C:\Program Files\Logic\resources\windows-x64\graph_server_shared.dll'
```

脚本执行以下只读分析：

1. 在 `.rdata` 中找到方法名和签名；
2. 在 `.text` 中寻找指向这些字符串的 RIP-relative 引用；
3. 使用 `.pdata` runtime-function 表确定引用所属函数；
4. 要求所有证据收敛到唯一函数起始 RVA；
5. 要求入口机器码匹配已维护 profile 的 trampoline-safe prologue；
6. 输出 RVA、函数结束 RVA、unwind RVA 和入口机器码。

若候选数不是 1，不要手工选择“看起来最接近”的地址。使用 Ghidra、IDA、Binary
Ninja 或 WinDbg 打开引用点，确认方法名和签名确实由同一个函数使用。
如果候选唯一但 prologue 不同，自动分析仍会拒绝运行。此时必须确认完整 x64 指令
边界、PC-relative/relative 指令和 trampoline 语义，不能只把新的 16 字节加入
profile。

Windows profile 的 `onDataBufferOffset` 是 RVA，不是磁盘文件偏移。运行时目标为：

```text
GetModuleHandle(GraphServer) + RVA
```

### macOS arm64

Bridge 的首选自动路径不依赖已知版本的完整函数签名。它直接解析 Mach-O：

1. 在 `__TEXT,__cstring` 中查找 `OnDataBuffer` 和
   `logic_device_node.cpp` 诊断字符串；
2. 解码 `__TEXT,__text` 中引用这些字符串的 ARM64 `ADRP + ADD` 指令对；
3. 使用 `LC_FUNCTION_STARTS` 将引用归属到函数边界；
4. 要求两类引用收敛到唯一函数；
5. 将函数入口通过 `LC_SEGMENT_64` 映射为运行时 offset；
6. 检查将被 trampoline 覆盖的前 16 字节不含 branch、`ADR/ADRP`、literal load
   等 PC-relative 指令；
7. 记录 `x3` 参数转存和 `[x3 + 0x10]` size load 等 ABI 置信证据。

第 4 至 6 项是自动生成候选的硬门槛。第 7 项是置信信息：它存在时能显著提高
候选可信度，但编译器可能用等价指令表达同一 ABI，因此缺失时不会单独阻止
`candidate` 生成，仍必须在实验启动和真机验证阶段确认。

先检查 identity 和 load commands：

```sh
dwarfdump --uuid libgraph_server_shared.dylib
otool -l libgraph_server_shared.dylib
strings -a -t x libgraph_server_shared.dylib \
  | rg 'LogicDeviceNode::OnDataBuffer|struct Saleae::Buffer'
```

如果字符串仍存在，在 Hopper、Ghidra 或 Binary Ninja 中查看字符串交叉引用，找到
同时使用方法名和签名的函数。再与已验证版本的控制流、参数寄存器和调用点比较。

Mach-O 文件偏移必须通过包含该位置的 `LC_SEGMENT_64` 映射到运行时地址：

```text
runtime offset = segment.vmaddr - image_base_vmaddr
               + file_offset - segment.fileoff
```

不能假设所有 Mach-O 的文件偏移都等于运行时偏移。当前 2.4.46 dylib 的布局刚好
允许得到已记录的 `0x1df994`，新版本仍需重新检查 segment。

ARM64 trampoline 会覆盖至少 16 字节。入口范围内若包含 PC-relative branch、
`adr`、`adrp`、literal load 或依赖当前位置的指令，简单复制到 trampoline 后语义
可能变化，必须先重定位这些指令或扩大/调整 hook 方案。

### Linux x64

先检查 ELF 信息和字符串：

```sh
readelf -n libgraph_server_shared.so
readelf -lW libgraph_server_shared.so
strings -a -t x libgraph_server_shared.so \
  | rg 'LogicDeviceNode::OnDataBuffer|struct Saleae::Buffer'
objdump -d -C -Mintel libgraph_server_shared.so > graph-server.asm
```

在反汇编器中检查字符串的 RIP-relative 引用，并与已验证函数比较。文件偏移通过
包含它的 `PT_LOAD` segment 转换为虚拟地址：

```text
runtime offset = segment.p_vaddr + file_offset - segment.p_offset
```

Linux x64 trampoline 至少覆盖 12 字节。被复制的入口指令不能包含未修正的
RIP-relative 数据访问或相对跳转。

## 四、确认 ABI 和 buffer 语义

当前 Bridge 使用的 buffer 布局为：

```c
typedef struct {
    void *data;
    void *shared_owner;
    uint64_t size;
} SaleaeBuffer;
```

当前调用约定为：

```c
// Windows x64: DeviceId 和 Buffer 都按非平凡 aggregate 间接传递
void OnDataBuffer(void *node, void *device_id, SaleaeBuffer *buffer);

// macOS arm64 / Linux SysV x64
void OnDataBuffer(
    void *node,
    uint64_t device_id_low,
    uint64_t device_id_high,
    SaleaeBuffer *buffer
);
```

反汇编时至少确认：

- `this/node`、`DeviceId`、buffer 分别来自预期寄存器或间接地址；
- 函数会读取 buffer 的 data 和 size；
- size 的单位仍然是字节；
- data 指向可写的数字采样内存；
- 原函数在回调返回前仍拥有 buffer；
- 回调不是模拟量、压缩数据或其他设备类型的路径；
- 函数没有新增必须由调用者提供的参数。

还要检查 GraphServer 导出的 `CreateGraphServer`、`DestroyGraphServer`、日志函数
是否仍存在。WebSocket 能启动并不能替代 OnDataBuffer ABI 检查。

## 五、选择 prologue

`prologueHex` 同时承担两个用途：

1. 运行时确认 profile 没有落到错误地址；
2. 指定复制到 trampoline 的完整指令范围。

长度必须满足当前平台绝对跳转大小：

- macOS arm64：至少 16 字节，并保持 4 字节指令对齐；
- Windows x64：至少 14 字节；
- Linux x64：至少 12 字节。

不要在一条指令中间截断。x64 需要由反汇编器确认指令边界；仅复制固定的前 16
字节并不总是正确。

`locatorSignatureHex` 是 Linux 自动定位和 macOS 结构分析失败时的回退方式。使用
该方式时必须显式记录至少 32 字节，并确保它在目标二进制中唯一出现；它应从已
验证函数入口开始且包含 `prologueHex`。macOS ARM64 若已通过字符串交叉引用、
`LC_FUNCTION_STARTS` 和入口重定位安全检查得到唯一函数，则不要求与旧版本保持
完整函数签名相同。

## 六、创建 profile

在 `tools/logic2-bridge/compatibility/profiles.json` 中加入一项：

```json
{
  "id": "logic-<version>-<platform>-<arch>-<identity-prefix>",
  "logicVersion": "<version>",
  "platform": "darwin | win32 | linux",
  "architecture": "arm64 | x64",
  "runtimeLayout": "<package-layout>",
  "graph": {
    "relativePath": "<path-inside-Logic>",
    "format": "mach-o | pe | elf",
    "identityKind": "<identity-kind>",
    "identity": "<exact-identity>",
    "sha256": "<exact-sha256>"
  },
  "hook": {
    "status": "pending-live-validation",
    "onDataBufferOffset": "0x...",
    "prologueHex": "...",
    "validation": "说明定位证据、ABI 检查和仍缺少的验证"
  }
}
```

只读静态分析生成的本机记录使用 `candidate`。人工确认 ABI 后可以使用
`pending-live-validation`。只有完成本文第八节后才能使用 `verified`。

修改后运行：

```sh
npm --prefix tools/logic2-bridge test
npm --prefix tools/logic2-bridge run check
```

## 七、隔离启动和失败恢复

先执行不启动 GraphServer 的路径/profile 检查：

```sh
node tools/logic2-bridge/index.cjs \
  --app "/path/to/Logic" \
  --compatibility-profiles /path/to/compatibility-analysis.json \
  --allow-pending-profile \
  --dry-run
```

再在没有重要采集任务的环境中进行实验启动。保存以下日志：

- Bridge 运行记录；
- `graphio.log`；
- native host 的 identity、prologue 和 hook 地址；
- 操作系统崩溃报告或 debugger 调用栈。

出现下列任一情况立即停止并将 profile 标记为不可用：

- identity 或 SHA 不匹配；
- `OnDataBuffer prologue mismatch`；
- native host 在安装 hook 时崩溃；
- GraphServer 无法监听 WebSocket；
- buffer size 异常、未对齐或持续 underflow；
- 波形通道错位、采样率错误或 decoder 大量误码。

Bridge 的 patch 只存在于子进程内存。退出 Bridge 和 Logic 后会消失，不需要恢复
Logic 文件。如果进程未退出，先使用系统任务管理器结束 Bridge 的 native host，
再结束由 Bridge 启动的 Logic 进程。不要删除用户的 `.sal` 或 Logic 配置来处理
native hook 故障。

## 八、真机验证和晋级

依次执行：

1. 单通道固定方波，核对频率和边沿数量；
2. D0-D3 四线 SPI，核对至少一个已知事务；
3. 稀疏通道，例如 D0、D4、D9；
4. 多种采样率；
5. PXLogic 1.8 V、2.5 V、3.3 V 和 5.0 V 硬件档位；
6. Logic 软件 trigger 和 glitch filter；
7. SPI、I2C 或 UART 原生 decoder；
8. 保存 `.sal`、关闭并重新打开；
9. 检查 callback、queued、underflow 和 dropped 统计。

晋级为 `verified` 前必须满足：

- exact identity、SHA 和 prologue 运行时检查通过；
- GraphServer 和 Logic WebSocket 正常启动；
- 真实 PXLogic 数据进入 OnDataBuffer；
- 通道、采样率、波形和至少一个 decoder 结果正确；
- trigger、停止采集和 `.sal` 保存恢复正常；
- 没有 ABI 崩溃、数据错位或持续丢包。

在 `validation` 中记录验证的 Logic 版本、平台、架构和测试范围。提交 profile 时
不要提交 Saleae 二进制，只提交指纹、偏移、入口机器码和分析报告。

## 九、自动分析失败报告

自动分析失败时至少保留：

```text
Logic version
platform / architecture
GraphServer path
format / identity kind / identity / SHA-256
analyzer version
failure reason
method-name/signature string occurrences
candidate count
```

本地失败记录不是永久结论。自动分析器版本变化后应重新分析。同一个二进制在旧
分析器中为 `unsupported`，可能在新版本 Bridge 中被识别，但仍需要上述人工 ABI
和真机验证流程。

## 十、分析器维护规则

离线分析实现位于：

```text
tools/logic2-bridge/lib/offline-compatibility.cjs
tools/logic2-bridge/lib/macos-hook-locator.cjs
tools/logic2-bridge/lib/windows-hook-locator.cjs
```

维护时遵守以下约束：

- 定位策略、证据规则或安全校验发生实质变化时，递增
  `compatibility/profiles.json` 中唯一的 `analyzerVersion`，使旧候选和失败记录
  自动失效；
- 只修改日志文本或测试时不要递增版本，避免无意义地重新扫描；
- 新增 Linux 已验证 profile 时记录至少 32 字节且唯一的
  `locatorSignatureHex`；macOS 已验证 profile 可以保留该字段作为结构定位失败时的
  回退证据；
- 新增 Windows profile 前确认 `prologueHex` 覆盖完整指令、没有需要重定位的
  RIP-relative/relative 指令，并完成本文第八节；
- 自动生成的本地 cache 不直接提交为内置 profile；先保留分析证据并人工验证；
- 内置 profile、定位器和 native trampoline 必须在同一次变更中保持一致。

每次维护至少运行：

```sh
pnpm check
pnpm test
pnpm build
```

跨平台 profile 或打包资源发生变化时，还要通过 GitHub Actions 的 macOS、Windows
和 Linux 构建检查。发布包不得包含 Saleae 二进制或用户本地 cache。

## 十一、官方 macOS 旧版回归记录

2026-08-07 至 2026-08-08 对多个官方 macOS arm64 发行包进行了回归。下载保留的 app 通过 Apple
签名和 Gatekeeper 公证校验，并能使用各自内置的 Electron/Node 执行离线分析器：

| Logic | Mach-O UUID | SHA-256 | 分析器版本 | 自动结果 |
| --- | --- | --- | --- |
| 2.4.36 | `43DF238D-8DB7-3E9C-B5FB-DA746EDF6B2A` | `cf4ad1c3351f6037685547df5fe6f58be08cf74f2fd5d518c17aa608eb460c9c` | v2 | `verified`，结构定位 `0x1a536c`；25 MHz、D0-D1 真机捕获及 UI 启停通过，零丢包 |
| 2.4.43 | `6074C56C-170E-3330-9940-BB549DFBDFAA` | `0b57f99bdbbf5efaf017d0e9374968e6978660971e12195536dcb54dd720bc70` | v2 | `candidate`，结构定位到 `0x1b8e68`，入口安全，发现 `x3` buffer 和 `[x3+0x10]` size 证据 |
| 2.4.45 | `7C6F5ED6-F84B-35E2-B528-2A74ADDAD3F3` | `6254f4c821c462bbcea2e37d0f5942afe483519579d1e0110f0c4f27e50e270a` | v2 | `verified`，结构定位 `0x1df9c0`；6.25 MHz、D0-D15 真机捕获及 UI 启停通过，零丢包 |

这些 v2 结果由纯 Node 静态分析得到，不依赖 LLDB、Hopper 或其他随包调试工具。
2.4.36 和 2.4.45 已完成真实 PXLogic 捕获，因此加入内置 `verified` profile，客户端
直接显示已兼容，不再经过旧 v1 的“不可用”状态。2.4.45 测试同时补齐了 PXLogic
100 MHz 基准时钟的精确 6.25 MHz 分频档位。

2.4.43 使用本地 profile 的隔离 native host 已通过 UUID、prologue、hook 安装和
GraphServer WebSocket 监听检查；它尚未完成真实 PXLogic 捕获，所以不会加入内置
`verified` profile。
