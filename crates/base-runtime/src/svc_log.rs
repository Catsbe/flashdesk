use base_common::unix_now;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();

pub fn init(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(md) = std::fs::metadata(path) {
        if md.len() > 8 * 1024 * 1024 {
            let old = PathBuf::from(format!("{}.old", path.display()));
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(path, &old);
        }
    }
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        opts.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    }
    if let Ok(file) = opts.open(path) {
        let _ = LOG_FILE.set(Mutex::new(file));
    }
}

pub fn write_line(level: &str, msg: &str) {
    let ts = unix_now();
    let line = format!("[{ts}] [{level}] {msg}\n");
    if let Some(m) = LOG_FILE.get() {
        if let Ok(mut f) = m.lock() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::svc_log::write_line("INFO", &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::svc_log::write_line("WARN", &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::svc_log::write_line("ERROR", &format!($($arg)*))
    };
}
