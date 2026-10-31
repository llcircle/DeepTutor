use serde::Serialize;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const PORTABLE_ROOT_ENV: &str = "DEEPTUTOR_PORTABLE_ROOT";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortablePaths {
    pub root: PathBuf,
    pub home: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    pub temp: PathBuf,
    pub logs: PathBuf,
    pub webview: PathBuf,
    pub uploads: PathBuf,
    pub outputs: PathBuf,
    pub database: PathBuf,
}

impl PortablePaths {
    pub fn resolve() -> Result<Self, String> {
        let root = portable_root();
        let data = root.join("data");
        let paths = Self {
            home: root.join("home"),
            cache: root.join("cache"),
            temp: root.join("temp"),
            logs: root.join("logs"),
            webview: root.join("cache").join("webview2"),
            uploads: data.join("uploads"),
            outputs: data.join("outputs"),
            database: data.join("deeptutor.sqlite3"),
            data,
            root,
        };
        paths.prepare()?;
        Ok(paths)
    }

    pub(crate) fn prepare(&self) -> Result<(), String> {
        for directory in [
            &self.home,
            // Windows 文件对话框会根据 USERPROFILE 访问标准用户文件夹。
            &self.home.join("Desktop"),
            &self.home.join("Documents"),
            &self.home.join("Downloads"),
            &self.home.join("Pictures"),
            &self.home.join("Music"),
            &self.home.join("Videos"),
            &self.home.join("Favorites"),
            &self.data,
            &self.cache,
            &self.temp,
            &self.logs,
            &self.webview,
            &self.uploads,
            &self.outputs,
            &self.data.join("settings"),
            &self.data.join("knowledge"),
            &self.data.join("reading"),
            &self.data.join("books"),
            &self.data.join("partners"),
            &self.cache.join("appdata/roaming"),
            &self.cache.join("appdata/local"),
            &self.cache.join("xdg/config"),
            &self.cache.join("xdg/cache"),
            &self.cache.join("xdg/data"),
            &self.cache.join("xdg/state"),
        ] {
            fs::create_dir_all(directory).map_err(|error| {
                format!(
                    "无法创建安装目录内的数据文件夹 {}：{error}。便携模式不会把应用数据写入用户目录。",
                    directory.display()
                )
            })?;
        }

        for directory in [&self.root, &self.data, &self.cache, &self.temp] {
            let probe = directory.join(format!(".write-probe-{}", uuid::Uuid::new_v4()));
            fs::write(&probe, b"portable")
                .and_then(|_| fs::remove_file(&probe))
                .map_err(|error| {
                    format!(
                        "安装目录不可写：{}（{error}）。请把 DeepTutor 解压到当前用户可写的目录。",
                        directory.display()
                    )
                })?;
        }
        Ok(())
    }

    pub(crate) fn for_user(&self, user_id: &str) -> Result<Self, String> {
        let component = safe_user_component(user_id)?;
        let data = self.data.join("users").join(&component);
        let paths = Self {
            root: self.root.clone(),
            home: self.home.join("users").join(&component),
            data: data.clone(),
            cache: self.cache.join("users").join(&component),
            temp: self.temp.join("users").join(&component),
            logs: self.logs.join("users").join(&component),
            webview: self.webview.clone(),
            uploads: data.join("uploads"),
            outputs: data.join("outputs"),
            database: data.join("deeptutor.sqlite3"),
        };
        paths.prepare()?;
        Ok(paths)
    }

    /// 在创建 WebView 和网络客户端前锁定缓存位置。
    pub fn apply_process_environment(&self) {
        for (key, value) in self.environment() {
            env::set_var(key, value);
        }
        if let Some(runtime) = self.webview2_runtime() {
            env::set_var("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER", runtime);
        }
        env::set_var(
            "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
            format!(
                "--disk-cache-dir=\"{}\" --disable-breakpad --disable-crash-reporter",
                self.webview.join("browser-cache").display()
            ),
        );
    }

    pub fn environment(&self) -> BTreeMap<String, String> {
        let path = |value: &Path| value.to_string_lossy().into_owned();
        BTreeMap::from([
            ("DEEPTUTOR_PORTABLE".into(), "1".into()),
            ("DEEPTUTOR_HOME".into(), path(&self.root)),
            ("DEEPTUTOR_CACHE_ROOT".into(), path(&self.cache)),
            ("DEEPTUTOR_DESKTOP_DATA_DIR".into(), path(&self.data)),
            ("DEEPTUTOR_DESKTOP_LOG_DIR".into(), path(&self.logs)),
            ("DEEPTUTOR_DATABASE".into(), path(&self.database)),
            ("WEBVIEW2_USER_DATA_FOLDER".into(), path(&self.webview)),
            ("HOME".into(), path(&self.home)),
            ("USERPROFILE".into(), path(&self.home)),
            ("APPDATA".into(), path(&self.cache.join("appdata/roaming"))),
            (
                "LOCALAPPDATA".into(),
                path(&self.cache.join("appdata/local")),
            ),
            (
                "XDG_CONFIG_HOME".into(),
                path(&self.cache.join("xdg/config")),
            ),
            ("XDG_CACHE_HOME".into(), path(&self.cache.join("xdg/cache"))),
            ("XDG_DATA_HOME".into(), path(&self.cache.join("xdg/data"))),
            ("XDG_STATE_HOME".into(), path(&self.cache.join("xdg/state"))),
            ("TMP".into(), path(&self.temp)),
            ("TEMP".into(), path(&self.temp)),
            ("TMPDIR".into(), path(&self.temp)),
            ("RUST_BACKTRACE".into(), "1".into()),
            ("DO_NOT_TRACK".into(), "1".into()),
        ])
    }

    pub fn web_assets(&self) -> Vec<PathBuf> {
        vec![
            self.root.join("web"),
            self.root.join("resources/web"),
            self.root.join("resources/resources/web"),
        ]
    }

    fn webview2_runtime(&self) -> Option<PathBuf> {
        let explicit = env::var_os("DEEPTUTOR_WEBVIEW2_RUNTIME").map(PathBuf::from);
        explicit
            .into_iter()
            .chain([
                self.root.join("resources/webview2-runtime"),
                self.root.join("webview2-runtime"),
            ])
            .find(|candidate| candidate.join("msedgewebview2.exe").is_file())
    }
}

fn portable_root() -> PathBuf {
    if let Some(raw) = env::var_os(PORTABLE_ROOT_ENV) {
        let value = PathBuf::from(raw);
        if !value.as_os_str().is_empty() {
            return absolutize(value);
        }
    }

    if cfg!(debug_assertions) {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        if let Some(repository_root) = manifest.parent() {
            return repository_root.to_path_buf();
        }
    }

    env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn absolutize(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn safe_user_component(user_id: &str) -> Result<String, String> {
    let value = user_id.trim();
    if value.is_empty()
        || value.len() > 96
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err("用户标识无法安全映射到便携数据目录".to_string());
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths(prefix: &str) -> PortablePaths {
        let root = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        PortablePaths {
            home: root.join("home"),
            data: root.join("data"),
            cache: root.join("cache"),
            temp: root.join("temp"),
            logs: root.join("logs"),
            webview: root.join("cache/webview2"),
            uploads: root.join("data/uploads"),
            outputs: root.join("data/outputs"),
            database: root.join("data/deeptutor.sqlite3"),
            root,
        }
    }

    fn assert_profile_directories(home: &Path) {
        for name in [
            "Desktop",
            "Documents",
            "Downloads",
            "Pictures",
            "Music",
            "Videos",
            "Favorites",
        ] {
            assert!(
                home.join(name).is_dir(),
                "便携用户文件夹未创建：{}",
                home.join(name).display()
            );
        }
    }

    #[test]
    fn prepare_creates_profile_directories_before_redirecting_userprofile() {
        let paths = test_paths("deeptutor-profile-test");
        assert!(!paths.home.exists());

        paths.prepare().expect("首次启动可创建便携用户文件夹");

        let environment = paths.environment();
        let userprofile = Path::new(
            environment
                .get("USERPROFILE")
                .expect("保留 USERPROFILE 重定向"),
        );
        assert_eq!(userprofile, paths.home);
        assert_profile_directories(userprofile);
        fs::remove_dir_all(&paths.root).expect("清理测试目录");
    }

    #[test]
    fn prepare_repairs_existing_profiles_without_overwriting_user_files() {
        let paths = test_paths("deeptutor-profile-upgrade-test");
        let desktop = paths.home.join("Desktop");
        fs::create_dir_all(&desktop).expect("创建已有的便携用户目录");
        let existing_file = desktop.join("user-document.txt");
        fs::write(&existing_file, "保留用户文件").expect("写入已有用户文件");

        paths.prepare().expect("升级时补齐缺少的用户文件夹");
        assert_profile_directories(&paths.home);
        paths.prepare().expect("重复启动可以安全补齐用户文件夹");
        assert_eq!(
            fs::read_to_string(&existing_file).expect("读取已有用户文件"),
            "保留用户文件"
        );
        fs::remove_dir_all(&paths.root).expect("清理测试目录");
    }

    #[test]
    fn every_runtime_path_stays_inside_the_portable_root() {
        let root =
            std::env::temp_dir().join(format!("deeptutor-portable-test-{}", uuid::Uuid::new_v4()));
        let paths = PortablePaths {
            home: root.join("home"),
            data: root.join("data"),
            cache: root.join("cache"),
            temp: root.join("temp"),
            logs: root.join("logs"),
            webview: root.join("cache/webview2"),
            uploads: root.join("data/uploads"),
            outputs: root.join("data/outputs"),
            database: root.join("data/deeptutor.sqlite3"),
            root: root.clone(),
        };

        for (key, value) in paths.environment() {
            if matches!(
                key.as_str(),
                "DEEPTUTOR_PORTABLE" | "DO_NOT_TRACK" | "RUST_BACKTRACE"
            ) {
                continue;
            }
            assert!(
                Path::new(&value).starts_with(&root),
                "{key}={value} 逃逸出便携目录 {}",
                root.display()
            );
        }
    }

    #[test]
    fn user_paths_remain_inside_the_portable_root() {
        let root =
            std::env::temp_dir().join(format!("deeptutor-user-path-test-{}", uuid::Uuid::new_v4()));
        let paths = PortablePaths {
            home: root.join("home"),
            data: root.join("data"),
            cache: root.join("cache"),
            temp: root.join("temp"),
            logs: root.join("logs"),
            webview: root.join("cache/webview2"),
            uploads: root.join("data/uploads"),
            outputs: root.join("data/outputs"),
            database: root.join("data/deeptutor.sqlite3"),
            root: root.clone(),
        };
        let user = paths.for_user("u_0123456789abcdef").expect("用户路径有效");
        for path in [
            &user.home,
            &user.data,
            &user.cache,
            &user.temp,
            &user.logs,
            &user.uploads,
            &user.outputs,
            &user.database,
        ] {
            assert!(path.starts_with(&root), "{} 逃逸出便携目录", path.display());
        }
        assert_profile_directories(&user.home);
        assert!(paths.for_user("../escape").is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
