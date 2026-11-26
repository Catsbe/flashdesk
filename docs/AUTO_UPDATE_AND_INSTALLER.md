# FlashDesk Windows 安装包与基座免大改自动更新方案设计

> 本文档完整记录 FlashDesk Windows 端的 **安装程序 (`FlashDesk-Setup.exe`)**、**轻量稳定基座 (`base-common` / `base-runtime`)**、**复用中转端口的二进制差量补丁自动更新体系** 以及 **发版 CLI (`flashdesk-release`)** 的架构设计与运维流程。

---

## 一、设计目标与核心原则

1. **基座极简且几乎永不改动**：
   - 将“Session 0 系统服务守护与入口启动”从庞大的业务主程序中剥离，形成独立的轻量基座（`flashdesk-svc.exe` 与 `FlashDesk.exe` 启动器）。
   - **基座零网络/协议/UI/编解码依赖**：`base-common` 与 `base-runtime` 严禁引入 `tokio`、`snow`、`ed25519-dalek`、`eframe`、`openh264` 或 `protocol`，仅依赖纯标准库、`serde`/`serde_json`、`sha2`/`hex`、`semver` 与 Win32 API，体积控制在数百 KB 级，最大程度消除升级基座的必要性。
   - 一切联网检查、差量合成、Ed25519 验签与文件暂存逻辑全部运行在可热替换的核心版本槽 `versions\<ver>\flashdesk-core.exe` 中。
2. **复用现有中转服端口，零新端口与额外服务**：
   - 不额外架设 Nginx / HTTPS / OSS 下载站，直接复用现有 `flashdesk-relay` TCP 端口，通过协议新增的 `ConnectionPurpose::UpdateChannel` 进行端到端设备证书鉴权分发（优先走 `FilePreferred` 大带宽节点，不可用时自动降级到当前或其它可用中转节点）。
   - 同步支持开发机用 `flashdesk-release publish` 将 Root 签名后的补丁直接推送至云服务器 `/var/lib/flashdesk/updates/`，并保留本地产出目录兼容手动 `scp`。
3. **二进制差量补丁 + 自动全量回退 + Ed25519 双重校验**：
   - 使用纯 Rust 的 `FLDP` v1 差量补丁格式（bsdiff 风格的 `COPY_DIFF` + `INSERT` 配合 `flate2` `rust_backend` 压缩，零 C/C++ MinGW 运行时依赖）。
   - 更新清单 `manifest.json` 必须携带个人根私钥 `root.key` 的 Ed25519 签名（域前缀 `FLASHDESK-UPDATE-MANIFEST-V1`）。
   - 客户端优先下载对应当前版本的差量补丁；跨多版本未命中、打补丁失败或校验失败时，自动无缝回退下载压缩全量包；无论何种路径，合成出的 `flashdesk-core.exe` 必须精确匹配清单里的 `target_sha256` 才允许提交切换。
4. **不主动轮询、不打断远控、免 UAC 静默升级**：
   - 平时不做高频轮询，仅在 **手动点击「检查更新」**、**系统服务在线每 24 小时自动检查一次**、**主控端在设备列表远程点击「更新」触发** 三种时机查询中转服。
   - 被控端（`--agent`）由 `SYSTEM` 身份的 `flashdesk-svc.exe` 拉起，天然具备对安装目录的写权限，后续更新全程无需 UAC 弹窗；有远控/文件会话（`active_sessions > 0`）时严格挂起，空闲 10 秒后再原子切槽重启。
5. **15 秒看门狗自动回滚防失联**：
   - 切换至候选版本 `candidate` 后进入 15 秒健康验收窗口：新 `--agent` 必须稳定存活且在 `agent.status.json` 中上报匹配的 `version == candidate`；若 15 秒内异常退出（允许重试 1 次防偶发抖动），基座立即回切 `previous_good_version` 并拉黑坏版本。
6. **绝不默认安装虚拟显示器驱动（VDD）**：
   - 安装包仅部署 FlashDesk 自身二进制与服务，VDD 仍完全由用户在软件「设置」中按需自行安装与开关。

---

## 二、安装目录与数据目录布局

### 1. 程序安装目录（默认 `C:\Program Files\FlashDesk`，安装向导支持自由指定路径）

```text
<InstallDir>\
├── FlashDesk.exe                   # 稳定入口启动器（所有桌面/开始菜单快捷方式均指向它）
├── bin\
│   ├── flashdesk-svc.exe           # 稳定 Session 0 Windows 系统服务基座（FlashDeskService）
│   └── flashdesk-uninstall.exe     # 独立卸载程序（注册到 Windows「应用和功能」）
├── versions\                       # 版本槽目录（自动清理，最多仅保留 active 与 previous_good 两个槽）
│   ├── 0.1.0\
│   │   └── flashdesk-core.exe
│   └── 0.2.0\
│       └── flashdesk-core.exe
├── updates\
│   ├── cache\                      # 下载中的 .fldp 差量补丁或 .deflate 全量包
│   ├── staging\                    # 合成并 SHA-256 校验通过、等待切换的候选版本
│   └── switch_request.json         # 核心程序提交给基座服务的切换请求（基座消费后删除）
├── logs\
│   └── base-service.log            # 基座服务独立日志（追加共享写，>8 MiB 轮转 .old）
└── state.json                      # 原子写入的基座状态（base_version / active / previous_good / candidate / blacklisted / last_check_at）
```

### 2. 用户配置与跨进程协同目录（沿用既有路径，重装/卸载默认保留身份）

```text
%USERPROFILE%\.flashdesk\
├── config.json                     # 用户桌面 GUI 配置（DeviceBundle、节点列表、界面偏好）
├── root.key                        # 个人 Ed25519 根私钥（仅主控主电脑持有，绝不随服务/安装包传播）
└── flashdesk.log                   # GUI 主程序运行日志

C:\ProgramData\FlashDesk\
├── config.json                     # 系统服务共享配置（仅在有效 bundle 下同步）
├── agent.status.json               # 被控代理心跳（扩展 version 与 active_sessions 字段，供 GUI 让权与基座空闲判定）
├── update.status.json              # 更新状态快照（供 GUI 顶栏/设置页实时读取进度与切换提示）
├── ipc\
│   └── request.json                # 桌面 GUI 通知 SYSTEM 代理立即执行 CheckNow 的轻量 IPC 文件（Authenticated Users 可写）
├── flashdesk-service.log           # 兼容旧便携服务日志路径
└── flashdesk-agent.log             # Session N 被控代理日志
```

---

## 三、组件职责与通信机制

| 模块 / 产物 | Crate | 职责摘要 |
| :--- | :--- | :--- |
| **`base_common`** | `crates/base-common` | 共享纯数据结构（`InstallState`、`SwitchRequest`、`AgentStatus`、`UpdateStatusSnapshot`、`UpdateIpcRequest`、`SetupPayloadManifest`）、安装布局探测、原子写 JSON（`MoveFileExW`）、ToolHelp 进程存活探测、IPC 目录与服务交互启停 ACL 授权（`SERVICE_INTERACTIVE_CONTROL_SDDL`）、图标 RGBA 绘制（`build_app_icon_rgba`）、15 秒看门狗与版本清理纯状态机 |
| **`flashdesk-svc.exe`** | `crates/base-runtime` | 注册并运行 `FlashDeskService`（Session 0）：启用 `SeTcbPrivilege` 并通过主令牌/回退 `winlogon.exe` 令牌拉起当前槽位 `versions\<active>\flashdesk-core.exe --agent`；监控 `switch_request.json` 与 `active_sessions == 0` 连续 10 秒空闲切槽；执行 15 秒崩溃看门狗与自动回滚；支持 100 ms 可中断休眠并在停止时清理 `agent.status.json` |
| **`FlashDesk.exe`** | `crates/base-runtime` | 桌面入口启动器：读取 `state.json` 解析可用核心版本（优先 `active`，缺失回退 `previous_good` 或最高版本目录），原样透传所有命令行参数启动 `flashdesk-core.exe`（无参双击立即脱离返回；带参数则等待子进程返回其退出码）；内置多分辨率 `assets/flashdesk.ico` PE 图标资源 |
| **`flashdesk-uninstall.exe`** | `crates/base-runtime` | Windows 卸载器：支持 `--silent [--purge-data] --dir <DIR>` 与交互确认弹窗；提权后停止并删除 `FlashDeskService`、结束运行中的核心进程、移除桌面/开始菜单快捷方式及 `Uninstall\FlashDesk` 注册表项，默认保留 `.flashdesk` 与 `ProgramData`，延迟 2 秒清理安装目录 |
| **`flashdesk-core.exe`** (`flashdesk.exe`) | `crates/desk-app` | 业务全功能主体 + 更新客户端：负责中转 `UpdateChannel` 查包/断点下载/差量合成/验签、`--agent` 的 24 小时定时与 IPC/远程触发响应、`--wait-pid` 无互斥竞态重启、无服务模式按需提权切槽 `--apply-staged-update`、GUI 顶栏更新提示与设备列表远程触发；托盘退出时自动停止 `FlashDeskService`（通过 `IU` 启停权限免重复 UAC，更新重启时不误停）、下次启动 GUI 自动静默恢复已停止的服务 |
| **`flashdesk-relay`** | `crates/relay-server` | 在原有隧道转发基础上新增 `updates.rs`：处理 `ConnectionPurpose::UpdateChannel`（`GetManifest`、`DownloadChunk`、`BeginPublish`、`UploadChunk`、`CommitPublish`）与 `PresenceClientMsg::RequestPeerUpdateCheck` 实时转发 |
| **`flashdesk-release.exe`** | `crates/release-tool` | 发版 CLI：`pack`（生成压缩全量 + 对历史版本算差量 + 根私钥签名）、`publish`（打包并直推到一台或多台中转服）、`build-setup`（将四大二进制载荷压缩并追加到 `flashdesk-setup-stub.exe` 生成单文件 `FlashDesk-Setup.exe`） |
| **`FlashDesk-Setup.exe`** | `crates/installer` | 纯 Rust `eframe (glow)` 图形化安装程序（支持 `--silent` 自动化部署）：内置多分辨率 PE 图标与窗口图标，从自身 PE 尾部 `FLDSKSTP` 载荷解包校验 SHA-256，创建目录结构、迁移旧便携版服务、注册 `FlashDeskService` 并授交互用户启停权、配置 `ipc\` 权限、创建带图标快捷方式（`SetIconLocation` + `SHChangeNotify`）与卸载注册表项 |

---

## 四、协议、补丁格式与安全模型

### 1. 线协议扩展（严格末尾追加原则）
依据 `AGENTS.md` 第 9 条，`bincode` 按枚举变体序号编码，因此以下新增变体**严格追加在枚举末尾**，并受单元测试索引断言保护：
- `ConnectionPurpose`（`crates/protocol/src/relay.rs`）：末尾追加 `UpdateChannel`（索引 `4`）。
- `PresenceClientMsg`：末尾追加 `RequestPeerUpdateCheck { target_device_id: Uuid }`（索引 `5`）。
- `PresenceServerMsg`：末尾追加 `CheckUpdateNow { from_device_id: Uuid, from_device_name: String }`（索引 `5`）。

### 2. 二进制差量格式 `FLDP` v1（`crates/protocol/src/patch.rs`）
- **文件头（53 字节明文定长）**：
  - `magic`: `b"FLDP"`（4 字节）
  - `version`: `1u8`（1 字节）
  - `old_len`: `u64`（大端）
  - `new_len`: `u64`（大端）
  - `new_sha256`: `[u8; 32]`
- **正文（`flate2` `rust_backend` Deflate 压缩流）**：
  - `0u8` (`INSERT`)：`len: u32 (BE)` + `raw[len]`
  - `1u8` (`COPY_DIFF`)：`old_offset: u64 (BE)` + `len: u32 (BE)` + `residual[len]`（逐字节 `new[i].wrapping_sub(old[old_offset + i])`，可跨越插入偏移与局部重定位指令，大幅压低差量残差熵）
- **防爆与防篡改**：解压阶段强制检查 `max_out`（128 MiB），还原全部字节后强制比对 `sha256(&new) == new_sha256`。

### 3. 更新清单与推送校验（`crates/protocol/src/update.rs` & `crates/relay-server/src/updates.rs`）
- **清单签名**：`SignedUpdateManifest` 使用个人根私钥对 `MANIFEST_SIGN_DOMAIN = b"FLASHDESK-UPDATE-MANIFEST-V1"` + 清单所有字段的大端规范序列化字节进行 Ed25519 签名。
- **服务端零信任防线**：
  - 客户端上传 `BeginPublish` 时，中转服不仅要求连接本身通过 `DeviceCert` 认证，还必须使用配置的 `root_pubkey` 校验 `SignedUpdateManifest` 签名及时间戳（禁止 `released_at` 倒流回滚攻击）。
  - 校验文件名白名单（仅 `[A-Za-z0-9._-]`、禁止 `..`）、单文件体积上限（64 MiB）与批次上限（256 MiB）。
  - 所有块落入临时目录 `updates/.incoming-<uuid>/`，`CommitPublish` 核对全部文件精确长度与 SHA-256 无误后，再原子替换 `updates/manifest.json` 与制品文件，并自动清理不再被引用的旧版本残留文件。

---

## 五、端到端更新时序与回滚状态机

```text
[触发检查]
  ├─ 手动：GUI 点「检查更新」 → 有健康系统服务时写 ProgramData\ipc\request.json；无服务时 GUI 自己查
  ├─ 定时：--agent 本地每 60s 核对 state.json.last_check_at，满 24h 发起一次网络查询
  └─ 远程：主控端在设备列表点「更新」 → 中转转发 CheckUpdateNow → 目标端立即检查
        │
        ▼
[下载与验签]
  ├─ 验签 SignedUpdateManifest (root_pubkey) + semver 比较 (> CORE_VERSION) + 黑名单过滤
  ├─ 若 manifest.min_base_version > state.base_version → 写 NeedsBaseUpgrade，提示运行新版 Setup.exe
  ├─ 优先查 deltas 匹配当前 core SHA-256 → 下载 .fldp → 本地合成 + 校验 target_sha256
  └─ 未命中或失败 → 自动回退下载 .deflate 全量包 → 解压 + 校验 target_sha256
        │
        ▼
[切槽与看门狗]
  ├─ 服务模式：写入 updates\staging\<ver>\flashdesk-core.exe 与 updates\switch_request.json
  │    └─ flashdesk-svc.exe 确认 agent.status.json.active_sessions == 0 持续满 10s
  │         └─ 原子移入 versions\<ver>\flashdesk-core.exe → 更新 state.json.candidate → 重启 --agent
  │              ├─ 15s 内存活且心跳 version == candidate → 转正为 active，清理更早旧槽，写 SwitchedRestartGui
  │              └─ 15s 内异常退出（连续 2 次） → 自动回切 previous_good，拉黑坏版本并写 RolledBack
  └─ 纯 GUI 模式：直接切槽（安装目录不可写时弹一次 UAC 调 --apply-staged-update），顶栏提示「立即重启更新」
       └─ 点击后带 --wait-pid <self_pid> 启动 FlashDesk.exe 避免单实例互斥锁竞态，优雅退出旧进程
```

---

## 六、安装器封装格式（`FLDSKSTP` 自解压载荷）

为避免引入 Inno Setup / WiX 等外部打包器，`FlashDesk-Setup.exe` 采用纯粹、稳定的 PE 尾部追加格式（Windows PE 加载器仅按节表映射代码/数据，尾部附加数据不影响签名与加载）：
- 格式：`[flashdesk-setup-stub.exe] + [deflate(SetupPayloadHeader + 4 个核心二进制)] + [24 字节定长尾标]`
- 24 字节尾标结构：
  - `magic`: `b"FLDSKSTP"`（8 字节）
  - `payload_offset`: `u64`（大端，压缩载荷起始偏移）
  - `payload_len`: `u64`（大端，压缩载荷字节数）
- 载荷内 `SetupPayloadManifest` 记录版本号与四个二进制（`flashdesk-svc.exe`、`flashdesk-launcher.exe`、`flashdesk-uninstall.exe`、`flashdesk-core.exe`）的长度和 SHA-256，安装器解包时逐一校验完整性。

---

## 七、日常发版与部署操作指南

### 1. 编译全部产物（Windows + Linux 静态中转服）
```powershell
$env:RUSTUP_HOME = "D:\Rust\rustup"; $env:CARGO_HOME = "D:\Rust\cargo"
$env:PATH = "D:\Rust\cargo\bin;D:\Rust\mingw64\bin;$env:PATH"
cargo test --workspace
cargo build --release --workspace
cargo build --release -p relay-server --target x86_64-unknown-linux-musl
```

### 2. 生成新版本单文件安装包 `FlashDesk-Setup.exe`
```powershell
D:\Rust\target-flashdesk\release\flashdesk-release.exe build-setup `
  --stub D:\Rust\target-flashdesk\release\flashdesk-setup-stub.exe `
  --svc D:\Rust\target-flashdesk\release\flashdesk-svc.exe `
  --launcher D:\Rust\target-flashdesk\release\flashdesk-launcher.exe `
  --uninstall D:\Rust\target-flashdesk\release\flashdesk-uninstall.exe `
  --core D:\Rust\target-flashdesk\release\flashdesk.exe `
  --version 0.1.1 `
  --out target\release\FlashDesk-Setup.exe
```

### 3. 一键打差量补丁、根私钥签名并推送到云服务器
当发布新版本（例如 `0.1.1`）时，确保已将历史版本的 `flashdesk-core.exe` 保存在归档目录（`publish` 会自动归档）：
```powershell
D:\Rust\target-flashdesk\release\flashdesk-release.exe publish `
  --core D:\Rust\target-flashdesk\release\flashdesk.exe `
  --version 0.1.1 `
  --min-base 1.0.0 `
  --notes "新增：仅已吊销设备支持从列表删除" `
  --history-dir .\releases\history `
  --out-dir .\releases\0.1.1
```
- 命令会自动读取 `%USERPROFILE%\.flashdesk\config.json` 与 `root.key`，计算相对于 `.\releases\history\` 中最近 5 个版本的 `FLDP` 差量补丁，生成压缩全量包并签名后直推到你配置的中转服务器。
- 若某些节点未运行在最新 `flashdesk-relay` 上，可先按 [deploy/README.md](../deploy/README.md) 升级一次 Linux 中转二进制 `deploy\flashdesk-relay-linux-x86_64`（需确保配置中带有 `--state-dir /var/lib/flashdesk`，[deploy/deploy-relay.sh](../deploy/deploy-relay.sh) 已默认开启）。
