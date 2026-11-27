# FlashDesk 测试指南

改动抓屏、编解码、传输或流控之后，按本文从上到下测一遍：

1. 自动化测试
2. Release 编译与打包审计
3. `--self-test`
4. `--link-bench` 端到端基准
5. 真机联调

背景与设计见 [ARCHITECTURE_AND_PITFALLS.md](ARCHITECTURE_AND_PITFALLS.md) 坑 #16，红线见 [AGENTS.md](../AGENTS.md)。

---

## 0. 先分清两个目录（重要）

| 目录 | 放什么 | 本文变量 |
|---|---|---|
| **项目目录** | 源码 + git 仓库，可在任意位置（例如 `E:\Antigravity项目\flashdesk`） | `$Proj` |
| **Rust 工具链** | `D:\Rust\rustup`、`D:\Rust\cargo`、`D:\Rust\mingw64` | — |
| **编译输出** | `D:\Rust\target-flashdesk`，由 `.cargo\config.toml` 的 `target-dir` 指定，**不在项目目录里** | `$Out` |

- 刚编译出来的程序在 `$Out\release\`：`flashdesk.exe`、`flashdesk-relay.exe`。Linux 中转端在 `$Out\x86_64-unknown-linux-musl\release\flashdesk-relay`。
- 项目里的 `target\release\` 和 `deploy\` 只放**复制过去**的发布副本（第 2 步）。
- `objdump` 不认中文路径：审计直接对 `$Out\release\` 里的文件做，那是纯 ASCII 路径。
- 机器没有 `D:` 盘时，工具链可以装在别处（如 `C:\Rust`），用环境变量覆盖，**不要改配置文件**。做法见架构文档「三、1. 工具链存放红线」。

### 每次打开 PowerShell 先执行

```powershell
$Proj = "E:\Antigravity项目\flashdesk"   # ← 改成你的项目目录
$Out  = "D:\Rust\target-flashdesk"
$env:RUSTUP_HOME = "D:\Rust\rustup"
$env:CARGO_HOME  = "D:\Rust\cargo"
$env:PATH = "D:\Rust\cargo\bin;D:\Rust\mingw64\bin;$env:PATH"
Set-Location $Proj
```

> `cargo` 必须在项目目录里运行，才能读到项目的 `.cargo\config.toml`，产物才会进 `$Out`。

---

## 1. 自动化测试

```powershell
cargo test --workspace
```

预期共 **86 项全部 ok**：

| 测试集 | 数量 | 覆盖内容 |
|---|---|---|
| desk-app | 55 | `net::flow` 的 5M 链路仿真；颜色转换与旧标量实现逐字节一致；缩放与分辨率上限；精确等待；基准标记块；文件传输；剪贴板；远控键盘快捷键转发（Ctrl+C/V/X 等被 egui 吞掉的按下，坑 #21）；剪贴板文字大小上限；托盘与单实例；已吊销设备删除与黑名单保留 |
| protocol | 14 | `control_msg_variant_indices_are_stable` 与 `relay_enum_variant_indices_are_stable`；`FLDP` 二进制差量与压缩往返；更新清单 Ed25519 验签与 semver |
| base-common | 8 | 安装布局探测；原子 JSON 写；`SwitchRequest` 与 15s 看门狗转正/回滚拉黑；旧版本槽清理；`SERVICE_INTERACTIVE_CONTROL_SDDL`；图标 RGBA 绘制 |
| relay_e2e | 7 | 中转端端到端（含 `UpdateChannel` 根签发布/分块下载与 `CheckUpdateNow` 远程触发） |
| release-tool | 2 | `pack` 差量生成/清单签名与 `build-setup` 载荷提取校验 |

---

## 2. Release 编译并同步产物

```powershell
cargo build --release --workspace
cargo build --release -p relay-server --target x86_64-unknown-linux-musl
```

打包独立安装器 `FlashDesk-Setup.exe` 并把产物从 `$Out` 复制回项目。先关掉正在运行的旧 `flashdesk.exe`，否则会提示文件被占用。

```powershell
New-Item -ItemType Directory -Force "$Proj\target\release" | Out-Null
& "$Out\release\flashdesk-release.exe" build-setup `
  --stub "$Out\release\flashdesk-setup-stub.exe" `
  --svc "$Out\release\flashdesk-svc.exe" `
  --launcher "$Out\release\flashdesk-launcher.exe" `
  --uninstall "$Out\release\flashdesk-uninstall.exe" `
  --core "$Out\release\flashdesk.exe" `
  --version 0.1.1 `
  --out "$Proj\target\release\FlashDesk-Setup.exe"

Copy-Item "$Out\release\flashdesk.exe"         "$Proj\target\release\flashdesk.exe" -Force
Copy-Item "$Out\release\flashdesk-relay.exe"   "$Proj\target\release\flashdesk-relay.exe" -Force
Copy-Item "$Out\release\flashdesk-release.exe" "$Proj\target\release\flashdesk-release.exe" -Force
Copy-Item "$Out\x86_64-unknown-linux-musl\release\flashdesk-relay" "$Proj\deploy\flashdesk-relay-linux-x86_64" -Force
Copy-Item "$Out\x86_64-unknown-linux-musl\release\flashdesk-relay" "$Proj\target\release\flashdesk-relay-linux-x86_64" -Force
```

### 打包审计（零 MinGW DLL 与 `.rsrc` 图标资源）

```powershell
Push-Location "$Out\release"
foreach ($bin in @("flashdesk.exe", "flashdesk-svc.exe", "flashdesk-launcher.exe", "flashdesk-uninstall.exe", "flashdesk-setup-stub.exe", "flashdesk-release.exe", "flashdesk-relay.exe")) {
    Write-Host "=== $bin ==="
    D:\Rust\mingw64\bin\objdump.exe -p ".\$bin" | Select-String "Subsystem|DLL Name"
    D:\Rust\mingw64\bin\objdump.exe -h ".\$bin" | Select-String "\.rsrc"
}
Pop-Location
Push-Location "$Proj\target\release"
D:\Rust\mingw64\bin\objdump.exe -p .\FlashDesk-Setup.exe | Select-String "Subsystem|DLL Name"
D:\Rust\mingw64\bin\objdump.exe -h .\FlashDesk-Setup.exe | Select-String "\.rsrc"
Pop-Location
```

- GUI 类程序（`flashdesk.exe`、`flashdesk-svc.exe`、`flashdesk-launcher.exe`、`flashdesk-uninstall.exe`、`FlashDesk-Setup.exe`）的 `Subsystem` 必须是 `00000002 (Windows GUI)`，且 `objdump -h` 均需包含非空 `.rsrc` 图标节。
- **所有二进制均不能出现** `libstdc++-6.dll`、`libwinpthread-1.dll`、`libgcc_s_seh-1.dll`。
- `winmm.dll` 是系统库（1 ms 计时精度用），属于正常。

---

## 3. 自检 `--self-test` 与安装器静默联调（洁净 PATH）

下面第一行会把 MinGW 和 cargo 从**当前会话**的 PATH 里去掉，用来证明程序不依赖它们。之后要再编译，请重开 PowerShell。

```powershell
$env:PATH = ($env:PATH -split ';' | Where-Object { $_ -and ($_ -notmatch 'mingw') -and ($_ -notmatch 'Rust\\cargo') }) -join ';'
& "$Out\release\flashdesk.exe" --self-test | Out-String

# 安装器与启动器端到端闭环验证（非提权临时目录模式）
$tmpInst = Join-Path $env:TEMP "flashdesk-setup-verify"
Remove-Item -Recurse -Force $tmpInst -ErrorAction SilentlyContinue
Start-Process -FilePath "$Proj\target\release\FlashDesk-Setup.exe" -ArgumentList "--silent --no-service --no-shortcuts --dir `"$tmpInst`"" -Wait -PassThru
& "$tmpInst\FlashDesk.exe" --self-test | Out-String
Start-Process -FilePath "$tmpInst\bin\flashdesk-uninstall.exe" -ArgumentList "--silent --dir `"$tmpInst`"" -Wait -PassThru
```

预期如下：

- 最后一行是 `=== All Self-Tests PASSED ===`。
- 第 [5/5] 步打开实时抓屏（`[DXGI]`），然后完成编码、解码往返。
- 在 NVIDIA 机器上，`Encode backends` 应出现 `MF-Hevc-HW (NVIDIA ...)` 和/或 `MF-H264-HW (...)`。如果只有 `OpenH264-SW`，说明硬件编码没用上。

> `flashdesk.exe` 是 GUI 子系统程序，在 PowerShell 里必须加 `| Out-String` 才会等它结束并显示输出。

---

## 4. 端到端基准 `--link-bench`（最重要）

### 它测什么

单机运行，**不需要第二台电脑，也不需要中转服务器**。同一个进程里会跑起：

- 真实的被控端会话：BGRA→NV12、硬件或软件编码、帧确认流控与自适应码率、Noise 加密。
- 一个模拟国内 5M 中转的限速代理：共享瓶颈、单向延迟、1 秒深缓冲。
- 真实的主控端会话：解密、解码、转 RGBA。

只有"屏幕"是合成的：一个 60Hz 桌面，依次经过三个阶段。

| 阶段 | 默认时长（前若干秒为热身，不计入结果） | 模拟什么 |
|---|---|---|
| `scroll` | 12 s（前 3 s 热身） | 连续滚动的文字页面 |
| `video` | 10 s（前 2 s 热身） | 屏幕中间一大块全动态画面，会打满链路，相当于看视频或玩游戏 |
| `typing` | 8 s（前 1 s 热身） | 每 150 ms 打一个字，其余静止 |

每帧左上角画有编号标记块，主控端解码后读回编号，就能精确算出**"屏幕出新画面 → 主控端解码完成"**的延迟。

请在**被控端那台机器**（双 2K + NVIDIA）上跑，这样测到的才是真实的硬件编码路径。那台机器不需要装 Rust，把 `flashdesk.exe` 复制过去就能跑。系统服务开着也没关系：基准只用本机回环和随机端口。

### 命令

```powershell
# 默认：1080p60、5 Mbps、码率上限 4000 kbps
& "$Out\release\flashdesk.exe" --link-bench | Out-String; "exit=$LASTEXITCODE"

# 你的真实分辨率：2K 原生
& "$Out\release\flashdesk.exe" --link-bench --bench-res 2560x1440 | Out-String; "exit=$LASTEXITCODE"

# 2K 屏 + 1080p 分辨率上限（走 5M 中转的推荐设置）
& "$Out\release\flashdesk.exe" --link-bench --bench-res 2560x1440 --bench-cap 1080 | Out-String; "exit=$LASTEXITCODE"
```

**新旧对比（可选）**：3 Mbps 拥塞链路、8 Mbps 码率上限，模拟中转被别的流量挤占、或者滑块拉得太高。两条各跑一次，看 `video`、`typing` 两个阶段的延迟：

```powershell
# 关闭帧确认 = 旧传输行为（预期延迟堆到 ~2 秒）
& "$Out\release\flashdesk.exe" --link-bench --bench-mbps 3 --bench-max-kbps 8000 --bench-no-ack | Out-String
# 新流控（预期 ~0.2 秒）
& "$Out\release\flashdesk.exe" --link-bench --bench-mbps 3 --bench-max-kbps 8000 | Out-String
```

在这个对比场景里，`video` 阶段的帧率判定出现 FAIL 是正常的，**看的是两次延迟差了多少**。

### 怎么看输出

- 开头的 `encoders:` 行列出实际用到的编码器。在 NVIDIA 机器上应该是 `MF-Hevc-HW (NVIDIA …)` 之类。
- 夹在中间的 `[OpenH264] … Warning …` 是软件编码器自己打的提示，可以忽略。

每秒一行的表格：

| 列 | 含义 | 期望 |
|---|---|---|
| `fps` | 主控端每秒解码帧数 | 接近 60；`typing` 阶段约 26，因为每个字后面会补 3 个冲刷帧，这是设计如此 |
| `cap/sent` | 被控端每秒抓到的帧 / 发出的帧 | 两数接近，说明编码器没有吞帧 |
| `recv` | 主控端实际收到的码率 | `video` 阶段接近但不超过链路带宽 |
| `ABR sent/tgt` | 被控端实际编码码率 / 自适应码率目标 | 硬件编码器应基本跟随目标 |
| `frame delay` | 被控端测得的"发出 → 主控解码完成"，平均/最大 | 稳定，不随时间增长 |
| `gate/busy` | 被流控闸门跳过的帧 / 因网络写入忙跳过的帧 | 拥塞时出现，属于正常；一直很大说明链路或主控解码跟不上 |
| `capture` | 抓一帧的耗时，含等待画面更新和颜色转换/缩放 | `typing` 阶段偏大是因为在等画面变化 |
| `encode` | 编码一帧耗时 | 硬件编码一般只有几 ms，软件编码 10–15 ms |
| `decode` | 主控端解码 + 转 RGBA 耗时 | 应明显小于 16.7 ms |
| `latency avg/p95` | 标记块测得的"出画 → 解码完成" | 见下面的判定标准 |

末尾的 `=== Results ===` 汇总三个阶段的帧率和延迟（avg、p50、p95、max），最后一行给出判定，**退出码 0 即 PASS**。默认参数（单向延迟 15 ms）下的标准：

| 阶段 | 帧率要求 | p95 延迟要求 |
|---|---|---|
| scroll | ≥ 54 fps（0.9 × 目标帧率） | ≤ 150 ms（2 × 单向延迟 + 120 ms） |
| video | ≥ 45 fps（0.75 × 目标帧率） | ≤ 230 ms（再放宽 80 ms） |
| typing | — | ≤ 150 ms |

### 参考数据

2026-11-23 测于 4 vCPU 虚拟机：没有硬件编码器（OpenH264 软件编码 + 微软 H.264 软件解码），被控、主控和代理全挤在同一台机器上。真机配合硬件编码应当不比这差。

| 场景 | 滚动 fps / p95 | 视频 fps / p95 | 打字 p95 |
|---|---|---|---|
| 1080p60，5 Mbps | 56–60 / 61 ms | 58 / 83 ms | 44 ms |
| 2560×1440 + 1080p 上限，5 Mbps | 59.7 / 74 ms | 54 / 118 ms | 51 ms |
| 3 Mbps 拥塞，关闭帧确认（旧） | 56 / 65 ms | 23 / **1983 ms** | **2005 ms** |
| 3 Mbps 拥塞，新流控 | 56 / 63 ms | 28.5 / **221 ms** | **39 ms** |

### 参数

| 参数 | 默认 | 说明 |
|---|---|---|
| `--bench-mbps` | 5.0 | 瓶颈带宽（Mbps） |
| `--bench-delay-ms` | 15 | 单向延迟（RTT = 2 倍） |
| `--bench-queue-ms` | 1000 | 中转缓冲深度，超过后链路才开始反压 |
| `--bench-res` | 1920x1080 | 合成桌面分辨率 |
| `--bench-fps` | 60 | 目标帧率 |
| `--bench-max-kbps` | 4000 | 码率上限，相当于工具栏滑块 |
| `--bench-cap` | 0 | 分辨率上限：0 = 原始，1080，720 |
| `--bench-scroll-secs` / `--bench-video-secs` / `--bench-typing-secs` | 12 / 10 / 8 | 各阶段时长，设为 0 跳过视频阶段 |
| `--bench-no-ack` | 关 | 关闭帧确认，模拟旧传输行为，用来对比 |

---

## 5. 真机联调

### 5.1 更新三处程序

**中转服务器（Linux）**：把 `$Proj\deploy\flashdesk-relay-linux-x86_64` 上传到服务器任意目录（例如 `/tmp/flashdesk/`），在该目录执行下面三条命令。配置文件和部署脚本都不用动。

```bash
sudo systemctl stop flashdesk-relay
sudo install -m 755 flashdesk-relay-linux-x86_64 /opt/flashdesk/flashdesk-relay
sudo systemctl start flashdesk-relay
```

**主控端**：关掉 FlashDesk，用新的 `flashdesk.exe` 覆盖旧文件，再打开。

**被控端**：

- **装了系统服务**：服务直接运行**安装时那个位置**的 `flashdesk.exe`，正在运行的文件不能覆盖，但可以改名。在那个 exe 所在目录，用**管理员** PowerShell 执行：

  ```powershell
  Rename-Item .\flashdesk.exe flashdesk.exe.running-old
  Copy-Item "$Out\release\flashdesk.exe" .\flashdesk.exe    # 或从别处复制来的新版
  .\flashdesk.exe --restart-service | Out-String
  ```

  `flashdesk.exe.running-old` 等下次服务重启后就可以删掉。
- **没装服务**：关掉程序，直接覆盖 exe。

> 两端都必须是新版，新的流控和自适应码率才会生效。旧版和新版之间也能连上，只是会退回旧的固定码率行为。

### 5.2 设置

- **码率上限**：旧配置里存的是 2500 kbps，升级不会自动改。建议在「设置 → 画面与传输」或远控工具栏「画质」里把「码率上限」调到 **4000** 左右。现在有自适应码率兜底，调高不会再造成排队。
- **分辨率上限**：2K/4K 屏走 5M 中转时，可以在远控工具栏「画质」或设置里选 **1080p**，每帧数据量会减半。

### 5.3 看 HUD

默认收起以保持画面简洁，点击顶部工具栏「文件传输」右侧的「**详情**」按钮会在工具栏下方展开统计面板：

- **第一行**：`fps · ms 延迟 · Mbps`。其中「延迟」是控制通道的往返时间（RTT），**量不到画面排队**。
- **第二行**（两端都是新版才会出现）：`画面 X ms（峰值 Y）· 码率 实际/目标 M · 解码 Z ms · 限流 N 帧`
  - 「画面」是被控端发出到主控端解码完成的时间，**真正的画面延迟看这个**。
  - 没有这一行，说明还有一端是旧版。
- **最后一行**：远程分辨率 · 编码器 · 编码耗时 · 中转节点。

### 5.4 场景清单

| 操作 | 预期 |
|---|---|
| 滚动长网页、拖动窗口 10 秒以上 | FPS 约 55–60；「画面」约等于「延迟」再加 30–80 ms，**不随时间增长** |
| 窗口里播放视频 | 自适应码率稳定在链路以下（5M 中转约 4–4.5 Mbps）；画面可以变糊，但不能出现秒级延迟 |
| 记事本打字、点菜单 | 立刻出现（< 0.1 s），不再"隔一下才变" |
| 工具栏「画质」里切换 1080p / 原始分辨率 | 短暂刷新一下，然后流畅 |
| 远端选中文字按 Ctrl+C，回到本机 Ctrl+V | 本机粘贴出远端的文字（Ctrl+X、Ctrl+Insert 同理） |
| 本机复制文字，在远端按 Ctrl+V（Shift+Insert 同理） | 远端粘贴出本机的文字；**连接前**复制的文字（≤ 256 KiB）连上后也能直接粘贴 |
| 本机复制一张小图片，在远端支持图片的程序里 Ctrl+V | 远端粘贴出图片 |
| 复制一张大截图（剪贴板图片） | 键鼠可能卡几秒，**这是已知遗留问题，尚未修复**（见架构文档「五、后续方向」） |

> 剪贴板没同步时，看两端日志里的 `clipboard: sending local … to peer` / `clipboard: applied … from peer`（只记类型和字节数，不记内容）和 `apply remote clipboard failed`，就能判断卡在哪一端。

---

## 6. 出问题时请收集

1. **HUD 截图**：点击顶部工具栏「详情」展开的统计面板，所有行都要。
2. **主控端日志**：配置文件同目录的 `flashdesk.log`。
3. **被控端日志**：
   - 服务模式：`C:\ProgramData\FlashDesk\flashdesk-agent.log`。
   - GUI 被控：exe 同目录的 `flashdesk.log`。
4. **基准输出**：被控端那台机器上第 4 步 `--link-bench` 的完整输出。
