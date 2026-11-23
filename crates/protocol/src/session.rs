use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VideoCodec {
    Hevc,
    H264,
}

impl VideoCodec {
    pub fn label(&self) -> &'static str {
        match self {
            VideoCodec::Hevc => "H.265/HEVC",
            VideoCodec::H264 => "H.264/AVC",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub index: u32,
    pub name: String,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    pub is_primary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseBtn {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    MouseMove {
        norm_x: f32,
        norm_y: f32,
    },
    MouseButton {
        button: MouseBtn,
        pressed: bool,
    },
    MouseWheel {
        delta_x: i16,
        delta_y: i16,
    },
    Key {
        vk: u16,
        scan_code: u16,
        extended: bool,
        pressed: bool,
    },
    SendSas,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClipboardData {
    Text(String),
    ImagePng(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VddDriverKind {
    None,
    ParsecVdd,
    VirtualDisplayDriver,
    ThirdParty(String),
}

impl VddDriverKind {
    pub fn label(&self) -> String {
        match self {
            VddDriverKind::None => "未安装虚拟屏驱动".into(),
            VddDriverKind::ParsecVdd => "Parsec VDD".into(),
            VddDriverKind::VirtualDisplayDriver => "Virtual-Display-Driver".into(),
            VddDriverKind::ThirdParty(s) => s.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VddInfo {
    pub driver: VddDriverKind,
    pub active_virtual_displays: u32,
    pub details: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VddRequest {
    Query,
    AddDisplay {
        width: u32,
        height: u32,
        refresh_hz: u32,
    },
    RemoveDisplay,
    SetMode {
        device_name: String,
        width: u32,
        height: u32,
        refresh_hz: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlMsg {
    Hello {
        encode_codecs: Vec<VideoCodec>,
        decode_codecs: Vec<VideoCodec>,
        monitors: Vec<MonitorInfo>,
        active_monitor: u32,
    },
    StartStream {
        codec: VideoCodec,
        target_fps: u32,
        bitrate_kbps: u32,
        monitor_index: u32,
    },
    UpdateStream {
        target_fps: u32,
        bitrate_kbps: u32,
        monitor_index: u32,
    },
    RequestKeyframe,
    Input(InputEvent),
    Ping {
        seq: u64,
        sent_us: u64,
    },
    Pong {
        seq: u64,
        sent_us: u64,
        encode_us: u32,
    },
    Notice(String),
    Clipboard(ClipboardData),
    OpenFileChannel {
        transfer_session_id: Uuid,
        relay_addr: Option<String>,
    },
    VddAction(VddRequest),
    VddStatus(VddInfo),
    MonitorsUpdated(Vec<MonitorInfo>),
    // ---- Flow control / ABR (appended 2026-11) ----
    // bincode encodes the variant *index*: new variants must only ever be appended here, never
    // inserted above, or mixed-version sessions break. Older peers fail to decode these and simply
    // skip them (both session loops ignore undecodable control messages).
    /// Viewer -> host, sent on the *reverse direction of the video channel* (so it never queues
    /// behind clipboard/input traffic on the control channel). Cumulative: every frame with an id
    /// `<= frame_id` has been received and decoded. `viewer_us` is the time the acknowledged frame
    /// spent on the viewer (network receipt -> decoded + converted), which lets the host tell
    /// network queueing apart from a slow viewer decoder.
    FrameAck {
        frame_id: u64,
        viewer_us: u32,
    },
    /// Viewer -> host: cap the streamed resolution, orientation-agnostic (`max_long` bounds the
    /// longer side, `max_short` the shorter one; `0, 0` = native). The host downscales before
    /// encoding.
    SetResolutionCap {
        max_long: u32,
        max_short: u32,
    },
    /// Host -> viewer: flow-control / ABR telemetry, sent right after `StartStream` (which also
    /// tells the viewer that the host understands `FrameAck`) and then once per second.
    HostStats(HostStreamStats),
}

/// Once-per-second host-side streaming telemetry shown in the viewer HUD.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostStreamStats {
    /// Bitrate currently requested from the encoder by the ABR controller.
    pub target_kbps: u32,
    /// Bitrate actually produced by the encoder over the last window.
    pub sent_kbps: u32,
    /// Frames handed to the network over the last window.
    pub sent_fps: f32,
    /// Mean / max time from frame hand-off on the host to "decoded" ack from the viewer.
    pub avg_delay_ms: u32,
    pub max_delay_ms: u32,
    /// Capture ticks skipped by flow control (network or viewer decoder behind).
    pub paused_ticks: u32,
    /// Capture ticks skipped because the local network writer was still busy.
    pub sender_busy_ticks: u32,
    /// Mean duration of a frame-producing capture call on the host: waiting for the next desktop
    /// update + BGRA->NV12 (+ downscale).
    pub capture_ms: f32,
    /// Mean encoder call time.
    pub encode_ms: f32,
    /// Frames captured (encoder calls) in the window; compare with `sent_fps` to spot frames the
    /// encoder swallowed (empty output).
    pub captured_frames: u32,
    /// Encoded stream resolution (after any resolution cap).
    pub stream_width: u32,
    pub stream_height: u32,
    /// True once the viewer's frame acks are driving the latency gate and ABR.
    pub flow_control: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variant_index(msg: &ControlMsg) -> u32 {
        let bytes = bincode::serialize(msg).unwrap();
        u32::from_le_bytes(bytes[..4].try_into().unwrap())
    }

    /// Mixed-version sessions depend on existing variant indices never moving.
    #[test]
    fn control_msg_variant_indices_are_stable() {
        assert_eq!(variant_index(&ControlMsg::RequestKeyframe), 3);
        assert_eq!(
            variant_index(&ControlMsg::Ping {
                seq: 1,
                sent_us: 2
            }),
            5
        );
        assert_eq!(variant_index(&ControlMsg::Notice(String::new())), 7);
        assert_eq!(variant_index(&ControlMsg::MonitorsUpdated(Vec::new())), 12);
        assert_eq!(
            variant_index(&ControlMsg::FrameAck {
                frame_id: 9,
                viewer_us: 1
            }),
            13
        );
        assert_eq!(
            variant_index(&ControlMsg::SetResolutionCap {
                max_long: 1920,
                max_short: 1080
            }),
            14
        );
        assert_eq!(
            variant_index(&ControlMsg::HostStats(HostStreamStats::default())),
            15
        );
    }

    #[test]
    fn new_messages_roundtrip() {
        let stats = HostStreamStats {
            target_kbps: 4200,
            sent_kbps: 3900,
            sent_fps: 59.8,
            avg_delay_ms: 48,
            max_delay_ms: 95,
            paused_ticks: 3,
            sender_busy_ticks: 1,
            capture_ms: 4.5,
            encode_ms: 3.25,
            captured_frames: 60,
            stream_width: 1920,
            stream_height: 1080,
            flow_control: true,
        };
        let raw = bincode::serialize(&ControlMsg::HostStats(stats.clone())).unwrap();
        match bincode::deserialize::<ControlMsg>(&raw).unwrap() {
            ControlMsg::HostStats(got) => assert_eq!(got, stats),
            other => panic!("unexpected {other:?}"),
        }
        let raw = bincode::serialize(&ControlMsg::FrameAck {
            frame_id: u64::MAX,
            viewer_us: 1234,
        })
        .unwrap();
        assert!(matches!(
            bincode::deserialize::<ControlMsg>(&raw).unwrap(),
            ControlMsg::FrameAck {
                frame_id: u64::MAX,
                viewer_us: 1234
            }
        ));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoFrame {
    pub frame_id: u64,
    pub timestamp_us: u64,
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub monitor_index: u32,
    pub is_keyframe: bool,
    pub encode_us: u32,
    pub encoder_name: String,
    pub data: Vec<u8>,
}
