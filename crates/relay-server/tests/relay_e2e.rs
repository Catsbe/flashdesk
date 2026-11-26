use protocol::auth::{DeviceIdentity, RootKeypair};
use protocol::framing::{read_frame, write_frame};
use protocol::noise::NoiseStream;
use protocol::relay::{
    ChannelKind, ConnectionPurpose, PresenceClientMsg, PresenceServerMsg, RelayAuthRequest,
    RelayAuthResponse, RelayHello,
};
use protocol::session::{ControlMsg, InputEvent, MouseBtn, VideoCodec, VideoFrame};
use relay_server::server::run_server;
use relay_server::state::RelayState;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use uuid::Uuid;

async fn connect_and_auth_relay(
    addr: SocketAddr,
    identity: &DeviceIdentity,
    purpose: ConnectionPurpose,
) -> anyhow::Result<TcpStream> {
    let mut stream = TcpStream::connect(addr).await?;
    let _ = stream.set_nodelay(true);
    let raw_hello = read_frame(&mut stream).await?;
    let hello: RelayHello = bincode::deserialize(&raw_hello)?;

    let req = RelayAuthRequest {
        cert: identity.cert.clone(),
        challenge_sig: identity.sign_challenge(&hello.server_nonce, &purpose.canonical_bytes()),
        purpose,
    };
    write_frame(&mut stream, &bincode::serialize(&req)?).await?;

    let raw_resp = read_frame(&mut stream).await?;
    let resp: RelayAuthResponse = bincode::deserialize(&raw_resp)?;
    match resp {
        RelayAuthResponse::Ok => Ok(stream),
        RelayAuthResponse::Rejected(reason) => anyhow::bail!("relay rejected: {reason}"),
    }
}

#[tokio::test]
async fn relay_rejects_bad_root_and_revoked_device() {
    let root = RootKeypair::generate();
    let other_root = RootKeypair::generate();
    let revoked_id = Uuid::new_v4();
    let revoked_identity = root.sign_device_cert(revoked_id, "RevokedPC", 3600);
    let foreign_identity = other_root.sign_device_cert(Uuid::new_v4(), "ForeignPC", 3600);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(RelayState::new(
        root.pubkey_bytes(),
        vec![revoked_id],
        Duration::from_secs(5),
    ));
    let server_task = tokio::spawn(run_server(listener, state));

    let res_foreign =
        connect_and_auth_relay(addr, &foreign_identity, ConnectionPurpose::Presence).await;
    assert!(res_foreign.is_err());

    let res_revoked =
        connect_and_auth_relay(addr, &revoked_identity, ConnectionPurpose::Presence).await;
    assert!(res_revoked.is_err());

    server_task.abort();
}

#[tokio::test]
async fn relay_presence_and_noise_e2e_multi_channel_streaming() {
    let root = RootKeypair::generate();
    let host_id = Uuid::new_v4();
    let viewer_id = Uuid::new_v4();
    let host_ident = root.sign_device_cert(host_id, "HostPC", 3600);
    let viewer_ident = root.sign_device_cert(viewer_id, "ViewerPC", 3600);
    let root_pub = root.pubkey_bytes();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(RelayState::new(root_pub, vec![], Duration::from_secs(5)));
    let server_task = tokio::spawn(run_server(listener, state));

    // Host connects presence
    let mut host_presence =
        connect_and_auth_relay(addr, &host_ident, ConnectionPurpose::Presence)
            .await
            .unwrap();
    write_frame(
        &mut host_presence,
        &bincode::serialize(&PresenceClientMsg::UpdateInfo {
            monitor_count: 2,
            encode_codecs: vec![VideoCodec::Hevc, VideoCodec::H264],
            decode_codecs: vec![VideoCodec::Hevc, VideoCodec::H264],
            os: "Windows".into(),
            accept_incoming: true,
        })
        .unwrap(),
    )
    .await
    .unwrap();

    // Viewer connects presence and receives device list
    let mut viewer_presence =
        connect_and_auth_relay(addr, &viewer_ident, ConnectionPurpose::Presence)
            .await
            .unwrap();
    write_frame(
        &mut viewer_presence,
        &bincode::serialize(&PresenceClientMsg::ListDevices).unwrap(),
    )
    .await
    .unwrap();

    let mut saw_host = false;
    for _ in 0..5 {
        let raw = read_frame(&mut viewer_presence).await.unwrap();
        let msg: PresenceServerMsg = bincode::deserialize(&raw).unwrap();
        if let PresenceServerMsg::DeviceList(list) = msg {
            if list
                .iter()
                .any(|d| d.device_id == host_id && d.monitor_count == 2)
            {
                saw_host = true;
                break;
            }
        }
    }
    assert!(saw_host, "viewer should see HostPC in online device list");

    // Viewer opens Control & Video channels to HostPC
    let session_id = Uuid::new_v4();
    let open_ctrl = tokio::spawn({
        let ident = viewer_ident.clone();
        async move {
            let s = connect_and_auth_relay(
                addr,
                &ident,
                ConnectionPurpose::OpenSession {
                    target_device_id: host_id,
                    session_id,
                    channel: ChannelKind::Control,
                },
            )
            .await
            .unwrap();
            NoiseStream::handshake_initiator(s, &root_pub, &ident, &[])
                .await
                .unwrap()
        }
    });

    let open_video = tokio::spawn({
        let ident = viewer_ident.clone();
        async move {
            let s = connect_and_auth_relay(
                addr,
                &ident,
                ConnectionPurpose::OpenSession {
                    target_device_id: host_id,
                    session_id,
                    channel: ChannelKind::Video,
                },
            )
            .await
            .unwrap();
            NoiseStream::handshake_initiator(s, &root_pub, &ident, &[])
                .await
                .unwrap()
        }
    });

    // Host receives IncomingChannel notifications and accepts both
    let mut host_ctrl = None;
    let mut host_video = None;
    while host_ctrl.is_none() || host_video.is_none() {
        let raw = read_frame(&mut host_presence).await.unwrap();
        let msg: PresenceServerMsg = bincode::deserialize(&raw).unwrap();
        if let PresenceServerMsg::IncomingChannel {
            session_id: sid,
            channel,
            ..
        } = msg
        {
            assert_eq!(sid, session_id);
            let s = connect_and_auth_relay(
                addr,
                &host_ident,
                ConnectionPurpose::AcceptSession {
                    session_id: sid,
                    channel,
                },
            )
            .await
            .unwrap();
            let ns = NoiseStream::handshake_responder(s, &root_pub, &host_ident, &[])
                .await
                .unwrap();
            match channel {
                ChannelKind::Control => host_ctrl = Some(ns),
                ChannelKind::Video => host_video = Some(ns),
                _ => {}
            }
        }
    }

    let mut viewer_ctrl = open_ctrl.await.unwrap();
    let mut viewer_video = open_video.await.unwrap();
    let mut host_ctrl = host_ctrl.unwrap();
    let mut host_video = host_video.unwrap();

    // Verify encrypted Control message exchange
    let ping = ControlMsg::Ping {
        seq: 42,
        sent_us: 123456,
    };
    host_ctrl
        .send_msg(&bincode::serialize(&ping).unwrap())
        .await
        .unwrap();
    let got_ctrl: ControlMsg =
        bincode::deserialize(&viewer_ctrl.recv_msg().await.unwrap()).unwrap();
    match got_ctrl {
        ControlMsg::Ping { seq, sent_us } => {
            assert_eq!(seq, 42);
            assert_eq!(sent_us, 123456);
        }
        other => panic!("unexpected control msg: {other:?}"),
    }

    // Verify encrypted >100KB VideoFrame exchange across multiple Noise chunks
    let big_payload = vec![0xABu8; 135 * 1024];
    let frame = VideoFrame {
        frame_id: 7,
        timestamp_us: 999,
        codec: VideoCodec::Hevc,
        width: 1920,
        height: 1080,
        monitor_index: 0,
        is_keyframe: true,
        encode_us: 1800,
        encoder_name: "MF-HEVC-HW".into(),
        data: big_payload.clone(),
    };
    host_video
        .send_msg(&bincode::serialize(&frame).unwrap())
        .await
        .unwrap();
    let got_frame: VideoFrame =
        bincode::deserialize(&viewer_video.recv_msg().await.unwrap()).unwrap();
    assert_eq!(got_frame.frame_id, 7);
    assert_eq!(got_frame.codec, VideoCodec::Hevc);
    assert_eq!(got_frame.data, big_payload);

    server_task.abort();
}

#[tokio::test]
async fn relay_pair_tunnel_offloads_file_without_presence() {
    let root = RootKeypair::generate();
    let host_id = Uuid::new_v4();
    let viewer_id = Uuid::new_v4();
    let host_ident = root.sign_device_cert(host_id, "HostPC", 3600);
    let viewer_ident = root.sign_device_cert(viewer_id, "ViewerPC", 3600);
    let root_pub = root.pubkey_bytes();

    // Separate file-only relay node (neither device has Presence on it)
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let file_relay_addr = listener.local_addr().unwrap();
    let state = Arc::new(RelayState::new(root_pub, vec![], Duration::from_secs(5)));
    let server_task = tokio::spawn(run_server(listener, state));

    let transfer_session_id = Uuid::new_v4();

    let viewer_task = tokio::spawn({
        let ident = viewer_ident.clone();
        async move {
            let s = connect_and_auth_relay(
                file_relay_addr,
                &ident,
                ConnectionPurpose::PairTunnel {
                    session_id: transfer_session_id,
                    channel: ChannelKind::File,
                    peer_device_id: host_id,
                },
            )
            .await
            .unwrap();
            NoiseStream::handshake_initiator(s, &root_pub, &ident, &[])
                .await
                .unwrap()
        }
    });

    let host_task = tokio::spawn({
        let ident = host_ident.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let s = connect_and_auth_relay(
                file_relay_addr,
                &ident,
                ConnectionPurpose::PairTunnel {
                    session_id: transfer_session_id,
                    channel: ChannelKind::File,
                    peer_device_id: viewer_id,
                },
            )
            .await
            .unwrap();
            NoiseStream::handshake_responder(s, &root_pub, &ident, &[])
                .await
                .unwrap()
        }
    });

    let (viewer_file, host_file) = tokio::join!(viewer_task, host_task);
    let viewer_file = &mut viewer_file.unwrap();
    let host_file = &mut host_file.unwrap();

    // Transfer >1MB encrypted file chunk over offloaded PairTunnel
    let payload: Vec<u8> = (0..(1100 * 1024)).map(|i| (i % 247) as u8).collect();
    let send_fut = viewer_file.send_msg(&payload);
    let recv_fut = host_file.recv_msg();
    let (s_res, r_res) = tokio::join!(send_fut, recv_fut);
    s_res.unwrap();
    assert_eq!(r_res.unwrap(), payload);

    server_task.abort();
}

#[tokio::test]
async fn relay_coexistence_routes_incoming_to_service_over_gui() {
    let root = RootKeypair::generate();
    let shared_id = Uuid::new_v4();
    let caller_id = Uuid::new_v4();
    let shared_ident = root.sign_device_cert(shared_id, "SharedPC", 3600);
    let caller_ident = root.sign_device_cert(caller_id, "CallerPC", 3600);
    let root_pub = root.pubkey_bytes();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(RelayState::new(root_pub, vec![], Duration::from_secs(5)));
    let server_task = tokio::spawn(run_server(listener, state));

    // GUI connects first with accept_incoming = false
    let mut gui_presence =
        connect_and_auth_relay(addr, &shared_ident, ConnectionPurpose::Presence)
            .await
            .unwrap();
    write_frame(
        &mut gui_presence,
        &bincode::serialize(&PresenceClientMsg::UpdateInfo {
            monitor_count: 1,
            encode_codecs: vec![VideoCodec::H264],
            decode_codecs: vec![VideoCodec::H264],
            os: "Windows-GUI".into(),
            accept_incoming: false,
        })
        .unwrap(),
    )
    .await
    .unwrap();

    // SYSTEM Service connects second with accept_incoming = true
    let mut svc_presence =
        connect_and_auth_relay(addr, &shared_ident, ConnectionPurpose::Presence)
            .await
            .unwrap();
    write_frame(
        &mut svc_presence,
        &bincode::serialize(&PresenceClientMsg::UpdateInfo {
            monitor_count: 2,
            encode_codecs: vec![VideoCodec::Hevc],
            decode_codecs: vec![VideoCodec::Hevc],
            os: "Windows-Service".into(),
            accept_incoming: true,
        })
        .unwrap(),
    )
    .await
    .unwrap();

    // Caller sees deduplicated single SharedPC entry preferring Service metadata
    let mut caller_presence =
        connect_and_auth_relay(addr, &caller_ident, ConnectionPurpose::Presence)
            .await
            .unwrap();
    write_frame(
        &mut caller_presence,
        &bincode::serialize(&PresenceClientMsg::ListDevices).unwrap(),
    )
    .await
    .unwrap();

    let mut checked_list = false;
    for _ in 0..6 {
        let raw = read_frame(&mut caller_presence).await.unwrap();
        if let PresenceServerMsg::DeviceList(list) = bincode::deserialize(&raw).unwrap() {
            let matches: Vec<_> = list.iter().filter(|d| d.device_id == shared_id).collect();
            if matches.len() == 1 && matches[0].os == "Windows-Service" {
                checked_list = true;
                break;
            }
        }
    }
    assert!(checked_list, "should deduplicate to Service entry");

    // Caller opens session -> IncomingChannel must arrive on Service, not GUI
    let session_id = Uuid::new_v4();
    let _open = tokio::spawn({
        let ident = caller_ident.clone();
        async move {
            let _ = connect_and_auth_relay(
                addr,
                &ident,
                ConnectionPurpose::OpenSession {
                    target_device_id: shared_id,
                    session_id,
                    channel: ChannelKind::Control,
                },
            )
            .await;
        }
    });

    let mut svc_got_invite = false;
    for _ in 0..5 {
        let raw = read_frame(&mut svc_presence).await.unwrap();
        if let PresenceServerMsg::IncomingChannel {
            session_id: sid, ..
        } = bincode::deserialize(&raw).unwrap()
        {
            assert_eq!(sid, session_id);
            svc_got_invite = true;
            break;
        }
    }
    assert!(svc_got_invite, "Service must receive IncomingChannel");

    // GUI should NOT receive IncomingChannel
    let gui_check = tokio::time::timeout(Duration::from_millis(120), async {
        while let Ok(raw) = read_frame(&mut gui_presence).await {
            if let Ok(PresenceServerMsg::IncomingChannel { .. }) = bincode::deserialize(&raw) {
                return true;
            }
        }
        false
    })
    .await;
    assert_ne!(gui_check, Ok(true), "GUI must not receive IncomingChannel when accept_incoming=false");

    server_task.abort();
}

#[tokio::test]
async fn relay_multiple_reconnects_and_rapid_bidirectional_control() {
    use std::collections::HashMap;
    use tokio::sync::mpsc;

    let root = RootKeypair::generate();
    let host_id = Uuid::new_v4();
    let viewer_id = Uuid::new_v4();
    let host_ident = root.sign_device_cert(host_id, "HostPC", 3600);
    let viewer_ident = root.sign_device_cert(viewer_id, "ViewerPC", 3600);
    let root_pub = root.pubkey_bytes();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(RelayState::new(root_pub, vec![], Duration::from_secs(5)));
    let server_task = tokio::spawn(run_server(listener, state));

    // Spawn a cancel-safe host presence handler mimicking desk-app's relay_client.rs
    let hp = connect_and_auth_relay(addr, &host_ident, ConnectionPurpose::Presence)
        .await
        .unwrap();
    let (mut hp_reader, mut hp_writer) = hp.into_split();
    write_frame(
        &mut hp_writer,
        &bincode::serialize(&PresenceClientMsg::UpdateInfo {
            monitor_count: 1,
            encode_codecs: vec![VideoCodec::H264],
            decode_codecs: vec![VideoCodec::H264],
            os: "Windows".into(),
            accept_incoming: true,
        })
        .unwrap(),
    )
    .await
    .unwrap();

    let (hp_tx, mut hp_rx) = mpsc::channel::<Vec<u8>>(64);
    let hp_reader_task = tokio::spawn(async move {
        while let Ok(raw) = read_frame(&mut hp_reader).await {
            if hp_tx.send(raw).await.is_err() {
                break;
            }
        }
    });

    // Helper task accepting paired sessions on host_presence #1
    let (sess_tx, mut sess_rx) = mpsc::channel::<(NoiseStream<TcpStream>, NoiseStream<TcpStream>)>(8);
    let host_accept_task = tokio::spawn({
        let ident = host_ident.clone();
        async move {
            let mut pending: HashMap<Uuid, (Option<NoiseStream<TcpStream>>, Option<NoiseStream<TcpStream>>)> =
                HashMap::new();
            let mut hb_tick = tokio::time::interval(Duration::from_millis(5));
            let mut hb_seq = 0u64;
            let (chan_tx, mut chan_rx) = mpsc::channel::<(Uuid, ChannelKind, NoiseStream<TcpStream>)>(16);

            loop {
                tokio::select! {
                    _ = hb_tick.tick() => {
                        hb_seq += 1;
                        if let Ok(b) = bincode::serialize(&PresenceClientMsg::Heartbeat(hb_seq)) {
                            if write_frame(&mut hp_writer, &b).await.is_err() {
                                break;
                            }
                        }
                    }
                    Some(raw) = hp_rx.recv() => {
                        if let Ok(PresenceServerMsg::IncomingChannel { session_id, channel, .. }) =
                            bincode::deserialize(&raw)
                        {
                            let c_tx = chan_tx.clone();
                            let id_clone = ident.clone();
                            tokio::spawn(async move {
                                let tcp = connect_and_auth_relay(
                                    addr,
                                    &id_clone,
                                    ConnectionPurpose::AcceptSession { session_id, channel },
                                )
                                .await
                                .unwrap();
                                let ns = NoiseStream::handshake_responder(tcp, &root_pub, &id_clone, &[])
                                    .await
                                    .unwrap();
                                let _ = c_tx.send((session_id, channel, ns)).await;
                            });
                        }
                    }
                    Some((sid, ch, ns)) = chan_rx.recv() => {
                        let slot = pending.entry(sid).or_insert((None, None));
                        match ch {
                            ChannelKind::Control => slot.0 = Some(ns),
                            ChannelKind::Video => slot.1 = Some(ns),
                            _ => {}
                        }
                        if slot.0.is_some() && slot.1.is_some() {
                            let (c, v) = pending.remove(&sid).unwrap();
                            if sess_tx.send((c.unwrap(), v.unwrap())).await.is_err() {
                                break;
                            }
                        }
                    }
                    else => break,
                }
            }
        }
    });

    // Run 3 full connect -> 150 bidirectional inputs + video -> disconnect -> reconnect cycles
    for cycle in 0..3u64 {
        let session_id = Uuid::new_v4();
        let open_ctrl = tokio::spawn({
            let ident = viewer_ident.clone();
            async move {
                let s = connect_and_auth_relay(
                    addr,
                    &ident,
                    ConnectionPurpose::OpenSession {
                        target_device_id: host_id,
                        session_id,
                        channel: ChannelKind::Control,
                    },
                )
                .await
                .unwrap();
                NoiseStream::handshake_initiator(s, &root_pub, &ident, &[])
                    .await
                    .unwrap()
            }
        });
        let open_video = tokio::spawn({
            let ident = viewer_ident.clone();
            async move {
                let s = connect_and_auth_relay(
                    addr,
                    &ident,
                    ConnectionPurpose::OpenSession {
                        target_device_id: host_id,
                        session_id,
                        channel: ChannelKind::Video,
                    },
                )
                .await
                .unwrap();
                NoiseStream::handshake_initiator(s, &root_pub, &ident, &[])
                    .await
                    .unwrap()
            }
        });

        let ((mut h_ctrl, mut h_video), (v_ctrl_res, v_video_res)) = tokio::join!(
            async { sess_rx.recv().await.unwrap() },
            async { tokio::join!(open_ctrl, open_video) }
        );
        let mut v_ctrl = v_ctrl_res.unwrap();
        let mut v_video = v_video_res.unwrap();

        // Handshake Hello / StartStream
        h_ctrl
            .send_msg(
                &bincode::serialize(&ControlMsg::Hello {
                    encode_codecs: vec![VideoCodec::H264],
                    decode_codecs: vec![VideoCodec::H264],
                    monitors: vec![],
                    active_monitor: 0,
                })
                .unwrap(),
            )
            .await
            .unwrap();
        let _: ControlMsg = bincode::deserialize(&v_ctrl.recv_msg().await.unwrap()).unwrap();
        v_ctrl
            .send_msg(
                &bincode::serialize(&ControlMsg::StartStream {
                    codec: VideoCodec::H264,
                    target_fps: 60,
                    bitrate_kbps: 2500,
                    monitor_index: 0,
                })
                .unwrap(),
            )
            .await
            .unwrap();
        let _: ControlMsg = bincode::deserialize(&h_ctrl.recv_msg().await.unwrap()).unwrap();

        let (mut h_cr, mut h_cw, _) = h_ctrl.into_split();
        let (mut v_cr, mut v_cw, _) = v_ctrl.into_split();

        // Rapid bidirectional stress (150 inputs + 150 pongs + 3 video frames) using cancel-safe readers
        let host_io = tokio::spawn(async move {
            let (in_tx, mut in_rx) = mpsc::channel::<Vec<u8>>(256);
            let r_task = tokio::spawn(async move {
                while let Ok(raw) = h_cr.recv_msg().await {
                    if in_tx.send(raw).await.is_err() {
                        break;
                    }
                }
            });

            let send_pongs = tokio::spawn(async move {
                for seq in 0..150u64 {
                    let msg = ControlMsg::Pong {
                        seq: cycle * 1000 + seq,
                        sent_us: seq,
                        encode_us: 100,
                    };
                    h_cw.send_msg(&bincode::serialize(&msg).unwrap()).await.unwrap();
                }
            });

            let mut received_inputs = 0u64;
            let mut ticker = tokio::time::interval(Duration::from_millis(1));
            while received_inputs < 150 {
                tokio::select! {
                    _ = ticker.tick() => {}
                    opt = in_rx.recv() => {
                        let raw = opt.expect("host control channel closed prematurely");
                        let msg: ControlMsg = bincode::deserialize(&raw).unwrap();
                        if matches!(msg, ControlMsg::Input(_)) {
                            received_inputs += 1;
                        }
                    }
                }
            }
            send_pongs.await.unwrap();
            r_task.abort();
            received_inputs
        });

        let viewer_io = tokio::spawn(async move {
            let (in_tx, mut in_rx) = mpsc::channel::<Vec<u8>>(256);
            let r_task = tokio::spawn(async move {
                while let Ok(raw) = v_cr.recv_msg().await {
                    if in_tx.send(raw).await.is_err() {
                        break;
                    }
                }
            });

            let send_inputs = tokio::spawn(async move {
                for idx in 0..150u32 {
                    let ev = if idx % 3 == 0 {
                        InputEvent::MouseButton {
                            button: MouseBtn::Left,
                            pressed: idx % 2 == 0,
                        }
                    } else if idx % 3 == 1 {
                        InputEvent::Key {
                            vk: 0x41,
                            scan_code: 0x1E,
                            extended: false,
                            pressed: true,
                        }
                    } else {
                        InputEvent::MouseMove {
                            norm_x: (idx as f32) / 150.0,
                            norm_y: 0.5,
                        }
                    };
                    v_cw.send_msg(&bincode::serialize(&ControlMsg::Input(ev)).unwrap())
                        .await
                        .unwrap();
                }
            });

            let mut pongs = 0u64;
            let mut ticker = tokio::time::interval(Duration::from_millis(1));
            while pongs < 150 {
                tokio::select! {
                    _ = ticker.tick() => {}
                    opt = in_rx.recv() => {
                        let raw = opt.expect("viewer control channel closed prematurely");
                        if let ControlMsg::Pong { seq, .. } = bincode::deserialize(&raw).unwrap() {
                            assert_eq!(seq, cycle * 1000 + pongs);
                            pongs += 1;
                        }
                    }
                }
            }
            send_inputs.await.unwrap();
            r_task.abort();
            pongs
        });

        for fid in 1..=3u64 {
            let vf = VideoFrame {
                frame_id: fid,
                timestamp_us: fid * 16_667,
                codec: VideoCodec::H264,
                width: 1280,
                height: 720,
                monitor_index: 0,
                is_keyframe: fid == 1,
                encode_us: 1000,
                encoder_name: "Test".into(),
                data: vec![0xAA; 16 * 1024],
            };
            h_video.send_msg(&bincode::serialize(&vf).unwrap()).await.unwrap();
            let got: VideoFrame = bincode::deserialize(&v_video.recv_msg().await.unwrap()).unwrap();
            assert_eq!(got.frame_id, fid);
        }

        let (got_inputs, got_pongs) = tokio::join!(host_io, viewer_io);
        assert_eq!(got_inputs.unwrap(), 150, "cycle {cycle} lost inputs");
        assert_eq!(got_pongs.unwrap(), 150, "cycle {cycle} lost pongs");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Stale-socket test: open a 2nd newest Host Presence while the 1st stays open/idle;
    // session #4 MUST route to the newest Presence socket cleanly.
    let mut hp_new = connect_and_auth_relay(addr, &host_ident, ConnectionPurpose::Presence)
        .await
        .unwrap();
    write_frame(
        &mut hp_new,
        &bincode::serialize(&PresenceClientMsg::UpdateInfo {
            monitor_count: 2,
            encode_codecs: vec![VideoCodec::H264],
            decode_codecs: vec![VideoCodec::H264],
            os: "Windows-New".into(),
            accept_incoming: true,
        })
        .unwrap(),
    )
    .await
    .unwrap();

    // Wait briefly for relay UpdateInfo broadcast to apply
    tokio::time::sleep(Duration::from_millis(30)).await;

    let session4 = Uuid::new_v4();
    let _open4 = tokio::spawn({
        let ident = viewer_ident.clone();
        async move {
            let _ = connect_and_auth_relay(
                addr,
                &ident,
                ConnectionPurpose::OpenSession {
                    target_device_id: host_id,
                    session_id: session4,
                    channel: ChannelKind::Control,
                },
            )
            .await;
        }
    });

    let mut routed_to_newest = false;
    for _ in 0..8 {
        let raw = tokio::time::timeout(Duration::from_secs(2), read_frame(&mut hp_new))
            .await
            .unwrap()
            .unwrap();
        if let PresenceServerMsg::IncomingChannel { session_id, .. } =
            bincode::deserialize(&raw).unwrap()
        {
            assert_eq!(session_id, session4);
            routed_to_newest = true;
            break;
        }
    }
    assert!(routed_to_newest, "IncomingChannel must route to newest active Host socket");

    hp_reader_task.abort();
    host_accept_task.abort();
    server_task.abort();
}

#[tokio::test]
async fn relay_remote_revocation_kicks_peer_and_persists() {
    let root = RootKeypair::generate();
    let other_root = RootKeypair::generate();
    let admin_id = Uuid::new_v4();
    let victim_id = Uuid::new_v4();
    let admin_ident = root.sign_device_cert(admin_id, "AdminPC", 3600);
    let victim_ident = root.sign_device_cert(victim_id, "VictimPlaceholder", 3600);
    let root_pub = root.pubkey_bytes();

    let state_dir = std::env::temp_dir().join(format!("flashdesk-relay-test-{}", Uuid::new_v4()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(RelayState::new_with_state_dir(
        root_pub,
        vec![],
        Duration::from_secs(5),
        Some(state_dir.clone()),
    ));
    let server_task = tokio::spawn(run_server(listener, Arc::clone(&state)));

    let mut admin_ws = connect_and_auth_relay(addr, &admin_ident, ConnectionPurpose::Presence)
        .await
        .unwrap();
    let mut victim_ws = connect_and_auth_relay(addr, &victim_ident, ConnectionPurpose::Presence)
        .await
        .unwrap();

    // Victim updates its display name dynamically via SetDisplayName.
    write_frame(
        &mut victim_ws,
        &bincode::serialize(&PresenceClientMsg::SetDisplayName("MyActualLaptop".into())).unwrap(),
    )
    .await
    .unwrap();

    let mut saw_renamed_victim = false;
    for _ in 0..8 {
        let raw = tokio::time::timeout(Duration::from_secs(2), read_frame(&mut admin_ws))
            .await
            .unwrap()
            .unwrap();
        if let PresenceServerMsg::DeviceList(list) = bincode::deserialize(&raw).unwrap() {
            if list
                .iter()
                .any(|d| d.device_id == victim_id && d.device_name == "MyActualLaptop")
            {
                saw_renamed_victim = true;
                break;
            }
        }
    }
    assert!(saw_renamed_victim, "SetDisplayName should update DeviceList");

    // A revocation signed by another root key must be rejected.
    let bad_cmd = other_root.sign_revocation_list(vec![victim_id]);
    write_frame(
        &mut admin_ws,
        &bincode::serialize(&PresenceClientMsg::SyncRevocations(bad_cmd)).unwrap(),
    )
    .await
    .unwrap();
    let mut saw_error = false;
    for _ in 0..5 {
        let raw = tokio::time::timeout(Duration::from_secs(2), read_frame(&mut admin_ws))
            .await
            .unwrap()
            .unwrap();
        if let PresenceServerMsg::Error(_) = bincode::deserialize(&raw).unwrap() {
            saw_error = true;
            break;
        }
    }
    assert!(saw_error, "wrong-root revocation command must be rejected");

    // Valid root-signed revocation kicks victim immediately and persists to state_dir/revoked.json.
    let good_cmd = root.sign_revocation_list(vec![victim_id]);
    write_frame(
        &mut admin_ws,
        &bincode::serialize(&PresenceClientMsg::SyncRevocations(good_cmd)).unwrap(),
    )
    .await
    .unwrap();

    let mut saw_synced = false;
    for _ in 0..5 {
        let raw = tokio::time::timeout(Duration::from_secs(2), read_frame(&mut admin_ws))
            .await
            .unwrap()
            .unwrap();
        if let PresenceServerMsg::RevocationsSynced { count, .. } =
            bincode::deserialize(&raw).unwrap()
        {
            assert_eq!(count, 1);
            saw_synced = true;
            break;
        }
    }
    assert!(saw_synced, "admin should receive RevocationsSynced");

    // Victim connection must close promptly after kick.
    let kicked = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if read_frame(&mut victim_ws).await.is_err() {
                return true;
            }
        }
    })
    .await;
    assert_eq!(kicked, Ok(true), "revoked online peer must be kicked");

    // Reconnecting as victim is rejected.
    assert!(
        connect_and_auth_relay(addr, &victim_ident, ConnectionPurpose::Presence)
            .await
            .is_err()
    );

    server_task.abort();

    // Reloading state from the same state_dir preserves the revocation across relay restarts.
    let reloaded = RelayState::new_with_state_dir(
        root_pub,
        vec![],
        Duration::from_secs(5),
        Some(state_dir.clone()),
    );
    assert!(reloaded.is_revoked(&victim_id).await);

    let _ = std::fs::remove_dir_all(&state_dir);
}

#[tokio::test]
async fn relay_update_publish_download_and_remote_trigger() {
    use protocol::patch::{apply_delta_patch, compress_full, create_delta_patch, MAX_UNCOMPRESSED_SIZE};
    use protocol::update::{
        sha256_bytes, DeltaPatchInfo, FullPackageInfo, PublishFileSpec, UpdateClientMsg,
        UpdateManifest, UpdateServerMsg,
    };
    use protocol::unix_timestamp_secs;

    let root = RootKeypair::generate();
    let other_root = RootKeypair::generate();
    let admin_id = Uuid::new_v4();
    let peer_id = Uuid::new_v4();
    let admin_ident = root.sign_device_cert(admin_id, "AdminPC", 3600);
    let peer_ident = root.sign_device_cert(peer_id, "RemoteHost", 3600);
    let root_pub = root.pubkey_bytes();

    let state_dir = std::env::temp_dir().join(format!("flashdesk-upd-e2e-{}", Uuid::new_v4()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(RelayState::new_with_state_dir(
        root_pub,
        vec![],
        Duration::from_secs(5),
        Some(state_dir.clone()),
    ));
    let server_task = tokio::spawn(run_server(listener, Arc::clone(&state)));

    // Prepare v1 -> v2 binaries, full deflate, and FLDP delta patch.
    let old_bin = b"FLASHDESK-OLD-BINARY-0123456789".repeat(512);
    let mut new_bin = old_bin.clone();
    new_bin[4096..4128].fill(0x5A);
    let full_z = compress_full(&new_bin).unwrap();
    let delta = create_delta_patch(&old_bin, &new_bin).unwrap();

    let full_name = "flashdesk-core-0.2.0.exe.deflate".to_string();
    let delta_name = "flashdesk-0.1.0-to-0.2.0.fldp".to_string();
    let manifest = UpdateManifest {
        format_version: 1,
        version: "0.2.0".into(),
        min_base_version: "1.0.0".into(),
        released_at: unix_timestamp_secs(),
        notes: "e2e test release".into(),
        full: FullPackageInfo {
            filename: full_name.clone(),
            size: full_z.len() as u64,
            sha256: sha256_bytes(&full_z),
            target_size: new_bin.len() as u64,
            target_sha256: sha256_bytes(&new_bin),
        },
        deltas: vec![DeltaPatchInfo {
            from_version: "0.1.0".into(),
            from_sha256: sha256_bytes(&old_bin),
            filename: delta_name.clone(),
            size: delta.len() as u64,
            sha256: sha256_bytes(&delta),
        }],
    };
    let files = vec![
        PublishFileSpec {
            filename: full_name.clone(),
            size: full_z.len() as u64,
            sha256: sha256_bytes(&full_z),
        },
        PublishFileSpec {
            filename: delta_name.clone(),
            size: delta.len() as u64,
            sha256: sha256_bytes(&delta),
        },
    ];

    // 1) Foreign-signed manifest must be rejected by the relay even from an authenticated client.
    let mut upd_conn = connect_and_auth_relay(addr, &admin_ident, ConnectionPurpose::UpdateChannel)
        .await
        .unwrap();
    let bad_signed = other_root.sign_update_manifest(manifest.clone());
    write_frame(
        &mut upd_conn,
        &bincode::serialize(&UpdateClientMsg::BeginPublish {
            manifest: bad_signed,
            files: files.clone(),
        })
        .unwrap(),
    )
    .await
    .unwrap();
    let resp: UpdateServerMsg = bincode::deserialize(&read_frame(&mut upd_conn).await.unwrap()).unwrap();
    assert!(matches!(resp, UpdateServerMsg::Error(_)));

    // 2) Valid root-signed BeginPublish -> UploadChunk -> CommitPublish succeeds.
    let good_signed = root.sign_update_manifest(manifest.clone());
    write_frame(
        &mut upd_conn,
        &bincode::serialize(&UpdateClientMsg::BeginPublish {
            manifest: good_signed,
            files,
        })
        .unwrap(),
    )
    .await
    .unwrap();
    let resp: UpdateServerMsg = bincode::deserialize(&read_frame(&mut upd_conn).await.unwrap()).unwrap();
    assert!(matches!(resp, UpdateServerMsg::PublishReady));

    for (name, bytes) in [(&full_name, &full_z), (&delta_name, &delta)] {
        write_frame(
            &mut upd_conn,
            &bincode::serialize(&UpdateClientMsg::UploadChunk {
                filename: name.clone(),
                offset: 0,
                data: bytes.clone(),
                eof: true,
            })
            .unwrap(),
        )
        .await
        .unwrap();
        let ack: UpdateServerMsg = bincode::deserialize(&read_frame(&mut upd_conn).await.unwrap()).unwrap();
        assert!(matches!(ack, UpdateServerMsg::UploadAck { .. }));
    }

    write_frame(
        &mut upd_conn,
        &bincode::serialize(&UpdateClientMsg::CommitPublish).unwrap(),
    )
    .await
    .unwrap();
    let committed: UpdateServerMsg = bincode::deserialize(&read_frame(&mut upd_conn).await.unwrap()).unwrap();
    assert!(matches!(
        committed,
        UpdateServerMsg::PublishCommitted { files: 2, .. }
    ));

    // 3) Peer downloads manifest & delta chunk, verifies signature and applies patch.
    let mut dl_conn = connect_and_auth_relay(addr, &peer_ident, ConnectionPurpose::UpdateChannel)
        .await
        .unwrap();
    write_frame(&mut dl_conn, &bincode::serialize(&UpdateClientMsg::GetManifest).unwrap())
        .await
        .unwrap();
    let got_m = match bincode::deserialize::<UpdateServerMsg>(&read_frame(&mut dl_conn).await.unwrap()).unwrap() {
        UpdateServerMsg::Manifest(Some(m)) => m,
        other => panic!("unexpected manifest response: {other:?}"),
    };
    got_m.verify(&root_pub, unix_timestamp_secs()).unwrap();
    assert_eq!(got_m.manifest.version, "0.2.0");

    write_frame(
        &mut dl_conn,
        &bincode::serialize(&UpdateClientMsg::DownloadChunk {
            filename: delta_name,
            offset: 0,
            max_len: 1024 * 1024,
        })
        .unwrap(),
    )
    .await
    .unwrap();
    let dl_delta = match bincode::deserialize::<UpdateServerMsg>(&read_frame(&mut dl_conn).await.unwrap()).unwrap() {
        UpdateServerMsg::ChunkData { data, eof: true, .. } => data,
        other => panic!("unexpected chunk response: {other:?}"),
    };
    assert_eq!(sha256_bytes(&dl_delta), got_m.manifest.deltas[0].sha256);
    let patched = apply_delta_patch(&old_bin, &dl_delta, MAX_UNCOMPRESSED_SIZE).unwrap();
    assert_eq!(patched, new_bin);

    // 4) Remote update check trigger via Presence delivers CheckUpdateNow to target peer.
    let mut admin_pres = connect_and_auth_relay(addr, &admin_ident, ConnectionPurpose::Presence)
        .await
        .unwrap();
    let mut peer_pres = connect_and_auth_relay(addr, &peer_ident, ConnectionPurpose::Presence)
        .await
        .unwrap();
    write_frame(
        &mut peer_pres,
        &bincode::serialize(&PresenceClientMsg::UpdateInfo {
            monitor_count: 1,
            encode_codecs: vec![],
            decode_codecs: vec![],
            os: "Windows".into(),
            accept_incoming: true,
        })
        .unwrap(),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;

    write_frame(
        &mut admin_pres,
        &bincode::serialize(&PresenceClientMsg::RequestPeerUpdateCheck {
            target_device_id: peer_id,
        })
        .unwrap(),
    )
    .await
    .unwrap();

    let mut saw_trigger = false;
    for _ in 0..8 {
        let raw = tokio::time::timeout(Duration::from_secs(2), read_frame(&mut peer_pres))
            .await
            .unwrap()
            .unwrap();
        if let PresenceServerMsg::CheckUpdateNow { from_device_id, .. } =
            bincode::deserialize(&raw).unwrap()
        {
            assert_eq!(from_device_id, admin_id);
            saw_trigger = true;
            break;
        }
    }
    assert!(saw_trigger, "target peer should receive CheckUpdateNow");

    server_task.abort();
    let _ = std::fs::remove_dir_all(&state_dir);
}
