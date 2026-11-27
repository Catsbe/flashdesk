pub mod client_manager;
pub mod host_handler;

use anyhow::{Context, Result};
use protocol::transfer::FileEntry;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::{c_void, OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use uuid::Uuid;
use windows::core::{w, GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED, ERROR_PRIVILEGE_NOT_HELD, HANDLE,
    INVALID_HANDLE_VALUE, LUID,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, GetTokenInformation, IsWellKnownSid,
    LookupPrivilegeValueW, SecurityImpersonation, TokenImpersonation, TokenUser, WinLocalSystemSid,
    LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_DUPLICATE,
    TOKEN_IMPERSONATE, TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Diagnostics::Debug::{
    SetThreadErrorMode, SEM_FAILCRITICALERRORS, THREAD_ERROR_MODE,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSGetActiveConsoleSessionId, WTSQueryUserToken,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::WindowsProgramming::{
    DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, SHGetKnownFolderPath,
    KF_FLAG_DEFAULT, KF_FLAG_DONT_VERIFY,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeMeta {
    pub total_size: u64,
    pub file_sha256: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Upload,
    Download,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    Queued,
    Active,
    Done,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct TransferRecord {
    pub transfer_id: Uuid,
    pub file_name: String,
    pub local_path: String,
    pub remote_path: String,
    pub direction: TransferDirection,
    pub total_bytes: u64,
    pub transferred_bytes: u64,
    pub speed_mbps: f32,
    pub via_label: String,
    pub resumed: bool,
    pub sha256_verified: bool,
    pub state: TransferState,
}

pub fn part_and_meta_paths(final_path: &Path) -> (PathBuf, PathBuf) {
    let mut part_os = OsString::from(final_path.as_os_str());
    part_os.push(".fdesk.part");
    let mut meta_os = OsString::from(final_path.as_os_str());
    meta_os.push(".fdesk.meta");
    (PathBuf::from(part_os), PathBuf::from(meta_os))
}

pub fn compute_bytes_sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

pub fn compute_file_sha256(path: &Path) -> Result<[u8; 32]> {
    let mut file = File::open(path)
        .with_context(|| format!("failed to open file for SHA-256: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct UserKnownFolders {
    desktop: Option<PathBuf>,
    documents: Option<PathBuf>,
    downloads: Option<PathBuf>,
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn from_raw(handle: HANDLE) -> Option<Self> {
        if handle.0.is_null() || handle == INVALID_HANDLE_VALUE {
            None
        } else {
            Some(Self(handle))
        }
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0 .0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

struct CoTaskPwstr(PWSTR);

impl Drop for CoTaskPwstr {
    fn drop(&mut self) {
        if !self.0 .0.is_null() {
            unsafe {
                CoTaskMemFree(Some(self.0 .0 as *const c_void));
            }
        }
    }
}

struct ErrorModeGuard(Option<THREAD_ERROR_MODE>);

impl ErrorModeGuard {
    fn suppress_critical_errors() -> Self {
        let mut prev = THREAD_ERROR_MODE::default();
        let ok = unsafe { SetThreadErrorMode(SEM_FAILCRITICALERRORS, Some(&mut prev)) }.is_ok();
        Self(ok.then_some(prev))
    }
}

impl Drop for ErrorModeGuard {
    fn drop(&mut self) {
        if let Some(prev) = self.0 {
            unsafe {
                let _ = SetThreadErrorMode(prev, None);
            }
        }
    }
}

fn to_wide_null(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn is_system_profile_path(path: &Path) -> bool {
    let normalized = path.to_string_lossy().replace('/', "\\").to_lowercase();
    normalized.contains("\\config\\systemprofile")
}

fn is_token_local_system(token: HANDLE) -> bool {
    unsafe {
        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        if (needed as usize) < std::mem::size_of::<TOKEN_USER>() {
            return false;
        }
        let words = (needed as usize).div_ceil(std::mem::size_of::<u64>());
        let mut buf = vec![0u64; words];
        if GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut c_void),
            needed,
            &mut needed,
        )
        .is_err()
        {
            return false;
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        IsWellKnownSid(user.User.Sid, WinLocalSystemSid).as_bool()
    }
}

fn is_running_as_local_system() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let Some(owned) = OwnedHandle::from_raw(token) else {
            return false;
        };
        is_token_local_system(owned.raw())
    }
}

fn active_user_session_id() -> Option<u32> {
    let mut self_sid = u32::MAX;
    if unsafe { ProcessIdToSessionId(std::process::id(), &mut self_sid) }.is_ok()
        && self_sid != 0
        && self_sid != u32::MAX
    {
        return Some(self_sid);
    }
    let console_sid = unsafe { WTSGetActiveConsoleSessionId() };
    (console_sid != 0 && console_sid != u32::MAX).then_some(console_sid)
}

fn enable_tcb_privilege() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .is_err()
        {
            return false;
        }
        let Some(owned) = OwnedHandle::from_raw(token) else {
            return false;
        };
        let mut luid = LUID::default();
        if LookupPrivilegeValueW(None, w!("SeTcbPrivilege"), &mut luid).is_err() {
            return false;
        }
        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        if AdjustTokenPrivileges(owned.raw(), false, Some(&tp), 0, None, None).is_err() {
            return false;
        }
        GetLastError() != ERROR_NOT_ALL_ASSIGNED
    }
}

fn duplicate_to_impersonation_token(token: HANDLE) -> Option<OwnedHandle> {
    unsafe {
        let mut dup = HANDLE::default();
        DuplicateTokenEx(
            token,
            TOKEN_QUERY | TOKEN_IMPERSONATE | TOKEN_DUPLICATE,
            None,
            SecurityImpersonation,
            TokenImpersonation,
            &mut dup,
        )
        .ok()?;
        OwnedHandle::from_raw(dup)
    }
}

fn find_explorer_user_token(session_id: u32) -> Option<OwnedHandle> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let _snap_guard = OwnedHandle::from_raw(snap)?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_err() {
            return None;
        }
        loop {
            let end = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
            if name.eq_ignore_ascii_case("explorer.exe") {
                let mut pid_sid = u32::MAX;
                if ProcessIdToSessionId(entry.th32ProcessID, &mut pid_sid).is_ok()
                    && pid_sid == session_id
                {
                    if let Ok(proc_h) = OpenProcess(
                        PROCESS_QUERY_LIMITED_INFORMATION,
                        false,
                        entry.th32ProcessID,
                    ) {
                        if let Some(proc_guard) = OwnedHandle::from_raw(proc_h) {
                            let mut tok = HANDLE::default();
                            if OpenProcessToken(
                                proc_guard.raw(),
                                TOKEN_QUERY | TOKEN_DUPLICATE,
                                &mut tok,
                            )
                            .is_ok()
                            {
                                if let Some(tok_guard) = OwnedHandle::from_raw(tok) {
                                    if let Some(imp) =
                                        duplicate_to_impersonation_token(tok_guard.raw())
                                    {
                                        if !is_token_local_system(imp.raw()) {
                                            return Some(imp);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if Process32NextW(snap, &mut entry).is_err() {
                break;
            }
        }
        None
    }
}

/// Acquires an impersonation token for the logged-in interactive user when running as SYSTEM.
fn acquire_interactive_user_token() -> Option<OwnedHandle> {
    let session_id = active_user_session_id()?;
    unsafe {
        let mut raw_tok = HANDLE::default();
        let mut res = WTSQueryUserToken(session_id, &mut raw_tok);
        if res.is_err() && GetLastError() == ERROR_PRIVILEGE_NOT_HELD && enable_tcb_privilege() {
            res = WTSQueryUserToken(session_id, &mut raw_tok);
        }
        if res.is_ok() {
            if let Some(tok_guard) = OwnedHandle::from_raw(raw_tok) {
                if !is_token_local_system(tok_guard.raw()) {
                    if let Some(imp) = duplicate_to_impersonation_token(tok_guard.raw()) {
                        return Some(imp);
                    }
                }
            }
        }
    }
    if let Some(tok) = find_explorer_user_token(session_id) {
        return Some(tok);
    }
    tracing::debug!("no interactive user token available for session {session_id}");
    None
}

fn query_known_folder_with_token(folder_id: &GUID, token: HANDLE) -> Option<PathBuf> {
    for flags in [KF_FLAG_DEFAULT, KF_FLAG_DONT_VERIFY] {
        let ptr = match unsafe { SHGetKnownFolderPath(folder_id, flags, token) } {
            Ok(p) => p,
            Err(_) => continue,
        };
        let _guard = CoTaskPwstr(ptr);
        if ptr.0.is_null() {
            continue;
        }
        let len = unsafe { (0usize..).take_while(|&i| *ptr.0.add(i) != 0).count() };
        if len == 0 {
            continue;
        }
        let path_str = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr.0, len) });
        let path = PathBuf::from(path_str);
        if !is_system_profile_path(&path) && path.is_dir() {
            return Some(path);
        }
    }
    None
}

fn resolve_user_known_folders() -> UserKnownFolders {
    let token_guard = if is_running_as_local_system() {
        // Under LocalSystem, never fall back to HANDLE::default() (which resolves systemprofile).
        let Some(tok) = acquire_interactive_user_token() else {
            return UserKnownFolders::default();
        };
        Some(tok)
    } else {
        None
    };
    let token = token_guard
        .as_ref()
        .map_or(HANDLE::default(), OwnedHandle::raw);
    UserKnownFolders {
        desktop: query_known_folder_with_token(&FOLDERID_Desktop, token),
        documents: query_known_folder_with_token(&FOLDERID_Documents, token),
        downloads: query_known_folder_with_token(&FOLDERID_Downloads, token),
    }
}

fn public_desktop_fallback() -> Option<PathBuf> {
    let public = PathBuf::from(r"C:\Users\Public\Desktop");
    if public.is_dir() {
        return Some(public);
    }
    let root = PathBuf::from(r"C:\");
    root.is_dir().then_some(root)
}

pub fn default_receive_dir() -> PathBuf {
    let folders = resolve_user_known_folders();
    folders
        .desktop
        .or(folders.downloads)
        .or(folders.documents)
        .or_else(public_desktop_fallback)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn format_drive_label(letter: char, drive_type: u32, volume_name: Option<&str>) -> String {
    if let Some(name) = volume_name.map(str::trim).filter(|s| !s.is_empty()) {
        return format!("{name} ({letter}:)");
    }
    let prefix = match drive_type {
        DRIVE_REMOVABLE => "可移动磁盘",
        DRIVE_REMOTE => "网络驱动器",
        DRIVE_CDROM => "光驱",
        DRIVE_RAMDISK => "RAM 磁盘",
        _ => "本地磁盘",
    };
    format!("{prefix} ({letter}:)")
}

fn enumerate_drives() -> Vec<FileEntry> {
    let _err_guard = ErrorModeGuard::suppress_critical_errors();
    let mask = unsafe { GetLogicalDrives() };
    let mut entries = Vec::new();
    for bit in 0..26u32 {
        if (mask & (1 << bit)) == 0 {
            continue;
        }
        let letter = (b'A' + bit as u8) as char;
        let root = format!("{letter}:\\");
        let root_w = to_wide_null(&root);
        let drive_type = unsafe { GetDriveTypeW(PCWSTR(root_w.as_ptr())) };

        // Skip GetVolumeInformationW for network/CD drives (prevents multi-second stalls on offline shares/empty trays).
        let should_query_vol = matches!(drive_type, DRIVE_FIXED | DRIVE_RAMDISK)
            || (drive_type == DRIVE_REMOVABLE && !matches!(letter, 'A' | 'B'));

        let volume_name = if should_query_vol {
            let mut vol_buf = [0u16; 260];
            unsafe {
                GetVolumeInformationW(
                    PCWSTR(root_w.as_ptr()),
                    Some(&mut vol_buf),
                    None,
                    None,
                    None,
                    None,
                )
            }
            .ok()
            .map(|()| {
                let end = vol_buf.iter().position(|&c| c == 0).unwrap_or(vol_buf.len());
                String::from_utf16_lossy(&vol_buf[..end])
            })
        } else {
            None
        };

        entries.push(FileEntry {
            name: format_drive_label(letter, drive_type, volume_name.as_deref()),
            path: root,
            is_dir: true,
            size: 0,
            modified_unix: 0,
        });
    }
    entries
}

fn build_root_entries(folders: &UserKnownFolders, drives: Vec<FileEntry>) -> Vec<FileEntry> {
    let mut entries = Vec::new();
    for (label, opt_path) in [
        ("桌面", &folders.desktop),
        ("文档", &folders.documents),
        ("下载", &folders.downloads),
    ] {
        if let Some(path) = opt_path {
            entries.push(FileEntry {
                name: label.into(),
                path: path.to_string_lossy().into_owned(),
                is_dir: true,
                size: 0,
                modified_unix: 0,
            });
        }
    }
    entries.extend(drives);
    entries
}

pub fn list_local_dir(path: &str) -> Result<Vec<FileEntry>> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        let folders = resolve_user_known_folders();
        let drives = enumerate_drives();
        return Ok(build_root_entries(&folders, drives));
    }

    let dir_path = Path::new(trimmed);
    let mut dirs_vec = Vec::new();
    let mut files_vec = Vec::new();

    for item in std::fs::read_dir(dir_path)
        .with_context(|| format!("无法读取目录: {}", dir_path.display()))?
    {
        let Ok(entry) = item else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = if is_dir {
            0
        } else {
            meta.as_ref().map(|m| m.len()).unwrap_or(0)
        };
        let modified_unix = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let fe = FileEntry {
            name,
            path: entry.path().to_string_lossy().into_owned(),
            is_dir,
            size,
            modified_unix,
        };
        if is_dir {
            dirs_vec.push(fe);
        } else {
            files_vec.push(fe);
        }
    }

    dirs_vec.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    files_vec.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    dirs_vec.extend(files_vec);
    Ok(dirs_vec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::client_manager::{run_download_over_stream, run_upload_over_stream};
    use crate::transfer::host_handler::run_host_file_session;
    use protocol::auth::RootKeypair;
    use protocol::noise::NoiseStream;
    use std::sync::atomic::AtomicBool;

    async fn make_paired_noise_streams() -> (
        NoiseStream<tokio::io::DuplexStream>,
        NoiseStream<tokio::io::DuplexStream>,
    ) {
        let root = RootKeypair::generate();
        let id_a = root.sign_device_cert(Uuid::new_v4(), "Viewer", 3600);
        let id_b = root.sign_device_cert(Uuid::new_v4(), "Host", 3600);
        let root_pub = root.pubkey_bytes();
        let (sa, sb) = tokio::io::duplex(256 * 1024);
        let (ra, rb) = tokio::join!(
            NoiseStream::handshake_initiator(sa, &root_pub, &id_a, &[]),
            NoiseStream::handshake_responder(sb, &root_pub, &id_b, &[])
        );
        (ra.unwrap(), rb.unwrap())
    }

    #[tokio::test]
    async fn file_upload_and_sha256_verify() {
        let tmp = std::env::temp_dir().join(format!("fdesk_ul_{}", Uuid::new_v4()));
        let src_dir = tmp.join("src");
        let dst_dir = tmp.join("dst");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&dst_dir).unwrap();

        let src_file = src_dir.join("test_payload.bin");
        let data: Vec<u8> = (0..(700 * 1024)).map(|i| ((i * 31) % 253) as u8).collect();
        std::fs::write(&src_file, &data).unwrap();

        let (viewer_ns, host_ns) = make_paired_noise_streams().await;
        let host_task = tokio::spawn(run_host_file_session(host_ns));

        let (mut r, mut w, _) = viewer_ns.into_split();
        let records = Arc::new(Mutex::new(Vec::new()));
        let cancel = Arc::new(AtomicBool::new(false));
        let tid = Uuid::new_v4();
        let res = run_upload_over_stream(
            &mut r,
            &mut w,
            tid,
            &src_file,
            &dst_dir.to_string_lossy(),
            "test-relay",
            &records,
            &cancel,
            &|| {},
        )
        .await
        .unwrap();

        assert!(res.sha256_verified);
        assert!(!res.resumed);
        let uploaded = std::fs::read(dst_dir.join("test_payload.bin")).unwrap();
        assert_eq!(uploaded, data);

        drop(r);
        drop(w);
        host_task.abort();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn file_upload_resumes_from_partial_file() {
        let tmp = std::env::temp_dir().join(format!("fdesk_resume_{}", Uuid::new_v4()));
        let src_dir = tmp.join("src");
        let dst_dir = tmp.join("dst");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&dst_dir).unwrap();

        let src_file = src_dir.join("resume_payload.bin");
        let data: Vec<u8> = (0..(600 * 1024)).map(|i| ((i * 17) % 251) as u8).collect();
        std::fs::write(&src_file, &data).unwrap();
        let expected_sha = compute_bytes_sha256(&data);

        let final_dst = dst_dir.join("resume_payload.bin");
        let (part_path, meta_path) = part_and_meta_paths(&final_dst);
        let half = 256 * 1024;
        std::fs::write(&part_path, &data[..half]).unwrap();
        let meta = ResumeMeta {
            total_size: data.len() as u64,
            file_sha256: expected_sha,
        };
        std::fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();

        let (viewer_ns, host_ns) = make_paired_noise_streams().await;
        let host_task = tokio::spawn(run_host_file_session(host_ns));

        let (mut r, mut w, _) = viewer_ns.into_split();
        let records = Arc::new(Mutex::new(Vec::new()));
        let cancel = Arc::new(AtomicBool::new(false));
        let tid = Uuid::new_v4();
        let res = run_upload_over_stream(
            &mut r,
            &mut w,
            tid,
            &src_file,
            &dst_dir.to_string_lossy(),
            "test-relay",
            &records,
            &cancel,
            &|| {},
        )
        .await
        .unwrap();

        assert!(res.resumed);
        assert!(res.sha256_verified);
        assert!(!part_path.exists());
        assert!(!meta_path.exists());
        assert_eq!(std::fs::read(&final_dst).unwrap(), data);

        drop(r);
        drop(w);
        host_task.abort();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn file_download_and_sha256_verify() {
        let tmp = std::env::temp_dir().join(format!("fdesk_dl_{}", Uuid::new_v4()));
        let remote_dir = tmp.join("remote");
        let local_dir = tmp.join("local");
        std::fs::create_dir_all(&remote_dir).unwrap();
        std::fs::create_dir_all(&local_dir).unwrap();

        let remote_file = remote_dir.join("movie_clip.dat");
        let data: Vec<u8> = (0..(650 * 1024)).map(|i| ((i * 13) % 241) as u8).collect();
        std::fs::write(&remote_file, &data).unwrap();

        let (viewer_ns, host_ns) = make_paired_noise_streams().await;
        let host_task = tokio::spawn(run_host_file_session(host_ns));

        let (mut r, mut w, _) = viewer_ns.into_split();
        let records = Arc::new(Mutex::new(Vec::new()));
        let cancel = Arc::new(AtomicBool::new(false));
        let tid = Uuid::new_v4();
        let res = run_download_over_stream(
            &mut r,
            &mut w,
            tid,
            &remote_file.to_string_lossy(),
            &local_dir,
            "test-relay",
            &records,
            &cancel,
            &|| {},
        )
        .await
        .unwrap();

        assert!(res.sha256_verified);
        assert_eq!(
            std::fs::read(local_dir.join("movie_clip.dat")).unwrap(),
            data
        );

        drop(r);
        drop(w);
        host_task.abort();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn system_profile_path_guard_blocks_systemprofile() {
        assert!(is_system_profile_path(Path::new(
            r"C:\Windows\System32\config\systemprofile\Desktop"
        )));
        assert!(is_system_profile_path(Path::new(
            r"c:/windows/syswow64/config/systemprofile/Documents"
        )));
        assert!(!is_system_profile_path(Path::new(
            r"C:\Users\alice\Desktop"
        )));
    }

    #[test]
    fn build_root_entries_and_drive_label_formatting() {
        assert_eq!(
            format_drive_label('C', DRIVE_FIXED, Some("系统")),
            "系统 (C:)"
        );
        assert_eq!(
            format_drive_label('D', DRIVE_FIXED, Some("   ")),
            "本地磁盘 (D:)"
        );
        assert_eq!(
            format_drive_label('E', DRIVE_REMOVABLE, None),
            "可移动磁盘 (E:)"
        );
        assert_eq!(
            format_drive_label('F', DRIVE_REMOTE, None),
            "网络驱动器 (F:)"
        );
        assert_eq!(format_drive_label('G', DRIVE_CDROM, None), "光驱 (G:)");
        assert_eq!(
            format_drive_label('R', DRIVE_RAMDISK, None),
            "RAM 磁盘 (R:)"
        );

        let folders = UserKnownFolders {
            desktop: Some(PathBuf::from(r"C:\Users\alice\Desktop")),
            documents: None,
            downloads: Some(PathBuf::from(r"C:\Users\alice\Downloads")),
        };
        let drives = vec![FileEntry {
            name: "本地磁盘 (C:)".into(),
            path: r"C:\".into(),
            is_dir: true,
            size: 0,
            modified_unix: 0,
        }];
        let built = build_root_entries(&folders, drives);
        let names: Vec<&str> = built.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["桌面", "下载", "本地磁盘 (C:)"]);
    }

    #[test]
    fn list_local_dir_root_contains_user_shortcuts_and_drives() {
        let entries = list_local_dir("").unwrap();
        for expected in ["桌面", "文档", "下载"] {
            let entry = entries
                .iter()
                .find(|e| e.name == expected)
                .unwrap_or_else(|| panic!("missing root shortcut '{expected}' in {entries:?}"));
            assert!(entry.is_dir);
            assert!(
                !is_system_profile_path(Path::new(&entry.path)),
                "shortcut '{expected}' leaked systemprofile: {}",
                entry.path
            );
            assert!(
                Path::new(&entry.path).is_dir(),
                "shortcut '{expected}' path does not exist: {}",
                entry.path
            );
        }
        let c_drive = entries
            .iter()
            .find(|e| e.path.eq_ignore_ascii_case(r"C:\"))
            .expect("C:\\ drive missing from root entries");
        assert!(c_drive.name.contains("(C:)"), "unexpected C: label: {}", c_drive.name);
    }

    #[test]
    fn default_receive_dir_resolves_existing_user_dir() {
        let dir = default_receive_dir();
        assert_ne!(dir, PathBuf::from("."));
        assert!(!is_system_profile_path(&dir));
        assert!(dir.is_dir(), "default_receive_dir not a dir: {}", dir.display());
    }

    #[test]
    fn known_folder_query_with_duplicated_impersonation_token_works() {
        let mut proc_tok = HANDLE::default();
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_DUPLICATE,
                &mut proc_tok,
            )
            .expect("OpenProcessToken failed");
        }
        let proc_guard = OwnedHandle::from_raw(proc_tok).expect("null proc token");
        let imp_guard = duplicate_to_impersonation_token(proc_guard.raw())
            .expect("DuplicateTokenEx to TokenImpersonation failed");

        for fid in [FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads] {
            let direct = query_known_folder_with_token(&fid, HANDLE::default());
            let via_tok = query_known_folder_with_token(&fid, imp_guard.raw());
            assert!(direct.is_some(), "direct known folder query returned None");
            assert_eq!(via_tok, direct);
        }
    }

    #[tokio::test]
    async fn remote_list_root_over_noise_returns_shortcuts_and_navigates_desktop() {
        use protocol::transfer::FileMsg;

        let (viewer_ns, host_ns) = make_paired_noise_streams().await;
        let host_task = tokio::spawn(run_host_file_session(host_ns));
        let (mut r, mut w, _) = viewer_ns.into_split();

        let req = FileMsg::ListDirReq {
            path: String::new(),
        };
        w.send_msg(&bincode::serialize(&req).unwrap()).await.unwrap();
        let resp: FileMsg = bincode::deserialize(&r.recv_msg().await.unwrap()).unwrap();
        let entries = match resp {
            FileMsg::ListDirResp {
                path,
                entries,
                error,
            } => {
                assert!(path.is_empty());
                assert!(error.is_none(), "unexpected error: {error:?}");
                entries
            }
            other => panic!("unexpected FileMsg: {other:?}"),
        };

        let desktop = entries
            .iter()
            .find(|e| e.name == "桌面")
            .expect("remote root missing 桌面");
        assert!(entries.iter().any(|e| e.name == "文档"));
        assert!(entries.iter().any(|e| e.name == "下载"));
        assert!(entries.iter().any(|e| e.path.eq_ignore_ascii_case(r"C:\")));

        let nav_req = FileMsg::ListDirReq {
            path: desktop.path.clone(),
        };
        w.send_msg(&bincode::serialize(&nav_req).unwrap())
            .await
            .unwrap();
        let nav_resp: FileMsg = bincode::deserialize(&r.recv_msg().await.unwrap()).unwrap();
        match nav_resp {
            FileMsg::ListDirResp { path, error, .. } => {
                assert_eq!(path, desktop.path);
                assert!(error.is_none(), "navigating remote desktop failed: {error:?}");
            }
            other => panic!("unexpected FileMsg: {other:?}"),
        }

        drop(r);
        drop(w);
        host_task.abort();
    }

    use std::sync::{Arc, Mutex};
}
