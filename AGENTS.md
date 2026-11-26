# AGENTS.md — FlashDesk 开发者与 AI 接手必读指南

本仓库为自建不限速 60fps Windows 远程桌面 **FlashDesk**（Rust workspace：`protocol` + `relay-server` + `base-common` + `base-runtime` + `desk-app` + `release-tool` + `installer`）。
**修改任何核心代码前，请先通读完整设计及避坑记录：[ARCHITECTURE_AND_PITFALLS.md](docs/ARCHITECTURE_AND_PITFALLS.md) 与 [AUTO_UPDATE_AND_INSTALLER.md](docs/AUTO_UPDATE_AND_INSTALLER.md)。**

---

## 1. 硬性环境与工具链约束
- **严禁写入 `~/.rustup` 或 `~/.cargo`**，所有工具链与产物缓存都在 `D:\Rust`。每次执行 `cargo`/`rustup` 必须带以下环境头：
  ```powershell
  $env:RUSTUP_HOME = "D:\Rust\rustup"
  $env:CARGO_HOME = "D:\Rust\cargo"
  $env:PATH = "D:\Rust\cargo\bin;D:\Rust\mingw64\bin;$env:PATH"
  ```
- **不要改成 MSVC**（沙箱无提权），也**不要把 `target-dir` 移回中文路径**（MinGW `dlltool`/`objdump` 不兼容中文路径，[.cargo/config.toml](.cargo/config.toml) 已指向 `D:\Rust\target-flashdesk`）。
- 机器没有 `D:` 数据盘时：同一套工具链装到 `C:\Rust` 之类的纯 ASCII 非用户目录，**不改配置文件**，用 `CARGO_TARGET_DIR` / `CARGO_TARGET_<triple>_LINKER` / `FLASHDESK_MINGW_DIR` 环境变量覆盖（完整命令见架构文档「三、1. 工具链存放红线」）。

---

## 2. 十一条不可打破的架构红线（踩坑血泪总结）
1. **禁止在 GUI 主线程初始化 COM MTA / Media Foundation**：`winit` 依赖 COM STA (`OleInitialize`)，冲突必 panic `RPC_E_CHANGED_MODE`。保留 [crates/desk-app/src/codec/mod.rs](crates/desk-app/src/codec/mod.rs) 中独立线程 `flashdesk-codec-probe` 的隔离机制；安装器创建快捷方式（`IShellLinkW`）同样必须隔离到短生命周期 STA 线程。
2. **禁止切换回 `eframe` 的 `wgpu` Vulkan 后端**：双显卡 / Intel 核显 Vulkan (`Unrecognized present mode 1000361000`) 会导致整窗黑白色块撕裂与断开后剧烈闪烁。必须保留 [crates/desk-app/Cargo.toml](crates/desk-app/Cargo.toml) 与 [crates/installer/Cargo.toml](crates/installer/Cargo.toml) 中的 `features = ["default_fonts", "glow"]`。
3. **保持单文件零外部 DLL**：保留 [.cargo/config.toml](.cargo/config.toml) `CXXSTDLIB = ""` 与 `build.rs` 静态组链接，每次改动后必须用 `objdump -p` 对**所有**发布 EXE（含基座、安装器、发版 CLI）确认零 `libstdc++-6.dll` / `libwinpthread-1.dll` / `libgcc_s_seh-1.dll`。
4. **保留三层抓屏降级与异步编解码保护**：
   - `capture/dxgi.rs`：`IDXGIOutput5::DuplicateOutput1` → `IDXGIOutput1::DuplicateOutput` → `GdiSingleCapturer`（`BitBlt` + 光标）。
   - `codec/mf_common.rs`：异步 MFT 只能走带超时的 `poll_event_until`，禁止无超时 `GetEvent(Default::default())`；`probe_capabilities` 必须用 640×360 真实帧探活出流。
   - 沙箱 `run_command` 桌面无画面权限（`0x80070005` / `0x80070006`）属于正常安全边界，`--self-test` 已自动退回 1080p 合成帧，严禁试图绕出沙箱桌面。
5. **中转服务端路由与超时顺序不可倒退**：[crates/relay-server/src/state.rs](crates/relay-server/src/state.rs) 必须用 `.rev().find(|e| e.accept_incoming)` 优先投递给最新活跃连接；修改中转端后需重新交叉编译并同步更新 `deploy\flashdesk-relay-linux-x86_64` 与提示用户升级服务端。
6. **`deploy/deploy-relay.sh` 必须严格保持 LF（0 个 `\r` 字节）**，修改后用 `D:\Git\bin\bash.exe -n` 校验。
7. **严禁将 `read_frame` / `NoiseReader::recv_msg` 直接放进 `tokio::select!`**：两者均非 cancel-safe，竞争就绪会丢弃半帧 TCP 字节并破坏 Noise nonce（导致键鼠失灵与断线假死）；必须保留独立 reader task + 取消安全 `mpsc::channel` 隔离设计。
8. **视频背压只能发生在抓屏/编码之前，禁止“编完再丢、丢了补关键帧”**（坑 #16）：被控端先 `video_tx.try_reserve()` + `FlowController::can_send()` 再抓屏编码；主控端解码队列禁止丢帧（`send().await`），积压时批量解码、只转最新一帧；关键帧请求两端都限频 ≥ 1s。否则 5M 中转上必然出现关键帧风暴（2K IDR 100–300 KB）。
9. **帧确认 `FrameAck` 必须走视频连接的反向**（不走控制通道，否则会排在剪贴板大图/键鼠后面把流控闸门卡死）；`ControlMsg` / 协议枚举的新变体**只能追加到末尾**（bincode 按变体序号编码，`protocol` 有 `control_msg_variant_indices_are_stable` 单测守护）。
10. **中转端交互通道（Control/Video）保持小缓冲**：`copy_bidirectional` 32 KB + Linux `TCP_NOTSENT_LOWAT` 16 KB；只有 File / Update 通道用 512 KB。改动抓屏/编解码/传输/流控后必须跑 `flashdesk.exe --link-bench`（见第 3 节）。
11. **严格保持轻量基座隔离（`base-common` / `base-runtime`）**：基座 crate 严禁引入 `tokio`、`snow`、`ed25519-dalek`、`eframe`、`openh264` 或 `protocol`，保证基座体积极简且几乎永不改动；一切联网更新、差量合成与验签都在核心版本槽 `flashdesk-core.exe` 中完成；安装器绝不默认安装虚拟显示器驱动（VDD）。

---

## 3. 修改后的标准编译、打包与验收命令

> 完整测试步骤（含 `--link-bench` 输出解读与真机联调清单）见 [docs/TESTING.md](docs/TESTING.md)，发版与补丁推送见 [docs/AUTO_UPDATE_AND_INSTALLER.md](docs/AUTO_UPDATE_AND_INSTALLER.md)。**项目目录与 Rust 工具链/编译输出是分开的**：工具链在 `D:\Rust`，编译产物在 `D:\Rust\target-flashdesk`（不在项目里），项目的 `target\release\` 只是复制过去的发布副本；写命令时用明确路径，不要假设产物或工具链在项目目录内。

```powershell
# 1. 全量测试 + Windows Release + Linux 静态 Relay 交叉编译
$env:RUSTUP_HOME = "D:\Rust\rustup"; $env:CARGO_HOME = "D:\Rust\cargo"
$env:PATH = "D:\Rust\cargo\bin;D:\Rust\mingw64\bin;$env:PATH"
cargo test --workspace
cargo build --release --workspace
cargo build --release -p relay-server --target x86_64-unknown-linux-musl

# 2. 构建独立图形化安装包 FlashDesk-Setup.exe 并同步发布产物（先确保未运行旧 target\release\flashdesk.exe）
D:\Rust\target-flashdesk\release\flashdesk-release.exe build-setup `
  --stub D:\Rust\target-flashdesk\release\flashdesk-setup-stub.exe `
  --svc D:\Rust\target-flashdesk\release\flashdesk-svc.exe `
  --launcher D:\Rust\target-flashdesk\release\flashdesk-launcher.exe `
  --uninstall D:\Rust\target-flashdesk\release\flashdesk-uninstall.exe `
  --core D:\Rust\target-flashdesk\release\flashdesk.exe `
  --version 0.1.1 `
  --out target\release\FlashDesk-Setup.exe
Copy-Item "D:\Rust\target-flashdesk\release\flashdesk.exe" "target\release\flashdesk.exe" -Force
Copy-Item "D:\Rust\target-flashdesk\release\flashdesk-relay.exe" "target\release\flashdesk-relay.exe" -Force
Copy-Item "D:\Rust\target-flashdesk\release\flashdesk-release.exe" "target\release\flashdesk-release.exe" -Force
Copy-Item "D:\Rust\target-flashdesk\x86_64-unknown-linux-musl\release\flashdesk-relay" "deploy\flashdesk-relay-linux-x86_64" -Force
Copy-Item "D:\Rust\target-flashdesk\x86_64-unknown-linux-musl\release\flashdesk-relay" "target\release\flashdesk-relay-linux-x86_64" -Force

# 3. PE、.rsrc 图标节与 zero-DLL 审计（注意先进入纯 ASCII 相对路径目录再调 objdump）
Push-Location D:\Rust\target-flashdesk\release
foreach ($b in @("flashdesk.exe","flashdesk-svc.exe","flashdesk-launcher.exe","flashdesk-uninstall.exe","flashdesk-setup-stub.exe","flashdesk-release.exe","flashdesk-relay.exe")) {
    D:\Rust\mingw64\bin\objdump.exe -p ".\$b" | Select-String "Subsystem|DLL Name"
    D:\Rust\mingw64\bin\objdump.exe -h ".\$b" | Select-String "\.rsrc"
}
Pop-Location
Push-Location target\release
D:\Rust\mingw64\bin\objdump.exe -p .\FlashDesk-Setup.exe | Select-String "Subsystem|DLL Name"
D:\Rust\mingw64\bin\objdump.exe -h .\FlashDesk-Setup.exe | Select-String "\.rsrc"
Pop-Location

> 注：各 crate 的 `build.rs` 编译 PE 图标时，务必先把 `assets/flashdesk.ico` 复制到纯 ASCII 的 `OUT_DIR` 再调 `windres.exe`（规避 Pitfall #2 中文路径问题），并通过 `cargo:rustc-link-arg-bins` 链接资源对象。

# 4. 无 MinGW 洁净环境验证 GUI 启动与 --self-test
$env:PATH = ($env:PATH -split ';' | Where-Object { $_ -and ($_ -notmatch 'mingw') -and ($_ -notmatch 'Rust\\cargo') }) -join ';'
Start-Process -FilePath .\target\release\flashdesk.exe -ArgumentList "--self-test" -Wait -PassThru

# 5. 5M 中转端到端基准（真实编解码 + 流控，合成桌面 + 限速代理；退出码 0 = PASS）
.\target\release\flashdesk.exe --link-bench | Out-String
```
