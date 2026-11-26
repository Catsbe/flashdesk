# ⚡ FlashDesk

自建中转 · 不限速 · 60fps 的 Windows 远程桌面（Rust 全栈单文件客户端 + 零信任自建中转）。
目标是在无第三方限速、无账号体系的前提下替代向日葵 / ToDesk，适合自用及多台常用电脑之间长期互连。

> 本仓库为个人自用项目。架构定稿、工具链硬约束与全部真机踩坑记录见
> [docs/ARCHITECTURE_AND_PITFALLS.md](docs/ARCHITECTURE_AND_PITFALLS.md)（Pitfalls #1–#19），
> 安装包与热更新体系见 [docs/AUTO_UPDATE_AND_INSTALLER.md](docs/AUTO_UPDATE_AND_INSTALLER.md)，
> AI / 新人接手必读 [AGENTS.md](AGENTS.md)。

---

## 特性

- **图形化安装器（`FlashDesk-Setup.exe`）与稳定基座热更新**：纯 Rust 自解压单文件安装器（绝不默认安装 VDD），拆分轻量零网络依赖基座（`FlashDesk.exe` 启动器 + `bin\flashdesk-svc.exe` + `bin\flashdesk-uninstall.exe`）与可热替换版本槽 `versions\<ver>\flashdesk-core.exe`；复用中转端口的 `UpdateChannel` 获取根私钥验签的 `FLDP` 二进制差量补丁（未命中自动回退全量压缩包），配合 15 秒看门狗自动回滚。
- **单文件双形态、托盘单例与服务启停协同**：也支持脱离安装器绿色单文件运行；关闭窗口 `X` 隐藏到托盘，GUI 单例防多开；从托盘右键「退出 FlashDesk」时自动停止 `FlashDeskService` 及其 Session 1 `--agent`（通过 `IU` 交互用户启停权限免重复 UAC，下次启动 GUI 自动静默恢复服务；更新重启 GUI 时不误停服务）。
- **SYSTEM 后台服务与健康接管**：支持注册 Windows `SYSTEM` 服务（Session 0 守护 + 交互会话 `--agent`），支持**锁屏（Winlogon）/ UAC secure desktop 穿透**与真实 `Ctrl+Alt+Del`（`sas.dll SendSAS`）；agent 心跳（`agent.status.json`）+ 配置一致性校验决定让权，异常时桌面 GUI 自动接管被控（Pitfall #15）。
- **多节点自建中转 + 局域网直连**：支持多台自建低延迟/大带宽节点分流，默认端口 `9442`（`PairTunnel` 跨节点文件对接）；
  TCP `19444` IP 直连绕过中转，同享端到端加密。
- **零信任安全模型与极简绑定**：密钥固定存放在 `%USERPROFILE%\.flashdesk\`（`root.key` 与 `config.json` 分离，换用新版 exe 不丢失）；
  主电脑一键生成紧凑短码（`fdesk://v2/...`）、二维码（供未来手机端扫码）及 `.fdesk` 文件（支持直接拖拽进窗口绑定）；
  主电脑内置已签发设备台账与根签名**远程吊销**，点击吊销即实时踢除并持久化至 `/var/lib/flashdesk/revoked.json`，且支持**仅对已吊销设备从列表删除**（底层保留 `revoked_device_ids` 黑名单持续拦截旧凭证）。
- **画面链路**：DXGI `DuplicateOutput1 → DuplicateOutput → GDI BitBlt` 三层抓屏降级；
  MF 硬件 HEVC / H.264 低延迟 CBR + OpenH264 软编回退（运行时热切换）；异步 MFT 全超时轮询杜绝死等。
- **60fps 体验**：首帧预热 + 1s 保活刷新、静止桌面不黑屏、切屏/分辨率变化自动重建编码器。
- **小水管不卡**：帧确认驱动的延迟闸门 + 自适应码率（5M 中转上画面延迟稳定在 ~100 ms 量级，不再堆积到秒级）；
  编码前背压、关键帧限频、变化后冲刷编解码器憋帧；抓屏/编码流水线 + 精确节拍；可选 1080p/720p 分辨率上限；
  `flashdesk.exe --link-bench` 端到端基准（合成桌面 + 限速中转代理）。
- **文件传输**：256 KiB 分块、`.fdesk.part/.fdesk.meta` 断点续传、SHA-256 校验、双栏拖拽上传。
- **双向剪贴板**（文本 + ≤8 MiB 图片，序列号防回环）、**虚拟显示器**（Parsec VDD / OrayIddDriver 检测与增删）。
- **原生外观与零依赖**：`eframe + glow(OpenGL)` 渲染兼容独显 / 核显 / 虚拟机；通过 MinGW `windres.exe` 为全部 Windows GUI/服务 EXE 及桌面/开始菜单快捷方式嵌入多分辨率 [assets/flashdesk.ico](assets/flashdesk.ico)（16–256 px）；全量静态链接零外部 MinGW DLL。

## 工作区结构

```text
assets/flashdesk.ico  多分辨率 Windows 图标资源（16–256 px）
crates/protocol       共享线协议、Ed25519 身份、Noise 加密流、FLDP 差量补丁与更新清单验签
crates/relay-server   自建纯 TCP 中转（flashdesk-relay）：验签、Presence 路由、隧道配对、UpdateChannel 分发
crates/base-common    轻量基座契约：state.json、看门狗状态机、服务交互启停 DACL、图标 RGBA 生成（零网络/UI依赖）
crates/base-runtime   稳定基座程序集（FlashDesk.exe 启动器、flashdesk-svc.exe、flashdesk-uninstall.exe）
crates/desk-app       Windows 桌面端 + 被控核心（flashdesk.exe / flashdesk-core.exe）
crates/installer      纯 Rust 图形化自解压安装程序（FlashDesk-Setup.exe）
crates/release-tool   发版工具 CLI（flashdesk-release.exe：pack / publish / build-setup）
deploy/               Ubuntu 一键部署脚本 + 静态 Linux x86_64 中转二进制说明
docs/                 架构设计、自动更新与安装器、测试指南与踩坑交接文档
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

### 2. 运行与安装

- **安装版（推荐）**：运行 `FlashDesk-Setup.exe` 选择安装目录（自动部署轻量基座、系统服务与快捷方式，支持后续免 UAC 差量更新）；
- **便携模式**：也可直接双击 `flashdesk.exe` 绿色运行；
- 主电脑填入中转地址并点击「初始化主电脑（生成根密钥）」，密钥固定保存到 `~/.flashdesk/root.key` 与 `config.json`；
- 在「设置 → 身份与设备绑定」复制一键部署命令在服务器安装中转，然后点击「生成绑定二维码与密钥」；
- 其它电脑直接把 `.fdesk` 拖入窗口（或粘贴 `fdesk://v2/...` 短码）即可绑定；
- 后续版本升级可使用 `flashdesk-release.exe publish` 一键打包算差量、根签名并推送到中转节点。

### 3. 部署中转服务器

见 [deploy/README.md](deploy/README.md)：上传 `deploy-relay.sh` + `flashdesk-relay-linux-x86_64`，
`sudo bash deploy-relay.sh --port 9442 --root-pubkey "<你的根公钥>"` 一键完成（BBR、systemd、防火墙与 `/var/lib/flashdesk` 状态目录）。

## 安全与隐私

- 仓库**不包含任何密钥与个人配置**：`*.fdesk`、`config.json`、`flashdesk.config.json`、`relay.toml`、
  `*.key`、`*.log`、`/releases/` 与 `/target` 均已 gitignore；设备私钥仅存在于本机配置与 ProgramData 共享配置中。
- 中转服务器零信任：仅持根公钥与吊销列表，无法解密控制 / 画面 / 文件流量；更新清单强校验 Ed25519 签名与发布时间戳。
- 丢失设备可在主电脑设置页「已签发设备与远程吊销」中一键远程吊销（实时踢除并持久化至 `/var/lib/flashdesk/revoked.json`），并可从列表删除已吊销记录而持续拦截旧凭证。

## 验证状态

- **2026-11-26（`v0.1.1` / 基座 `v1.0.0`）**：`cargo test --workspace` **69/69** 全通过（`base-common` 7 + `desk-app` 39 + `protocol` 14 + `relay_e2e` 7 + `release-tool` 2）；`FlashDesk-Setup.exe`（10.06 MiB）及全部 Windows EXE 通过 `objdump -p/-h` 零 MinGW DLL 与 `.rsrc` 图标段审计；洁净 PATH `--self-test` 全通过；实机完成 `v0.1.0 → v0.1.1` 在线差量/切槽更新、托盘退出自动停服务与已吊销设备删除验收。
- **2026-11-23（Pitfalls #15/#16）**：域管办公机（双 2K + NVIDIA 硬编）验证 SYSTEM agent / 锁屏 UAC 穿透与服务自动让权；4 vCPU 虚拟机 `--link-bench` 模拟国内 5M 中转：1080p60 滚动 56–60 fps（p95 ≈ 61 ms），拥塞对照画面延迟由 ~2 s 降至 ~0.2 s。

## 文档

- [AGENTS.md](AGENTS.md) — 接手速查红线（工具链、十一条架构红线、验收命令）
- [docs/ARCHITECTURE_AND_PITFALLS.md](docs/ARCHITECTURE_AND_PITFALLS.md) — 完整架构与 Pitfalls #1–#19
- [docs/AUTO_UPDATE_AND_INSTALLER.md](docs/AUTO_UPDATE_AND_INSTALLER.md) — 安装包、轻量稳定基座与二进制差量自动更新设计
- [docs/TESTING.md](docs/TESTING.md) — 测试指南：项目目录与 `D:\Rust` 工具链分开的路径约定、自动化测试、打包审计、`--self-test`、`--link-bench` 基准解读、真机联调清单
- [deploy/README.md](deploy/README.md) — Ubuntu 中转部署与运维
