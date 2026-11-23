# AGENTS.md — FlashDesk 开发者与 AI 接手必读指南

本仓库为自建不限速 60fps Windows 远程桌面 **FlashDesk**（Rust workspace：`protocol` + `relay-server` + `desk-app`）。
**修改任何核心代码前，请先通读完整设计及避坑记录：[ARCHITECTURE_AND_PITFALLS.md](docs/ARCHITECTURE_AND_PITFALLS.md)。**

---

## 1. 硬性环境与工具链约束
- **严禁写入 `~/.rustup` 或 `~/.cargo`**，所有工具链与产物缓存都在 `D:\Rust`。每次执行 `cargo`/`rustup` 必须带以下环境头：
  ```powershell
  $env:RUSTUP_HOME = "D:\Rust\rustup"
  $env:CARGO_HOME = "D:\Rust\cargo"
  $env:PATH = "D:\Rust\cargo\bin;D:\Rust\mingw64\bin;$env:PATH"
  ```
- **不要改成 MSVC**（沙箱无提权），也**不要把 `target-dir` 移回中文路径**（MinGW `dlltool`/`objdump` 不兼容中文路径，[.cargo/config.toml](.cargo/config.toml) 已指向 `D:\Rust\target-flashdesk`）。

---

## 2. 七条不可打破的架构红线（踩坑血泪总结）
1. **禁止在 GUI 主线程初始化 COM MTA / Media Foundation**：`winit` 依赖 COM STA (`OleInitialize`)，冲突必 panic `RPC_E_CHANGED_MODE`。保留 [crates/desk-app/src/codec/mod.rs](crates/desk-app/src/codec/mod.rs) 中独立线程 `flashdesk-codec-probe` 的隔离机制。
2. **禁止切换回 `eframe` 的 `wgpu` Vulkan 后端**：双显卡 / Intel 核显 Vulkan (`Unrecognized present mode 1000361000`) 会导致整窗黑白色块撕裂与断开后剧烈闪烁。必须保留 [crates/desk-app/Cargo.toml](crates/desk-app/Cargo.toml) 中的 `features = ["default_fonts", "glow"]`。
3. **保持单文件零外部 DLL**：保留 [.cargo/config.toml](.cargo/config.toml) `CXXSTDLIB = ""` 与 [crates/desk-app/build.rs](crates/desk-app/build.rs) 静态组链接，每次改动后必须用 `objdump -p` 确认零 `libstdc++-6.dll` / `libwinpthread-1.dll` / `libgcc_s_seh-1.dll`。
4. **保留三层抓屏降级与异步编解码保护**：
   - `capture/dxgi.rs`：`IDXGIOutput5::DuplicateOutput1` → `IDXGIOutput1::DuplicateOutput` → `GdiSingleCapturer`（`BitBlt` + 光标）。
   - `codec/mf_common.rs`：异步 MFT 只能走带超时的 `poll_event_until`，禁止无超时 `GetEvent(Default::default())`；`probe_capabilities` 必须用 640×360 真实帧探活出流。
   - 沙箱 `run_command` 桌面无画面权限（`0x80070005` / `0x80070006`）属于正常安全边界，`--self-test` 已自动退回 1080p 合成帧，严禁试图绕出沙箱桌面。
5. **中转服务端路由与超时顺序不可倒退**：[crates/relay-server/src/state.rs](crates/relay-server/src/state.rs) 必须用 `.rev().find(|e| e.accept_incoming)` 优先投递给最新活跃连接；修改中转端后需重新交叉编译并同步更新 `deploy\flashdesk-relay-linux-x86_64` 与提示用户升级服务端。
6. **`deploy/deploy-relay.sh` 必须严格保持 LF（0 个 `\r` 字节）**，修改后用 `D:\Git\bin\bash.exe -n` 校验。
7. **严禁将 `read_frame` / `NoiseReader::recv_msg` 直接放进 `tokio::select!`**：两者均非 cancel-safe，竞争就绪会丢弃半帧 TCP 字节并破坏 Noise nonce（导致键鼠失灵与断线假死）；必须保留独立 reader task + 取消安全 `mpsc::channel` 隔离设计。

---

## 3. 修改后的标准编译、打包与验收命令

```powershell
# 1. 全量测试 + Windows Release + Linux 静态 Relay 交叉编译
$env:RUSTUP_HOME = "D:\Rust\rustup"; $env:CARGO_HOME = "D:\Rust\cargo"
$env:PATH = "D:\Rust\cargo\bin;D:\Rust\mingw64\bin;$env:PATH"
cargo test --workspace
cargo build --release --workspace
cargo build --release -p relay-server --target x86_64-unknown-linux-musl

# 2. 同步发布产物（先确保未运行旧 target\release\flashdesk.exe）
Copy-Item "D:\Rust\target-flashdesk\release\flashdesk.exe" "target\release\flashdesk.exe" -Force
Copy-Item "D:\Rust\target-flashdesk\release\flashdesk-relay.exe" "target\release\flashdesk-relay.exe" -Force
Copy-Item "D:\Rust\target-flashdesk\x86_64-unknown-linux-musl\release\flashdesk-relay" "deploy\flashdesk-relay-linux-x86_64" -Force
Copy-Item "D:\Rust\target-flashdesk\x86_64-unknown-linux-musl\release\flashdesk-relay" "target\release\flashdesk-relay-linux-x86_64" -Force

# 3. PE 与 zero-DLL 审计（注意先进入纯 ASCII 相对路径目录再调 objdump）
Push-Location target\release
D:\Rust\mingw64\bin\objdump.exe -p .\flashdesk.exe | Select-String "Subsystem|DLL Name"
Pop-Location

# 4. 无 MinGW 洁净环境验证 GUI 启动与 --self-test
$env:PATH = ($env:PATH -split ';' | Where-Object { $_ -and ($_ -notmatch 'mingw') -and ($_ -notmatch 'Rust\\cargo') }) -join ';'
Start-Process -FilePath .\target\release\flashdesk.exe -ArgumentList "--self-test" -Wait -PassThru
```
