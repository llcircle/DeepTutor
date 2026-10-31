#[cfg_attr(test, allow(dead_code))]
mod logging;
#[cfg_attr(test, allow(dead_code))]
mod portable;
mod server;
mod storage;
mod update;

#[cfg(not(test))]
use portable::PortablePaths;
#[cfg(not(test))]
use serde::Serialize;
#[cfg(not(test))]
use server::{AppState, ServerRuntime};
#[cfg(not(test))]
use std::path::PathBuf;
#[cfg(not(test))]
use std::process::Command;
#[cfg(not(test))]
use std::sync::Arc;
#[cfg(not(test))]
use storage::Database;
#[cfg(not(test))]
use tauri::{AppHandle, Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder};
#[cfg(not(test))]
use tokio::sync::RwLock;
#[cfg(not(test))]
use tracing::info;

#[cfg(not(test))]
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStatus {
    state: String,
    detail: String,
    last_error: Option<String>,
    progress: u8,
    active_step: String,
    completed_steps: Vec<String>,
    gateway_url: Option<String>,
    backend_url: Option<String>,
    frontend_url: Option<String>,
    portable_root: String,
    data_dir: String,
    cache_dir: String,
    logs_dir: String,
}

#[cfg(not(test))]
struct DesktopState {
    paths: PortablePaths,
    database: Database,
    server: RwLock<Option<ServerRuntime>>,
}

#[cfg(not(test))]
impl DesktopState {
    fn new(paths: PortablePaths, database: Database) -> Self {
        Self {
            paths,
            database,
            server: RwLock::new(None),
        }
    }

    fn paths(&self) -> &PortablePaths {
        &self.paths
    }

    async fn start_server(
        &self,
        resource_dir: Option<PathBuf>,
        app: Option<AppHandle>,
    ) -> Result<String, String> {
        if let Some(runtime) = self.server.read().await.as_ref() {
            return Ok(runtime.url.clone());
        }
        let web_directory = server::find_web_directory(&self.paths, resource_dir.as_deref())
            .ok_or_else(|| {
                format!(
                    "找不到桌面前端资源。请先运行 `npm run build:desktop`，检查 {}。",
                    self.paths.root.join("resources/web").display()
                )
            })?;
        let exit_hook = app.map(|app| Arc::new(move || app.exit(0)) as Arc<dyn Fn() + Send + Sync>);
        let app_state =
            AppState::new_with_exit(self.database.clone(), self.paths.clone(), exit_hook)?;
        let runtime = server::start(app_state, web_directory).await?;
        let url = runtime.url.clone();
        *self.server.write().await = Some(runtime);
        Ok(url)
    }

    async fn stop_server(&self) {
        if let Some(runtime) = self.server.write().await.take() {
            runtime.shutdown.cancel();
            let _ = runtime.task.await;
        }
    }
}

#[cfg(not(test))]
#[tauri::command]
async fn runtime_status(state: State<'_, DesktopState>) -> Result<RuntimeStatus, String> {
    let server = state.server.read().await;
    let paths = state.paths();
    let (status, detail, url) = if let Some(runtime) = server.as_ref() {
        ("ready", "Rust 原生服务已就绪", Some(runtime.url.clone()))
    } else {
        ("starting", "正在启动 Rust 原生服务", None)
    };
    Ok(RuntimeStatus {
        state: status.into(),
        detail: detail.into(),
        last_error: None,
        progress: if url.is_some() { 100 } else { 50 },
        active_step: String::new(),
        completed_steps: vec!["paths".into(), "database".into(), "server".into()],
        gateway_url: url.clone(),
        backend_url: url.clone(),
        frontend_url: url,
        portable_root: paths.root.to_string_lossy().into_owned(),
        data_dir: paths.data.to_string_lossy().into_owned(),
        cache_dir: paths.cache.to_string_lossy().into_owned(),
        logs_dir: paths.logs.to_string_lossy().into_owned(),
    })
}

#[cfg(not(test))]
#[tauri::command]
async fn start_runtime(
    app: AppHandle,
    state: State<'_, DesktopState>,
) -> Result<RuntimeStatus, String> {
    let resource_dir = app.path().resource_dir().ok();
    state.start_server(resource_dir, Some(app.clone())).await?;
    runtime_status(state).await
}

#[cfg(not(test))]
#[tauri::command]
async fn restart_runtime(
    app: AppHandle,
    state: State<'_, DesktopState>,
) -> Result<RuntimeStatus, String> {
    state.stop_server().await;
    start_runtime(app, state).await
}

#[cfg(not(test))]
#[tauri::command]
async fn enter_app(
    window: tauri::WebviewWindow,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    let server = state.server.read().await;
    let runtime = server
        .as_ref()
        .ok_or_else(|| "Rust 原生服务尚未就绪".to_string())?;
    let url = runtime
        .url
        .parse()
        .map_err(|error| format!("无效的本地服务地址：{error}"))?;
    window
        .navigate(url)
        .map_err(|error| format!("无法进入 DeepTutor 工作台：{error}"))
}

#[cfg(not(test))]
#[tauri::command]
fn open_runtime_folder(kind: String, state: State<'_, DesktopState>) -> Result<(), String> {
    let paths = state.paths();
    let path = match kind.as_str() {
        "root" => &paths.root,
        "data" => &paths.data,
        "cache" => &paths.cache,
        "logs" => &paths.logs,
        "webview" => &paths.webview,
        _ => return Err(format!("未知目录类型：{kind}")),
    };
    open_folder(path)
}

#[cfg(not(test))]
fn open_folder(path: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("explorer");
        command.arg(path);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(path);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(path);
        command
    };
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("无法打开目录 {}：{error}", path.display()))
}

#[cfg(not(test))]
pub fn run() {
    if update::run_worker_if_requested() {
        return;
    }
    let paths = match PortablePaths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    paths.apply_process_environment();
    if let Err(error) = logging::initialise(&paths.logs) {
        eprintln!("{error}");
    }
    let database = Database::new(&paths.database);
    if let Err(error) = database.initialize() {
        eprintln!("{error}");
        std::process::exit(1);
    }
    info!(
        root = %paths.root.display(),
        data = %paths.data.display(),
        cache = %paths.cache.display(),
        "便携目录与 SQLite 已初始化"
    );

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .manage(DesktopState::new(paths, database))
        .invoke_handler(tauri::generate_handler![
            runtime_status,
            start_runtime,
            restart_runtime,
            enter_app,
            open_runtime_folder
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            let state = handle.state::<DesktopState>();
            let resource_dir = handle.path().resource_dir().ok();
            let server_url = tauri::async_runtime::block_on(
                state.start_server(resource_dir, Some(handle.clone())),
            )
            .map_err(std::io::Error::other)?;
            if let Err(error) = update::reconcile_startup(state.paths()) {
                eprintln!("无法协调上次更新状态：{error}");
            }
            let webview_url = server_url
                .parse()
                .map_err(|error| std::io::Error::other(format!("无效窗口地址：{error}")))?;
            let webview_dir = state.paths().webview.clone();
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(webview_url))
                .title("DeepTutor")
                .inner_size(1440.0, 900.0)
                .min_inner_size(1024.0, 700.0)
                .center()
                .data_directory(webview_dir)
                .build()?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("无法构建 DeepTutor 桌面应用");

    app.run(move |app_handle, event| {
        if matches!(event, RunEvent::Exit | RunEvent::ExitRequested { .. }) {
            let state = app_handle.state::<DesktopState>();
            tauri::async_runtime::block_on(state.stop_server());
        }
    });
}
