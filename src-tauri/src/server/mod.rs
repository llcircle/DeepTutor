mod agent_config;
mod audio_overview;
mod auth;
mod calendar;
mod catalog;
mod cron;
mod deferred;
mod document_settings;
mod documents;
mod documents_api;
mod engine;
pub mod error;
mod exec_tool;
mod files;
mod files_api;
mod hints;
mod http;
mod knowledge;
mod knowledge_sources;
mod knowledge_visuals;
mod learning_journal;
mod llm;
mod marginnote4;
mod mastery;
mod mcp_cli;
mod media_services;
mod memory;
mod modules;
mod multi_user;
mod notebook;
mod office_preview;
mod parsing;
mod partner_groups;
mod partners;
mod plugin_wasm;
mod plugins;
mod prompts;
mod provider_auth;
mod provider_probe;
mod question_bank;
mod rag_config;
mod reading;
mod settings_profile;
mod settings_services;
mod skills;
mod source_inventory;
mod subagents;
mod system_update;
mod tool_policy;
mod tools;
mod video_learning;
mod visualizers;
mod workspace_data;
mod ws;
mod ws_books;
mod ws_quiz;

use crate::portable::PortablePaths;
use crate::storage::Database;
use axum::extract::{DefaultBodyLimit, FromRequestParts};
use axum::http::request::Parts;
use axum::response::Redirect;
use axum::routing::get;
use axum::Router;
use parking_lot::Mutex;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;
use tracing::info;

pub use error::{ApiError, ApiResult};

#[derive(Clone)]
pub struct AppState {
    pub database: Database,
    pub paths: PortablePaths,
    pub client: Client,
    pub active_turns: Arc<Mutex<HashMap<String, CancellationToken>>>,
    pub turn_inputs: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Value>>>>,
    pub turn_events: broadcast::Sender<Value>,
    pub book_events: broadcast::Sender<Value>,
    pub mastery_events: broadcast::Sender<Value>,
    pub group_events: broadcast::Sender<Value>,
    pub group_live: Arc<Mutex<HashMap<String, Arc<partner_groups::GroupLiveOperation>>>>,
    pub(super) service_tests: Arc<Mutex<HashMap<String, Arc<provider_probe::ServiceTestRun>>>>,
    pub(super) hint_cache: Arc<hints::HintCache>,
    maintenance: Arc<Mutex<bool>>,
    update_exit: Option<Arc<dyn Fn() + Send + Sync>>,
    pub(super) document_jobs: Arc<document_settings::DocumentJobManager>,
    pub(super) mineru_download_jobs: Arc<document_settings::DocumentJobManager>,
    pub(super) provider_auth: Arc<provider_auth::ProviderAuthManager>,
    user_states: Arc<Mutex<HashMap<String, AppState>>>,
    pub started_at: Instant,
}

impl AppState {
    pub fn new(database: Database, paths: PortablePaths) -> Result<Self, String> {
        Self::new_with_exit(database, paths, None)
    }

    pub fn new_with_exit(
        database: Database,
        paths: PortablePaths,
        update_exit: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Self, String> {
        question_bank::initialize(&database)?;
        let client = Client::builder()
            .no_proxy()
            .user_agent(format!("DeepTutor/{}", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(20))
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .map_err(|error| format!("无法初始化网络客户端：{error}"))?;
        Ok(Self {
            database,
            paths,
            client,
            active_turns: Arc::new(Mutex::new(HashMap::new())),
            turn_inputs: Arc::new(Mutex::new(HashMap::new())),
            turn_events: broadcast::channel(4096).0,
            book_events: broadcast::channel(4096).0,
            mastery_events: broadcast::channel(4096).0,
            group_events: broadcast::channel(4096).0,
            group_live: Arc::new(Mutex::new(HashMap::new())),
            service_tests: Arc::new(Mutex::new(HashMap::new())),
            hint_cache: Arc::new(hints::HintCache::new()),
            maintenance: Arc::new(Mutex::new(false)),
            update_exit,
            document_jobs: Arc::new(document_settings::DocumentJobManager::new()),
            mineru_download_jobs: Arc::new(document_settings::DocumentJobManager::new()),
            provider_auth: Arc::new(provider_auth::ProviderAuthManager::new()),
            user_states: Arc::new(Mutex::new(HashMap::new())),
            started_at: Instant::now(),
        })
    }

    pub fn user_state(&self, user_id: &str) -> Result<AppState, String> {
        if let Some(state) = self.user_states.lock().get(user_id).cloned() {
            return Ok(state);
        }
        let paths = self.paths.for_user(user_id)?;
        let first_open = !paths.database.exists();
        let database = Database::new(&paths.database);
        let mut state = Self::new(database, paths)?;
        if first_open {
            for key in [
                "catalog",
                "enabled_tools",
                "capability_settings",
                "memory_settings",
                "network",
                "mcp",
            ] {
                if let Some(value) = self.database.get_setting(key)? {
                    state.database.set_setting(key, &value)?;
                }
            }
        }
        state.active_turns = self.active_turns.clone();
        state.maintenance = self.maintenance.clone();
        state.update_exit = self.update_exit.clone();
        state.user_states = Arc::new(Mutex::new(HashMap::new()));
        self.user_states
            .lock()
            .insert(user_id.to_string(), state.clone());
        Ok(state)
    }

    pub fn register_turn(&self, turn_id: &str) -> Option<CancellationToken> {
        let _gate = self.maintenance.lock();
        if *_gate {
            return None;
        }
        let token = CancellationToken::new();
        self.active_turns
            .lock()
            .insert(turn_id.to_string(), token.clone());
        Some(token)
    }

    pub fn turns_open(&self) -> bool {
        !*self.maintenance.lock()
    }

    pub fn begin_update_maintenance(&self) -> bool {
        let mut maintenance = self.maintenance.lock();
        if *maintenance {
            return false;
        }
        *maintenance = true;
        true
    }

    pub fn end_update_maintenance(&self) {
        *self.maintenance.lock() = false;
    }

    pub fn update_can_restart(&self) -> bool {
        self.update_exit.is_some()
    }

    pub fn request_update_restart(&self) -> bool {
        let Some(exit) = self.update_exit.as_ref() else {
            return false;
        };
        exit();
        true
    }

    pub fn has_active_turns_anywhere(&self) -> Result<bool, String> {
        if !self.active_turns.lock().is_empty() {
            return Ok(true);
        }
        if self.database.has_active_turns()? {
            return Ok(true);
        }
        let databases = self
            .user_states
            .lock()
            .values()
            .map(|state| state.database.clone())
            .collect::<Vec<_>>();
        for database in databases {
            if database.has_active_turns()? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn cancel_turn(&self, turn_id: &str) -> bool {
        if let Some(token) = self.active_turns.lock().get(turn_id).cloned() {
            token.cancel();
            true
        } else {
            false
        }
    }

    pub fn finish_turn(&self, turn_id: &str) {
        self.active_turns.lock().remove(turn_id);
        self.turn_inputs.lock().remove(turn_id);
    }

    pub fn register_turn_input(&self, turn_id: &str) -> mpsc::UnboundedReceiver<Value> {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.turn_inputs.lock().insert(turn_id.to_string(), sender);
        receiver
    }

    pub fn submit_turn_input(&self, turn_id: &str, input: Value) -> bool {
        self.turn_inputs
            .lock()
            .get(turn_id)
            .is_some_and(|sender| sender.send(input).is_ok())
    }

    pub fn broadcast_turn_event(&self, event: Value) {
        let _ = self.turn_events.send(event);
    }
}

pub(super) struct ScopedState(pub AppState);

impl Deref for ScopedState {
    type Target = AppState;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl FromRequestParts<AppState> for ScopedState {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let principal = parts
            .extensions
            .get::<auth::Principal>()
            .cloned()
            .unwrap_or_else(auth::Principal::local_admin);
        if principal.is_admin() || is_central_identity_path(parts.uri.path()) {
            return Ok(Self(state.clone()));
        }
        state
            .user_state(&principal.user_id)
            .map(Self)
            .map_err(ApiError::internal)
    }
}

fn is_central_identity_path(path: &str) -> bool {
    path.starts_with("/api/auth/")
        || path == "/api/auth"
        || path.starts_with("/api/multi-user/")
        || path == "/api/multi-user"
        || path.starts_with("/auth/")
        || path == "/auth"
}

pub struct ServerRuntime {
    pub url: String,
    pub task: JoinHandle<()>,
    pub shutdown: CancellationToken,
}

pub async fn start(state: AppState, web_directory: PathBuf) -> Result<ServerRuntime, String> {
    let index = web_directory.join("index.html");
    if !index.is_file() {
        return Err(format!(
            "缺少桌面前端资源 {}。构建前请先运行 `npm run build:desktop`。",
            index.display()
        ));
    }

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|error| format!("无法分配本地服务端口：{error}"))?;
    let address: SocketAddr = listener
        .local_addr()
        .map_err(|error| format!("无法读取本地服务地址：{error}"))?;
    let scheduler_state = state.clone();

    let static_service = ServeDir::new(&web_directory).not_found_service(ServeFile::new(&index));
    let api =
        http::router()
            .with_state(state.clone())
            .route_layer(axum::middleware::from_fn_with_state(
                state.clone(),
                auth::middleware,
            ));
    let websocket_routes = Router::new()
        .route("/ws", get(ws::websocket))
        .route("/ws/books", get(ws_books::websocket))
        .route("/ws/mastery-paths", get(mastery::websocket))
        .merge(partners::ws_router())
        .merge(partner_groups::ws_router())
        .route("/ws/questions/judge", get(ws_quiz::websocket))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::middleware,
        ));
    let files_routes = Router::new()
        .merge(files::router())
        .merge(files_api::router())
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::middleware,
        ));
    let app = Router::new()
        .nest("/api", api)
        .merge(files_routes)
        .route("/", get(root))
        .route("/health", get(http::health))
        .route("/health/live", get(http::health_live))
        .route("/health/ready", get(http::health))
        .merge(websocket_routes)
        .fallback_service(static_service)
        .layer(DefaultBodyLimit::max(512 * 1024 * 1024))
        .layer(CompressionLayer::new())
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let shutdown = CancellationToken::new();
    let _scheduler_task = cron::start_scheduler(scheduler_state, shutdown.clone());
    let server_shutdown = shutdown.clone();
    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app)
            .with_graceful_shutdown(server_shutdown.cancelled_owned())
            .await
        {
            tracing::error!(%error, "Rust 本地服务异常退出");
        }
    });
    let url = format!("http://{address}");
    info!(%url, web = %web_directory.display(), "Rust 原生服务已启动");
    Ok(ServerRuntime {
        url,
        task,
        shutdown,
    })
}

async fn root() -> Redirect {
    Redirect::temporary("/chat")
}

pub fn find_web_directory(paths: &PortablePaths, resource_dir: Option<&Path>) -> Option<PathBuf> {
    let mut candidates = paths.web_assets();
    if let Some(resource_dir) = resource_dir {
        candidates.extend([
            resource_dir.join("web"),
            resource_dir.join("resources/web"),
            resource_dir.join("resources/resources/web"),
        ]);
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.join("index.html").is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{header::LOCATION, StatusCode};
    use axum::response::IntoResponse;

    #[tokio::test]
    async fn root_redirects_to_the_desktop_workspace() {
        let response = root().await.into_response();
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(response.headers()[LOCATION], "/chat");
    }
}
