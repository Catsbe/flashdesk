pub mod bench;
pub mod direct;
pub mod flow;
pub mod host_session;
pub mod relay_client;
pub mod viewer_session;

use anyhow::Result;
use protocol::auth::DeviceIdentity;
use protocol::framing::{read_frame, write_frame};
use protocol::relay::{
    ChannelKind, ConnectionPurpose, OnlineDevice, RelayAuthRequest, RelayAuthResponse, RelayHello,
};
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;
use tokio::net::TcpStream;

#[derive(Debug, Clone, Default)]
pub struct RelayStatus {
    pub connected: bool,
    pub relay_id: String,
    pub relay_name: String,
    pub relay_addr: String,
    pub rtt_ms: Option<u32>,
    pub error: Option<String>,
    pub devices: Vec<OnlineDevice>,
    pub last_revocation_sync: Option<(usize, u64)>,
}

pub async fn connect_and_auth_relay(
    addr: &str,
    identity: &DeviceIdentity,
    purpose: ConnectionPurpose,
) -> Result<TcpStream> {
    let mut stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow::anyhow!("连接中转服务器超时: {addr}"))??;

    let is_file = matches!(
        purpose,
        ConnectionPurpose::OpenSession {
            channel: ChannelKind::File,
            ..
        } | ConnectionPurpose::AcceptSession {
            channel: ChannelKind::File,
            ..
        }
    );
    if !is_file {
        let _ = stream.set_nodelay(true);
    }

    tokio::time::timeout(Duration::from_secs(5), async {
        let raw_hello = read_frame(&mut stream).await?;
        let hello: RelayHello = bincode::deserialize(&raw_hello)?;

        let req = RelayAuthRequest {
            cert: identity.cert.clone(),
            challenge_sig: identity
                .sign_challenge(&hello.server_nonce, &purpose.canonical_bytes()),
            purpose,
        };
        write_frame(&mut stream, &bincode::serialize(&req)?).await?;

        let raw_resp = read_frame(&mut stream).await?;
        let resp: RelayAuthResponse = bincode::deserialize(&raw_resp)?;
        match resp {
            RelayAuthResponse::Ok => Ok(stream),
            RelayAuthResponse::Rejected(reason) => anyhow::bail!("中转认证被拒绝: {reason}"),
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("中转握手超时"))?
}

pub fn local_lan_ip() -> String {
    if let Some(ip) = preferred_lan_ipv4() {
        return ip.to_string();
    }
    // Fallback: route probe. Reject fake-ip / link-local results (Clash TUN etc. would otherwise
    // advertise an unreachable 198.18.x.x address as the "LAN direct" endpoint).
    if let Ok(sock) = UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("223.5.5.5:53").is_ok() {
            if let Ok(SocketAddr::V4(v4)) = sock.local_addr() {
                let ip = *v4.ip();
                if is_usable_lan_ipv4(ip) {
                    return ip.to_string();
                }
            }
        }
    }
    "127.0.0.1".into()
}

/// Usable for LAN direct-connect advertising: not loopback/link-local/unspecified, not the
/// 198.18.0.0/15 benchmark range used by Clash-style fake-ip TUNs, not CGNAT 100.64.0.0/10.
pub fn is_usable_lan_ipv4(ip: std::net::Ipv4Addr) -> bool {
    if ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || ip.is_broadcast() {
        return false;
    }
    let o = ip.octets();
    if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
        return false;
    }
    if o[0] == 100 && (o[1] & 0xC0) == 64 {
        return false;
    }
    true
}

/// Picks the best IPv4 address of an operational physical adapter: RFC1918 first, then any other
/// usable address. Avoids virtual/fake-ip adapters that the old route-probe heuristic returned.
fn preferred_lan_ipv4() -> Option<std::net::Ipv4Addr> {
    use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH, GAA_FLAG_SKIP_ANYCAST,
        GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
    };
    const AF_INET: u32 = 2;
    const IF_OPER_STATUS_UP: i32 = 1;
    const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;

    unsafe {
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
        let mut size = 16 * 1024u32;
        // u64 buffer guarantees the 8-byte alignment the adapter structs need.
        let mut buf: Vec<u64> = vec![0; (size as usize) / 8 + 1];
        let mut ret = GetAdaptersAddresses(
            AF_INET,
            flags,
            None,
            Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
            &mut size,
        );
        if ret == ERROR_BUFFER_OVERFLOW.0 {
            buf = vec![0; (size as usize) / 8 + 1];
            ret = GetAdaptersAddresses(
                AF_INET,
                flags,
                None,
                Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
                &mut size,
            );
        }
        if ret != ERROR_SUCCESS.0 {
            return None;
        }

        let mut fallback: Option<std::net::Ipv4Addr> = None;
        let mut p = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !p.is_null() {
            let ad = &*p;
            if ad.OperStatus.0 == IF_OPER_STATUS_UP && ad.IfType != IF_TYPE_SOFTWARE_LOOPBACK {
                let mut ua = ad.FirstUnicastAddress;
                while !ua.is_null() {
                    let sa = (*ua).Address.lpSockaddr;
                    if !sa.is_null() && (*sa).sa_family.0 == AF_INET as u16 {
                        // sockaddr_in: u16 family, u16 port, then 4 IPv4 octets.
                        let base = sa as *const u8;
                        let ip = std::net::Ipv4Addr::new(
                            *base.add(4),
                            *base.add(5),
                            *base.add(6),
                            *base.add(7),
                        );
                        if is_usable_lan_ipv4(ip) {
                            if ip.is_private() {
                                return Some(ip);
                            }
                            if fallback.is_none() {
                                fallback = Some(ip);
                            }
                        }
                    }
                    ua = (*ua).Next;
                }
            }
            p = ad.Next;
        }
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::is_usable_lan_ipv4;
    use std::net::Ipv4Addr;

    #[test]
    fn lan_ip_filter_rejects_fake_ip_and_special_ranges() {
        assert!(is_usable_lan_ipv4(Ipv4Addr::new(192, 168, 1, 100)));
        assert!(is_usable_lan_ipv4(Ipv4Addr::new(10, 0, 0, 5)));
        assert!(is_usable_lan_ipv4(Ipv4Addr::new(172, 16, 0, 9)));
        assert!(!is_usable_lan_ipv4(Ipv4Addr::new(198, 18, 0, 1))); // Clash fake-ip
        assert!(!is_usable_lan_ipv4(Ipv4Addr::new(198, 19, 255, 1)));
        assert!(!is_usable_lan_ipv4(Ipv4Addr::new(169, 254, 1, 1)));
        assert!(!is_usable_lan_ipv4(Ipv4Addr::new(127, 0, 0, 1)));
        assert!(!is_usable_lan_ipv4(Ipv4Addr::new(100, 64, 0, 1))); // CGNAT / Tailscale
        assert!(!is_usable_lan_ipv4(Ipv4Addr::new(0, 0, 0, 0)));
    }
}
