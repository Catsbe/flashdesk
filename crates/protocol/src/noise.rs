use crate::auth::{verify_domain_signature, DeviceCert, DeviceIdentity};
use crate::framing::{read_frame, write_frame, MAX_FRAME_LEN};
use crate::{unix_timestamp_secs, ProtocolError, Result};
use serde::{Deserialize, Serialize};
use snow::{Builder, StatelessTransportState};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use uuid::Uuid;

const NOISE_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const BIND_DOMAIN: &[u8] = b"FLASHDESK-NOISE-BIND-V1";
const CHUNK_PLAINTEXT_LIMIT: usize = 60_000;
const NOISE_TAG_LEN: usize = 16;
const CHUNK_FLAG_MORE: u8 = 0;
const CHUNK_FLAG_FINAL: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PeerAuthBind {
    cert: DeviceCert,
    handshake_sig: Vec<u8>,
}

pub struct NoiseReader<R> {
    reader: R,
    transport: Arc<StatelessTransportState>,
    recv_nonce: u64,
    peer_cert: DeviceCert,
}

pub struct NoiseWriter<W> {
    writer: W,
    transport: Arc<StatelessTransportState>,
    send_nonce: u64,
    peer_cert: DeviceCert,
}

pub struct NoiseStream<S> {
    stream: S,
    transport: Arc<StatelessTransportState>,
    send_nonce: u64,
    recv_nonce: u64,
    peer_cert: DeviceCert,
}

impl<R: AsyncRead + Unpin> NoiseReader<R> {
    pub fn peer_cert(&self) -> &DeviceCert {
        &self.peer_cert
    }

    pub async fn recv_msg(&mut self) -> Result<Vec<u8>> {
        recv_chunked(&mut self.reader, &self.transport, &mut self.recv_nonce).await
    }
}

impl<W: AsyncWrite + Unpin> NoiseWriter<W> {
    pub fn peer_cert(&self) -> &DeviceCert {
        &self.peer_cert
    }

    pub async fn send_msg(&mut self, data: &[u8]) -> Result<()> {
        send_chunked(&mut self.writer, &self.transport, &mut self.send_nonce, data).await
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> NoiseStream<S> {
    pub fn peer_cert(&self) -> &DeviceCert {
        &self.peer_cert
    }

    pub async fn send_msg(&mut self, data: &[u8]) -> Result<()> {
        send_chunked(&mut self.stream, &self.transport, &mut self.send_nonce, data).await
    }

    pub async fn recv_msg(&mut self) -> Result<Vec<u8>> {
        recv_chunked(&mut self.stream, &self.transport, &mut self.recv_nonce).await
    }

    pub fn into_split_with<R, W>(
        self,
        split_fn: impl FnOnce(S) -> (R, W),
    ) -> (NoiseReader<R>, NoiseWriter<W>, DeviceCert) {
        let (reader, writer) = split_fn(self.stream);
        let peer_cert = self.peer_cert;
        (
            NoiseReader {
                reader,
                transport: Arc::clone(&self.transport),
                recv_nonce: self.recv_nonce,
                peer_cert: peer_cert.clone(),
            },
            NoiseWriter {
                writer,
                transport: self.transport,
                send_nonce: self.send_nonce,
                peer_cert: peer_cert.clone(),
            },
            peer_cert,
        )
    }

    pub fn into_split(self) -> (NoiseReader<ReadHalf<S>>, NoiseWriter<WriteHalf<S>>, DeviceCert) {
        self.into_split_with(tokio::io::split)
    }

    /// Performs Noise_XX initiator handshake and verifies peer's Ed25519 DeviceCert binding.
    pub async fn handshake_initiator(
        mut stream: S,
        root_pubkey: &[u8; 32],
        identity: &DeviceIdentity,
        revoked_ids: &[Uuid],
    ) -> Result<Self> {
        let builder = Builder::new(
            NOISE_PARAMS
                .parse()
                .map_err(|e| ProtocolError::Noise(format!("invalid noise params: {e}")))?,
        );
        let static_kp = builder
            .generate_keypair()
            .map_err(|e| ProtocolError::Noise(format!("noise keygen failed: {e}")))?;
        let mut hs = builder
            .local_private_key(&static_kp.private)
            .build_initiator()
            .map_err(|e| ProtocolError::Noise(format!("build initiator failed: {e}")))?;

        let mut buf = vec![0u8; 65535];
        let mut out = vec![0u8; 65535];

        // -> e
        let n = hs
            .write_message(&[], &mut buf)
            .map_err(|e| ProtocolError::Noise(format!("noise msg 1 write failed: {e}")))?;
        write_frame(&mut stream, &buf[..n]).await?;

        // <- e, ee, s, es
        let msg2 = read_frame(&mut stream).await?;
        hs.read_message(&msg2, &mut out)
            .map_err(|e| ProtocolError::Noise(format!("noise msg 2 read failed: {e}")))?;

        // -> s, se
        let n = hs
            .write_message(&[], &mut buf)
            .map_err(|e| ProtocolError::Noise(format!("noise msg 3 write failed: {e}")))?;
        write_frame(&mut stream, &buf[..n]).await?;

        let hs_hash = hs.get_handshake_hash().to_vec();
        let transport = Arc::new(
            hs.into_stateless_transport_mode()
                .map_err(|e| ProtocolError::Noise(format!("transport transition failed: {e}")))?,
        );

        let mut send_nonce = 0u64;
        let mut recv_nonce = 0u64;

        let local_bind = make_auth_bind(identity, &hs_hash)?;
        send_chunked(&mut stream, &transport, &mut send_nonce, &local_bind).await?;

        let remote_bind = recv_chunked(&mut stream, &transport, &mut recv_nonce).await?;
        let peer_cert = verify_auth_bind(&remote_bind, root_pubkey, revoked_ids, &hs_hash)?;

        Ok(Self {
            stream,
            transport,
            send_nonce,
            recv_nonce,
            peer_cert,
        })
    }

    /// Performs Noise_XX responder handshake and verifies peer's Ed25519 DeviceCert binding.
    pub async fn handshake_responder(
        mut stream: S,
        root_pubkey: &[u8; 32],
        identity: &DeviceIdentity,
        revoked_ids: &[Uuid],
    ) -> Result<Self> {
        let builder = Builder::new(
            NOISE_PARAMS
                .parse()
                .map_err(|e| ProtocolError::Noise(format!("invalid noise params: {e}")))?,
        );
        let static_kp = builder
            .generate_keypair()
            .map_err(|e| ProtocolError::Noise(format!("noise keygen failed: {e}")))?;
        let mut hs = builder
            .local_private_key(&static_kp.private)
            .build_responder()
            .map_err(|e| ProtocolError::Noise(format!("build responder failed: {e}")))?;

        let mut buf = vec![0u8; 65535];
        let mut out = vec![0u8; 65535];

        // <- e
        let msg1 = read_frame(&mut stream).await?;
        hs.read_message(&msg1, &mut out)
            .map_err(|e| ProtocolError::Noise(format!("noise msg 1 read failed: {e}")))?;

        // -> e, ee, s, es
        let n = hs
            .write_message(&[], &mut buf)
            .map_err(|e| ProtocolError::Noise(format!("noise msg 2 write failed: {e}")))?;
        write_frame(&mut stream, &buf[..n]).await?;

        // <- s, se
        let msg3 = read_frame(&mut stream).await?;
        hs.read_message(&msg3, &mut out)
            .map_err(|e| ProtocolError::Noise(format!("noise msg 3 read failed: {e}")))?;

        let hs_hash = hs.get_handshake_hash().to_vec();
        let transport = Arc::new(
            hs.into_stateless_transport_mode()
                .map_err(|e| ProtocolError::Noise(format!("transport transition failed: {e}")))?,
        );

        let mut send_nonce = 0u64;
        let mut recv_nonce = 0u64;

        let remote_bind = recv_chunked(&mut stream, &transport, &mut recv_nonce).await?;
        let peer_cert = verify_auth_bind(&remote_bind, root_pubkey, revoked_ids, &hs_hash)?;

        let local_bind = make_auth_bind(identity, &hs_hash)?;
        send_chunked(&mut stream, &transport, &mut send_nonce, &local_bind).await?;

        Ok(Self {
            stream,
            transport,
            send_nonce,
            recv_nonce,
            peer_cert,
        })
    }
}

fn make_auth_bind(identity: &DeviceIdentity, hs_hash: &[u8]) -> Result<Vec<u8>> {
    let bind = PeerAuthBind {
        cert: identity.cert.clone(),
        handshake_sig: identity.sign_domain_payload(BIND_DOMAIN, hs_hash),
    };
    bincode::serialize(&bind).map_err(|e| ProtocolError::Serialization(e.to_string()))
}

fn verify_auth_bind(
    raw: &[u8],
    root_pubkey: &[u8; 32],
    revoked_ids: &[Uuid],
    hs_hash: &[u8],
) -> Result<DeviceCert> {
    let bind: PeerAuthBind =
        bincode::deserialize(raw).map_err(|e| ProtocolError::Serialization(e.to_string()))?;
    if revoked_ids.contains(&bind.cert.device_id) {
        return Err(ProtocolError::Auth(format!(
            "peer device {} is revoked",
            bind.cert.device_id
        )));
    }
    bind.cert.verify(root_pubkey, unix_timestamp_secs())?;
    verify_domain_signature(
        &bind.cert.device_pubkey,
        BIND_DOMAIN,
        hs_hash,
        &bind.handshake_sig,
    )?;
    Ok(bind.cert)
}

async fn send_chunked<W: AsyncWrite + Unpin>(
    writer: &mut W,
    transport: &StatelessTransportState,
    nonce: &mut u64,
    data: &[u8],
) -> Result<()> {
    if data.len() > MAX_FRAME_LEN {
        return Err(ProtocolError::Framing(format!(
            "message exceeds max size: {} > {}",
            data.len(),
            MAX_FRAME_LEN
        )));
    }

    let mut plain = Vec::with_capacity(CHUNK_PLAINTEXT_LIMIT + 1);
    let mut cipher = vec![0u8; 1 + CHUNK_PLAINTEXT_LIMIT + NOISE_TAG_LEN];

    if data.is_empty() {
        plain.push(CHUNK_FLAG_FINAL);
        let n = transport
            .write_message(*nonce, &plain, &mut cipher)
            .map_err(|e| ProtocolError::Noise(format!("noise encrypt failed: {e}")))?;
        *nonce = nonce
            .checked_add(1)
            .ok_or_else(|| ProtocolError::Noise("nonce overflow".into()))?;
        return write_frame(writer, &cipher[..n]).await;
    }

    let total_chunks = data.len().div_ceil(CHUNK_PLAINTEXT_LIMIT);
    for (idx, chunk) in data.chunks(CHUNK_PLAINTEXT_LIMIT).enumerate() {
        let is_final = idx + 1 == total_chunks;
        plain.clear();
        plain.push(if is_final {
            CHUNK_FLAG_FINAL
        } else {
            CHUNK_FLAG_MORE
        });
        plain.extend_from_slice(chunk);

        let n = transport
            .write_message(*nonce, &plain, &mut cipher)
            .map_err(|e| ProtocolError::Noise(format!("noise encrypt failed: {e}")))?;
        *nonce = nonce
            .checked_add(1)
            .ok_or_else(|| ProtocolError::Noise("nonce overflow".into()))?;
        write_frame(writer, &cipher[..n]).await?;
    }

    Ok(())
}

async fn recv_chunked<R: AsyncRead + Unpin>(
    reader: &mut R,
    transport: &StatelessTransportState,
    nonce: &mut u64,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut plain = vec![0u8; 65535];

    loop {
        let cipher = read_frame(reader).await?;
        let n = transport
            .read_message(*nonce, &cipher, &mut plain)
            .map_err(|e| ProtocolError::Noise(format!("noise decrypt failed: {e}")))?;
        *nonce = nonce
            .checked_add(1)
            .ok_or_else(|| ProtocolError::Noise("nonce overflow".into()))?;

        if n == 0 {
            return Err(ProtocolError::Noise("empty noise chunk".into()));
        }

        let flag = plain[0];
        let payload = &plain[1..n];
        if out.len() + payload.len() > MAX_FRAME_LEN {
            return Err(ProtocolError::Framing(
                "reassembled message exceeds MAX_FRAME_LEN".into(),
            ));
        }
        out.extend_from_slice(payload);

        match flag {
            CHUNK_FLAG_MORE => continue,
            CHUNK_FLAG_FINAL => return Ok(out),
            other => {
                return Err(ProtocolError::Noise(format!(
                    "invalid chunk flag: {other}"
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::RootKeypair;

    #[tokio::test]
    async fn noise_handshake_and_large_split_transfer() {
        let root = RootKeypair::generate();
        let id_a = root.sign_device_cert(Uuid::new_v4(), "PC-A", 3600);
        let id_b = root.sign_device_cert(Uuid::new_v4(), "PC-B", 3600);
        let root_pub = root.pubkey_bytes();

        let (stream_a, stream_b) = tokio::io::duplex(64 * 1024);

        let (res_a, res_b) = tokio::join!(
            NoiseStream::handshake_initiator(stream_a, &root_pub, &id_a, &[]),
            NoiseStream::handshake_responder(stream_b, &root_pub, &id_b, &[]),
        );
        let ns_a = res_a.unwrap();
        let ns_b = res_b.unwrap();
        assert_eq!(ns_a.peer_cert().device_name, "PC-B");
        assert_eq!(ns_b.peer_cert().device_name, "PC-A");

        let (mut r_a, mut w_a, _) = ns_a.into_split();
        let (mut r_b, mut w_b, _) = ns_b.into_split();

        let big_a_to_b: Vec<u8> = (0..(310 * 1024)).map(|i| (i % 251) as u8).collect();
        let big_b_to_a: Vec<u8> = (0..(260 * 1024)).map(|i| ((i * 7) % 251) as u8).collect();

        let send_a = big_a_to_b.clone();
        let send_b = big_b_to_a.clone();

        let task_a_write = tokio::spawn(async move { w_a.send_msg(&send_a).await });
        let task_b_write = tokio::spawn(async move { w_b.send_msg(&send_b).await });
        let task_a_read = tokio::spawn(async move { r_a.recv_msg().await });
        let task_b_read = tokio::spawn(async move { r_b.recv_msg().await });

        task_a_write.await.unwrap().unwrap();
        task_b_write.await.unwrap().unwrap();
        let got_a = task_a_read.await.unwrap().unwrap();
        let got_b = task_b_read.await.unwrap().unwrap();

        assert_eq!(got_a, big_b_to_a);
        assert_eq!(got_b, big_a_to_b);
    }

    #[tokio::test]
    async fn noise_rejects_wrong_root_key() {
        let root_a = RootKeypair::generate();
        let root_b = RootKeypair::generate();
        let id_a = root_a.sign_device_cert(Uuid::new_v4(), "PC-A", 3600);
        let id_b = root_b.sign_device_cert(Uuid::new_v4(), "PC-B", 3600);

        let (stream_a, stream_b) = tokio::io::duplex(32 * 1024);
        let root_a_pub = root_a.pubkey_bytes();
        let root_b_pub = root_b.pubkey_bytes();

        let (res_a, res_b) = tokio::join!(
            NoiseStream::handshake_initiator(stream_a, &root_a_pub, &id_a, &[]),
            NoiseStream::handshake_responder(stream_b, &root_b_pub, &id_b, &[]),
        );
        assert!(res_a.is_err() || res_b.is_err());
    }
}
