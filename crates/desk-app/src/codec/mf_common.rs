use anyhow::{Context, Result};
use std::sync::OnceLock;
use windows::core::{GUID, PWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMFMediaBuffer, IMFMediaEvent, IMFMediaEventGenerator, IMFSample, MFCreateMemoryBuffer,
    MFCreateSample, MFStartup, MFSTARTUP_NOSOCKET, MF_EVENT_FLAG_NO_WAIT, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED,
};

static MF_INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();

pub fn ensure_mf_startup() -> Result<()> {
    let res = MF_INIT.get_or_init(|| unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).map_err(|e| format!("MFStartup failed: {e}"))
    });
    match res {
        Ok(()) => Ok(()),
        Err(e) => anyhow::bail!("{e}"),
    }
}

pub fn ensure_com_mta() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

pub fn poll_event_until(
    event_gen: &IMFMediaEventGenerator,
    deadline: std::time::Instant,
) -> Result<Option<IMFMediaEvent>> {
    // Hardware MFTs usually answer within a few ms: yield-poll for the first 2 ms (a 1 ms sleep
    // can last a whole 15 ms OS tick on coarse-timer systems), then back off to 1 ms sleeps.
    let spin_until = std::time::Instant::now() + std::time::Duration::from_millis(2);
    loop {
        match unsafe { event_gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(ev) => return Ok(Some(ev)),
            Err(_) => {
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Ok(None);
                }
                if now < spin_until {
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
    }
}

#[inline]
pub fn pack_u32_pair(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | (lo as u64)
}

#[inline]
pub fn unpack_u32_pair(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32)
}

pub fn create_nv12_sample(
    nv12: &[u8],
    timestamp_us: u64,
    duration_100ns: i64,
) -> Result<IMFSample> {
    unsafe {
        let buffer: IMFMediaBuffer =
            MFCreateMemoryBuffer(nv12.len() as u32).context("MFCreateMemoryBuffer failed")?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(nv12.len() as u32)?;

        let sample: IMFSample = MFCreateSample().context("MFCreateSample failed")?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime((timestamp_us * 10) as i64)?;
        sample.SetSampleDuration(duration_100ns)?;
        Ok(sample)
    }
}

pub fn create_compressed_sample(data: &[u8], timestamp_us: u64) -> Result<IMFSample> {
    unsafe {
        let buffer: IMFMediaBuffer =
            MFCreateMemoryBuffer(data.len() as u32).context("MFCreateMemoryBuffer failed")?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(data.len() as u32)?;

        let sample: IMFSample = MFCreateSample().context("MFCreateSample failed")?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime((timestamp_us * 10) as i64)?;
        Ok(sample)
    }
}

pub fn read_sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut cur_len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut cur_len))?;
        let slice = std::slice::from_raw_parts(ptr, cur_len as usize);
        let out = slice.to_vec();
        buffer.Unlock()?;
        Ok(out)
    }
}

pub fn pwstr_to_string_and_free(pwstr: PWSTR) -> String {
    if pwstr.is_null() {
        return String::new();
    }
    unsafe {
        let s = pwstr.to_string().unwrap_or_default();
        CoTaskMemFree(Some(pwstr.0 as *const _));
        s
    }
}

pub fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a == b
}

/// Splits an Annex-B byte stream into NAL units (without start codes).
pub fn split_annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut offsets = Vec::new();
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if i + 3 < data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                offsets.push((i, 4));
                i += 4;
                continue;
            } else if data[i + 2] == 1 {
                offsets.push((i, 3));
                i += 3;
                continue;
            }
        }
        i += 1;
    }
    let mut nals = Vec::with_capacity(offsets.len());
    for idx in 0..offsets.len() {
        let start = offsets[idx].0 + offsets[idx].1;
        let end = if idx + 1 < offsets.len() {
            offsets[idx + 1].0
        } else {
            data.len()
        };
        if start < end {
            nals.push(&data[start..end]);
        }
    }
    nals
}

pub fn annexb_has_param_sets(data: &[u8], is_hevc: bool) -> bool {
    for nal in split_annexb_nals(data) {
        if nal.is_empty() {
            continue;
        }
        if is_hevc {
            let nal_type = (nal[0] >> 1) & 0x3F;
            if nal_type == 32 || nal_type == 33 || nal_type == 34 {
                return true;
            }
        } else {
            let nal_type = nal[0] & 0x1F;
            if nal_type == 7 || nal_type == 8 {
                return true;
            }
        }
    }
    false
}

pub fn extract_annexb_param_sets(data: &[u8], is_hevc: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in split_annexb_nals(data) {
        if nal.is_empty() {
            continue;
        }
        let is_param = if is_hevc {
            let nal_type = (nal[0] >> 1) & 0x3F;
            nal_type == 32 || nal_type == 33 || nal_type == 34
        } else {
            let nal_type = nal[0] & 0x1F;
            nal_type == 7 || nal_type == 8
        };
        if is_param {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
    }
    out
}
