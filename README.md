# ⚡ FlashDesk

自建中转 · 不限速 · 60fps 的 Windows 远程桌面（Rust 全栈单文件客户端 + 零信任自建中转）。
目标是在无第三方限速、无账号体系的前提下替代向日葵 / ToDesk，适合自用及多台常用电脑之间长期互连。

> 本仓库为个人自用项目。架构定稿、工具链硬约束与全部真机踩坑记录见
> [docs/ARCHITECTURE_AND_PITFALLS.md](docs/ARCHITECTURE_AND_PITFALLS.md)（Pitfalls #1–#15），
> AI / 新人接手必读 [AGENTS.md](AGENTS.md)。

---

## 特性

- **单文件双形态与系统托盘单例**：双击即绿色便携 GUI（关闭 `X` 退到任务栏托盘，托盘右键真正退出；GUI 单例防多开，重复打开直接唤醒已有窗口）。一键 UAC 注册为 `SYSTEM` 后台服务（Session 0 监督 + 交互会话 agent），
  支持**锁屏（Winlogon）/ UAC secure desktop 穿透**与真实 `Ctrl+Alt+Del`（`sas.dll SendSAS`）。
- **服务与 GUI 共存、按健康度自动接管**：agent 心跳（`agent.status.json`）+ 服务配置一致性校验决定让权；
  agent 死亡或服务配置未同步时桌面 GUI 自动接管中继被控，恢复/同步后自动让回（Pitfall #15）。
- **多节点自建中转 + 局域网直连**：支持多台自建低延迟/大带宽节点分流，默认端口 `9442`（`PairTunnel` 跨节点文件对接）；
  TCP `19444` IP 直连绕过中转，同享端到端加密。
- **零信任安全模型与极简绑定**：密钥固定存放在 `%USERPROFILE%\.flashdesk\`（`root.key` 与 `config.json` 分离，换用新版 exe 不丢失）；
  主电脑一键生成紧凑短码（`fdesk://v2/...`）、二维码（供未来手机端扫码）及 `.fdesk` 文件（支持直接拖拽进窗口绑定）；
  主电脑内置已签发设备台账与根签名**远程吊销**，点击吊销即实时踢除并持久化至 `/var/lib/flashdesk/revoked.json`。
- **画面链路**：DXGI `DuplicateOutput1 → DuplicateOutput → GDI BitBlt` 三层抓屏降级；
  MF 硬件 HEVC / H.264 低延迟 CBR + OpenH264 软编回退（运行时热切换）；异步 MFT 全超时轮询杜绝死等。
- **60fps 体验**：首帧预热 + 1s 保活刷新、静止桌面不黑屏、切屏/分辨率变化自动重建编码器。
- **小水管不卡**：帧确认驱动的延迟闸门 + 自适应码率（5M 中转上画面延迟稳定在 ~100 ms 量级，不再堆积到秒级）；
  编码前背压、关键帧限频、变化后冲刷编解码器憋帧；抓屏/编码流水线 + 精确节拍；可选 1080p/720p 分辨率上限；
  `flashdesk.exe --link-bench` 端到端基准（合成桌面 + 限速中转代理）。
- **文件传输**：256 KiB 分块、`.fdesk.part/.fdesk.meta` 断点续传、SHA-256 校验、双栏拖拽上传。
- **双向剪贴板**（文本 + ≤8 MiB 图片，序列号防回环）、**虚拟显示器**（Parsec VDD / OrayIddDriver 检测与增删）。
- **eframe + glow(OpenGL)** 渲染：兼容独显 / 核显 / 虚拟机，无 Vulkan 花屏问题；单文件零外部 MinGW DLL。

## 工作区结构

```text
crates/protocol      共享线协议、Ed25519 身份、Noise 加密流、会话/文件消息
crates/relay-server  自建纯 TCP 中转（flashdesk-relay）：验签、Presence 路由、隧道配对
crates/desk-app      Windows 桌面端 + 被控系统服务（flashdesk.exe，GUI/服务/agent 三形态）
deploy/              Ubuntu 一键部署脚本 + 静态 Linux x86_64 中转二进制
docs/                架构设计、开发约束与踩坑交接文档
```

## 快速开始

### 1. 编译（Windows GNU 工具链，硬约束见 AGENTS.md）

```powershell
$env:RUSTUP_HOME = "D:\Rust\rustup"; $env:CARGO_HOME = "D:\Rust\cargo"
$env:PATH = "D:\Rust\cargo\bin;D:\Rust\mingw64\bin;$env:PATH"
cargo test --workspace
cargo build --release --workspace
cargo build --release -p relay-server --target x86_64-unknown-linux-musl
```

> 工具链与产物缓存固定于 `D:\Rust`（`.cargo/config.toml` 已映射 target-dir）；
> 严禁写入 `~/.rustup` / `~/.cargo`，严禁切 MSVC，严禁把 target-dir 移回中文路径。

### 2. 运行

- 主电脑双击 `flashdesk.exe` → 填入中转地址并点击「初始化主电脑（生成根密钥）」，密钥固定保存到 `~/.flashdesk/root.key` 与 `config.json`；
- 在「设置 → 身份与设备绑定」复制一键部署命令在服务器安装中转，然后点击「生成绑定二维码与密钥」；
- 其它电脑直接把 `.fdesk` 拖入窗口（或粘贴 `fdesk://v2/...` 短码）即可绑定；
- 被控端点主页「运行模式」右侧的「安装系统服务」获得锁屏 / UAC 能力；「运行模式」如实显示服务健康与配置同步状态。

### 3. 部署中转服务器

见 [deploy/README.md](deploy/README.md)：上传 `deploy-relay.sh` + `flashdesk-relay-linux-x86_64`，
`sudo bash deploy-relay.sh --port 9442 --root-pubkey "<你的根公钥>"` 一键完成（BBR、systemd、防火墙）。

## 安全与隐私

- 仓库**不包含任何密钥与个人配置**：`*.fdesk`、`config.json`、`flashdesk.config.json`、`relay.toml`、
  `*.key`、`*.log` 与 `/target` 均已 gitignore；设备私钥仅存在于本机配置与 ProgramData 共享配置中。
- 中转服务器零信任：仅持根公钥与吊销列表，无法解密控制 / 画面 / 文件流量。
- 丢失设备可在主电脑设置页「已签发设备与远程吊销」中一键远程吊销（实时踢除并持久化至 `/var/lib/flashdesk/revoked.json`），或写入 `relay.toml`。

## 验证状态

2026-11-23 真机（域管办公机，双 2K 屏 + NVIDIA 硬编）：16/16 单元与集成测试、PE 零 MinGW DLL 审计、
洁净环境 `--self-test` 全链路通过；服务 agent 以 SYSTEM 拉起并持有直连端口，主控端反复连接 / 锁屏场景初测联通。

2026-11-23 低延迟传输改造（Pitfall #16，4 vCPU 虚拟机 + OpenH264 软编）：38/38 测试、PE 零 MinGW DLL、
洁净环境 `--self-test` 通过；`--link-bench` 模拟国内 5M 中转：1080p 滚动 56–60 fps、画面延迟 p95 ≈ 60 ms，
2K + 1080p 上限 59.7 fps；拥塞对照中画面延迟从关闭帧确认时的 ~2 s 降到 ~0.2 s。真机硬编环境尚待实测。

## 文档

- [AGENTS.md](AGENTS.md) — 接手速查红线（工具链、十条架构红线、验收命令）
- [docs/ARCHITECTURE_AND_PITFALLS.md](docs/ARCHITECTURE_AND_PITFALLS.md) — 完整架构与 Pitfalls #1–#16
- [docs/TESTING.md](docs/TESTING.md) — 测试指南：项目目录与 `D:\Rust` 工具链分开的路径约定、自动化测试、打包审计、`--self-test`、`--link-bench` 基准解读、真机联调清单
- [deploy/README.md](deploy/README.md) — Ubuntu 中转部署与运维
