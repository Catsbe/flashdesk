# FlashDesk Ubuntu 中转服务器一键部署指南

支持同时部署多台自建节点（例如：**国内 5M 节点**用于低延迟控屏，**海外 1G 节点**用于大文件高速传输，两个节点填同一把个人根公钥即可全机互通）。

---

## 一、准备工作

1. 在任一台 Windows 电脑打开 `flashdesk.exe`：
   - 点击「**生成个人根密钥（主设备）**」。
   - 进入「**设置**」→「身份与设备授权」，点击根公钥右侧的「**复制**」（Base64 字符串）。
2. 将本目录下的两个文件上传到你的 Ubuntu 服务器同一目录（例如 `/tmp/flashdesk/`）：
   - `deploy-relay.sh`
   - `flashdesk-relay-linux-x86_64`（已静态交叉编译，无需在服务器上装任何依赖）

---

## 二、一键部署命令

SSH 登录 Ubuntu 服务器后执行：

```bash
cd /tmp/flashdesk
sudo bash deploy-relay.sh --port 19443 --root-pubkey "粘贴你复制的根公钥"
```

> **穿透公司严格防火墙小技巧**：如果公司网络屏蔽非标端口，可以把 `--port 19443` 改为 `--port 443`（脚本已自动授予 `CAP_NET_BIND_SERVICE` 权限，安全监听 443 端口）。

部署脚本会自动完成：
1. 安装静态二进制至 `/opt/flashdesk/flashdesk-relay`
2. 创建配置文件 `/etc/flashdesk/relay.toml`
3. 开启内核 **BBR** + 32MB TCP 缓冲（显著提升海外 200ms 高延迟链路的大文件吞吐）
4. 注册 `systemd` 后台服务（开机自启、崩溃自动拉起）
5. 若启用了 `ufw` 则自动放行端口

> **提醒**：请在阿里云 / 腾讯云 / 海外云厂商控制台的**安全组 / 防火墙**中也放行对应的 TCP 端口（`19443` 或 `443`）。

---

## 三、常用运维命令

```bash
# 查看运行状态
sudo systemctl status flashdesk-relay

# 查看实时日志
sudo journalctl -u flashdesk-relay -f

# 吊销某台丢失设备的权限（编辑 revoked_device_ids 后重启）
sudo nano /etc/flashdesk/relay.toml
sudo systemctl restart flashdesk-relay

# 卸载服务
sudo bash deploy-relay.sh --uninstall
```

---

## 四、升级中转二进制（2026-11 低延迟版）

2026-11 起中转端对**控屏/视频通道**改用 32 KB 转发缓冲 + `TCP_NOTSENT_LOWAT`（文件通道仍为 512 KB），配合客户端的帧确认流控把 5M 小水管上的画面延迟压在 ~100 ms 量级。只需替换二进制，配置与部署脚本不变：

```bash
sudo systemctl stop flashdesk-relay
sudo install -m 755 flashdesk-relay-linux-x86_64 /opt/flashdesk/flashdesk-relay
sudo systemctl start flashdesk-relay
```

> 旧中转端也能和新客户端互通（流控在客户端两端完成），只是中转自身的大缓冲会多藏几百毫秒排队；建议升级。
