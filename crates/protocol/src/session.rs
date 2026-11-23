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
