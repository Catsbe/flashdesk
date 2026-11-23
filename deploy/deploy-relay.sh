#!/usr/bin/env bash
set -euo pipefail

# FlashDesk Ubuntu Relay Server One-Click Deployment Script
# Installs static relay binary, applies BBR + 32MB TCP tuning, and configures systemd.

PORT=""
ROOT_PUBKEY=""
BINARY_PATH=""
UNINSTALL=0

usage() {
    cat <<EOF
用法: sudo bash $0 [选项]
选项:
  --port <端口>            监听 TCP 端口（默认 19443，推荐 443 可过企业防火墙）
  --root-pubkey <Base64>   从 Windows 客户端「设置与密钥」复制的个人根公钥
  --binary <路径>          flashdesk-relay Linux 二进制路径（默认自动查找同目录 flashdesk-relay-linux-x86_64）
  --uninstall              停止并卸载 flashdesk-relay 服务
  -h, --help               显示帮助
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --port)
            PORT="${2:-}"
            shift 2
            ;;
        --root-pubkey)
            ROOT_PUBKEY="${2:-}"
            shift 2
            ;;
        --binary)
            BINARY_PATH="${2:-}"
            shift 2
            ;;
        --uninstall)
            UNINSTALL=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "未知参数: $1"
            usage
            exit 1
            ;;
    esac
done

if [[ "${EUID}" -ne 0 ]]; then
    echo "❌ 请使用 root 或 sudo 运行此脚本（例如: sudo bash $0）"
    exit 1
fi

if [[ "${UNINSTALL}" -eq 1 ]]; then
    echo "正在停止并卸载 flashdesk-relay..."
    systemctl disable --now flashdesk-relay >/dev/null 2>&1 || true
    rm -f /etc/systemd/system/flashdesk-relay.service
    systemctl daemon-reload
    rm -rf /opt/flashdesk
    echo "✅ 已卸载服务（配置文件 /etc/flashdesk/relay.toml 已保留，如需清理请手动删除）。"
    exit 0
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ -z "${BINARY_PATH}" ]]; then
    if [[ -f "${SCRIPT_DIR}/flashdesk-relay-linux-x86_64" ]]; then
        BINARY_PATH="${SCRIPT_DIR}/flashdesk-relay-linux-x86_64"
    elif [[ -f "${SCRIPT_DIR}/flashdesk-relay" ]]; then
        BINARY_PATH="${SCRIPT_DIR}/flashdesk-relay"
    fi
fi

# Interactive prompts if flags were omitted
if [[ -z "${PORT}" ]]; then
    read -r -p "请输入监听端口 [默认 19443，如需伪装 HTTPS 端口可填 443]: " input_port || true
    PORT="${input_port:-19443}"
fi

if ! [[ "${PORT}" =~ ^[0-9]+$ ]] || (( PORT < 1 || PORT > 65535 )); then
    echo "❌ 无效端口号: ${PORT}"
    exit 1
fi

if [[ -z "${ROOT_PUBKEY}" ]] && [[ -f /etc/flashdesk/relay.toml ]]; then
    existing_pk="$(grep -E '^root_pubkey_b64' /etc/flashdesk/relay.toml | head -n1 | sed -E 's/.*"([^"]+)".*/\1/' || true)"
    if [[ -n "${existing_pk}" ]]; then
        echo "检测到已有配置文件中的根公钥: ${existing_pk}"
        ROOT_PUBKEY="${existing_pk}"
    fi
fi

if [[ -z "${ROOT_PUBKEY}" ]]; then
    read -r -p "请粘贴客户端「设置与密钥」中的个人根公钥 (Base64): " ROOT_PUBKEY
fi

if [[ -z "${ROOT_PUBKEY}" ]]; then
    echo "❌ 根公钥不能为空！"
    exit 1
fi

echo "[1/5] 准备中转服务端二进制..."
mkdir -p /opt/flashdesk /etc/flashdesk

if [[ -n "${BINARY_PATH}" ]] && [[ -f "${BINARY_PATH}" ]]; then
    install -m 0755 "${BINARY_PATH}" /opt/flashdesk/flashdesk-relay
elif [[ -f "${SCRIPT_DIR}/../Cargo.toml" ]]; then
    echo "未找到预编译 Linux 二进制，尝试在当前 Ubuntu 上从源码编译..."
    if ! command -v cargo >/dev/null 2>&1; then
        apt-get update -y
        apt-get install -y curl build-essential pkg-config
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        # shellcheck source=/dev/null
        source "${HOME}/.cargo/env"
    fi
    (cd "${SCRIPT_DIR}/.." && cargo build --release -p relay-server)
    BUILT_BIN="${SCRIPT_DIR}/../target/release/flashdesk-relay"
    if [[ ! -f "${BUILT_BIN}" ]]; then
        BUILT_BIN="$(find "${SCRIPT_DIR}/.." -name flashdesk-relay -type f -executable | head -n1 || true)"
    fi
    if [[ -z "${BUILT_BIN}" ]] || [[ ! -f "${BUILT_BIN}" ]]; then
        echo "❌ 源码编译后未找到 flashdesk-relay 二进制"
        exit 1
    fi
    install -m 0755 "${BUILT_BIN}" /opt/flashdesk/flashdesk-relay
else
    echo "❌ 未找到 flashdesk-relay-linux-x86_64，请将其与 deploy-relay.sh 放在同一目录或通过 --binary 指定。"
    exit 1
fi

echo "[2/5] 创建系统服务账号并写入 /etc/flashdesk/relay.toml..."
if ! id -u flashdesk >/dev/null 2>&1; then
    useradd --system --no-create-home --shell /usr/sbin/nologin flashdesk
fi

cat > /etc/flashdesk/relay.toml <<EOF
listen_addr = "0.0.0.0:${PORT}"
root_pubkey_b64 = "${ROOT_PUBKEY}"
revoked_device_ids = []
pending_timeout_secs = 15
EOF
chown -R flashdesk:flashdesk /opt/flashdesk /etc/flashdesk
chmod 0640 /etc/flashdesk/relay.toml

echo "[3/5] 开启内核 BBR 拥塞控制与大缓冲区优化..."
modprobe tcp_bbr >/dev/null 2>&1 || true
cat > /etc/sysctl.d/99-flashdesk-bbr.conf <<'EOF'
net.core.default_qdisc = fq
net.ipv4.tcp_congestion_control = bbr
net.core.rmem_max = 33554432
net.core.wmem_max = 33554432
net.ipv4.tcp_rmem = 4096 87380 33554432
net.ipv4.tcp_wmem = 4096 65536 33554432
net.ipv4.tcp_slow_start_after_idle = 0
net.ipv4.tcp_mtu_probing = 1
EOF
sysctl -p /etc/sysctl.d/99-flashdesk-bbr.conf >/dev/null 2>&1 || sysctl --system >/dev/null 2>&1 || true

echo "[4/5] 安装并启动 systemd 服务 flashdesk-relay..."
cat > /etc/systemd/system/flashdesk-relay.service <<'EOF'
[Unit]
Description=FlashDesk Encrypted Multi-Channel Relay Server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=flashdesk
Group=flashdesk
ExecStart=/opt/flashdesk/flashdesk-relay serve --config /etc/flashdesk/relay.toml
Restart=always
RestartSec=3
LimitNOFILE=65535
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=true
ProtectSystem=strict
ReadOnlyPaths=/etc/flashdesk
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable flashdesk-relay >/dev/null
systemctl restart flashdesk-relay

if command -v ufw >/dev/null 2>&1 && ufw status | grep -q "Status: active"; then
    echo "检测到 UFW 防火墙已启用，正在放行 TCP ${PORT}..."
    ufw allow "${PORT}/tcp" >/dev/null
fi

echo "[5/5] 校验服务运行状态..."
sleep 1
if systemctl is-active --quiet flashdesk-relay; then
    echo "✅ FlashDesk 中转服务端已成功运行！"
    echo "   - 监听地址: 0.0.0.0:${PORT}"
    echo "   - 拥塞算法: $(sysctl -n net.ipv4.tcp_congestion_control 2>/dev/null || echo unknown)"
    ss -tlnp 2>/dev/null | grep ":${PORT} " || true
    echo ""
    echo "⚠️ 请确认云厂商控制台安全组已放行 TCP ${PORT} 端口！"
    echo "   查看实时日志: sudo journalctl -u flashdesk-relay -f"
else
    echo "❌ 服务启动失败，最近日志如下:"
    journalctl -u flashdesk-relay -n 30 --no-pager || true
    exit 1
fi
