use std::fs::{self, OpenOptions};
use std::path::Path;
use tracing_subscriber::EnvFilter;

pub fn initialise(log_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(log_dir)
        .map_err(|error| format!("无法创建日志目录 {}：{error}", log_dir.display()))?;
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("desktop.log"))
        .map_err(|error| format!("无法打开桌面日志：{error}"))?;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("deeptutor_desktop=info,tauri=warn"));
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(filter)
        .with_writer(log_file)
        .try_init()
        .map_err(|error| format!("无法初始化日志：{error}"))
}
