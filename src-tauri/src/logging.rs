use std::fs::{self, OpenOptions};
use std::path::Path;
use tracing_subscriber::EnvFilter;

const DEFAULT_FILTER: &str = "deeptutor_desktop_lib=info,tauri=warn";

pub fn initialise(log_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(log_dir)
        .map_err(|error| format!("无法创建日志目录 {}：{error}", log_dir.display()))?;
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("desktop.log"))
        .map_err(|error| format!("无法打开桌面日志：{error}"))?;

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(filter)
        .with_writer(log_file)
        .try_init()
        .map_err(|error| format!("无法初始化日志：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_filter_targets_the_rust_library_crate() {
        let filter = EnvFilter::try_new(DEFAULT_FILTER).expect("默认日志过滤器必须有效");
        assert_eq!(filter.to_string(), DEFAULT_FILTER);
    }
}
