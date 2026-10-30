//! 便携版更新检查、可信下载、原子替换与失败回滚。
//!
//! 更新代理是当前可执行文件的安装目录副本。主进程退出后，代理才会替换
//! 程序文件；任何数据目录都不会进入更新包，也不会被替换。

use crate::portable::PortablePaths;
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use futures_util::StreamExt;
use regex::Regex;
use reqwest::redirect;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use url::Url;
use uuid::Uuid;
use zip::ZipArchive;

const RELEASE_REPOSITORY: &str = "HKUDS/DeepTutor";
const RELEASE_API_URL: &str = "https://api.github.com/repos/HKUDS/DeepTutor/releases/latest";
const RELEASE_PAGE_PREFIX: &str = "https://github.com/HKUDS/DeepTutor/releases/";
const RELEASE_ASSET_PATH_PREFIX: &str = "/HKUDS/DeepTutor/releases/download/";
const VERSION_CHECK_TTL_SECONDS: i64 = 24 * 60 * 60;
const MAX_RELEASE_JSON_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CHECKSUM_BYTES: u64 = 16 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 50_000;
const MAX_REDIRECTS: usize = 6;
const WORKER_START_TIMEOUT_SECONDS: u64 = 90;
const PARENT_EXIT_TIMEOUT_SECONDS: u64 = 120;
const UPDATE_MARKER_ARG: &str = "--deeptutor-apply-update";

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub type UpdateResult<T> = Result<T, String>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub version: String,
    pub name: String,
    pub published_at: String,
    pub url: String,
    pub excerpt: String,
    pub migration_warning: bool,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseCache {
    pub checked_at: String,
    pub release: ReleaseInfo,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpdateJob {
    pub schema_version: u32,
    pub id: String,
    pub status: String,
    pub current_version: String,
    pub target_version: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub error: Option<String>,
    pub restart_count: u32,
    pub release_url: String,
    pub asset_name: String,
    pub asset_url: String,
    pub checksum_asset_name: String,
    pub checksum_asset_url: String,
    pub executable_name: String,
    #[serde(default)]
    pub worker_pid: Option<u32>,
}

impl UpdateJob {
    pub fn public_payload(&self) -> Value {
        json!({
            "id": self.id,
            "status": self.status,
            "current_version": self.current_version,
            "target_version": self.target_version,
            "created_at": self.created_at,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "error": self.error,
            "restart_count": self.restart_count,
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct InstallationInfo {
    pub mode: String,
    pub current_version: String,
    pub automatic_update: bool,
    pub command: String,
    pub reason: String,
}

pub struct UpdatePaths {
    pub root: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
    pub active: PathBuf,
    pub jobs: PathBuf,
    pub backups: PathBuf,
    pub workers: PathBuf,
    pub log: PathBuf,
}

impl UpdatePaths {
    pub fn new(paths: &PortablePaths) -> Self {
        let root = paths.data.join("user").join("update");
        Self {
            state: root.join("state.json"),
            cache: root.join("release-cache.json"),
            active: root.join("active"),
            jobs: root.join("jobs"),
            backups: root.join("backups"),
            workers: root.join("workers"),
            log: root.join("worker.log"),
            root,
        }
    }

    pub fn prepare(&self) -> UpdateResult<()> {
        for directory in [&self.root, &self.jobs, &self.backups, &self.workers] {
            fs::create_dir_all(directory).map_err(|error| {
                format!(
                    "无法创建安装目录内的更新目录 {}：{error}",
                    directory.display()
                )
            })?;
        }
        Ok(())
    }

    pub fn load_job(&self) -> UpdateResult<Option<UpdateJob>> {
        let raw = match fs::read(&self.state) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("无法读取更新状态：{error}")),
        };
        if raw.len() > 256 * 1024 {
            return Err("更新状态文件异常过大".to_string());
        }
        let job: UpdateJob = serde_json::from_slice(&raw)
            .map_err(|error| format!("更新状态 JSON 已损坏：{error}"))?;
        validate_job(&job)?;
        Ok(Some(job))
    }

    pub fn write_job(&self, job: &UpdateJob) -> UpdateResult<()> {
        validate_job(job)?;
        self.prepare()?;
        write_json_atomic(&self.state, job)
    }

    pub fn release_active(&self, job_id: &str) {
        let owner = fs::read_to_string(&self.active)
            .ok()
            .map(|value| value.trim().to_string());
        if owner.as_deref() == Some(job_id) {
            let _ = fs::remove_file(&self.active);
        }
    }

    fn claim_active(&self, job_id: &str) -> UpdateResult<()> {
        self.prepare()?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.active)
        {
            Ok(mut file) => {
                file.write_all(job_id.as_bytes())
                    .map_err(|error| format!("无法写入更新互斥标记：{error}"))?;
                file.sync_all()
                    .map_err(|error| format!("无法同步更新互斥标记：{error}"))?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err("另一个 DeepTutor 更新任务仍在进行".to_string())
            }
            Err(error) => Err(format!("无法创建更新互斥标记：{error}")),
        }
    }
}

pub fn release_cache(paths: &UpdatePaths) -> Option<ReleaseCache> {
    let raw = fs::read(&paths.cache).ok()?;
    if raw.len() > MAX_RELEASE_JSON_BYTES as usize {
        return None;
    }
    let cache: ReleaseCache = serde_json::from_slice(&raw).ok()?;
    if normalize_stable_version(&cache.release.version).ok()? != cache.release.version {
        return None;
    }
    validate_release_url(&cache.release.url).ok()?;
    Some(cache)
}

pub fn cache_is_fresh(cache: &ReleaseCache) -> bool {
    DateTime::parse_from_rfc3339(&cache.checked_at)
        .map(|checked| {
            Utc::now().signed_duration_since(checked.with_timezone(&Utc))
                < ChronoDuration::seconds(VERSION_CHECK_TTL_SECONDS)
        })
        .unwrap_or(false)
}

pub fn write_release_cache(
    paths: &UpdatePaths,
    release: &ReleaseInfo,
) -> UpdateResult<ReleaseCache> {
    let cache = ReleaseCache {
        checked_at: now(),
        release: release.clone(),
    };
    paths.prepare()?;
    write_json_atomic(&paths.cache, &cache)?;
    Ok(cache)
}

pub fn updater_client() -> UpdateResult<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("DeepTutor-Portable-Updater/{CURRENT_VERSION}"))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30 * 60))
        .redirect(redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.stop()
            } else if redirect_target_is_trusted(attempt.url()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .map_err(|error| format!("无法初始化更新网络客户端：{error}"))
}

pub async fn check_release(client: &reqwest::Client) -> UpdateResult<ReleaseInfo> {
    let response = client
        .get(RELEASE_API_URL)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|error| format!("无法检查 DeepTutor 更新：{error}"))?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(format!(
            "GitHub 更新接口返回 HTTP {}",
            response.status().as_u16()
        ));
    }
    if !redirect_target_is_trusted(response.url()) {
        return Err("GitHub 更新接口发生了不受信任的重定向".to_string());
    }
    let bytes = read_response_limited(response, MAX_RELEASE_JSON_BYTES).await?;
    parse_release_payload(&bytes)
}

fn parse_release_payload(bytes: &[u8]) -> UpdateResult<ReleaseInfo> {
    let payload: Value =
        serde_json::from_slice(bytes).map_err(|error| format!("更新元数据 JSON 无效：{error}"))?;
    if !payload.is_object()
        || payload.get("draft").and_then(Value::as_bool) == Some(true)
        || payload.get("prerelease").and_then(Value::as_bool) == Some(true)
    {
        return Err("官方仓库当前没有可用的稳定版本".to_string());
    }
    let version = normalize_stable_version(
        payload
            .get("tag_name")
            .and_then(Value::as_str)
            .unwrap_or(""),
    )?;
    let url = payload
        .get("html_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    validate_release_url(&url)?;
    if !url.contains(&format!("/releases/tag/")) || !release_url_matches_version(&url, &version) {
        return Err("官方 Release 页面地址与版本不匹配".to_string());
    }
    let body = payload
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let migration_warning = Regex::new(
        r"(?i)breaking chang|migration(?: needed| required)|database migration|run migration|migrate your",
    )
    .expect("固定正则有效")
    .is_match(&body);
    let mut assets = Vec::new();
    if let Some(items) = payload.get("assets").and_then(Value::as_array) {
        if items.len() > 256 {
            return Err("官方 Release 的附件数量异常".to_string());
        }
        for item in items {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let asset_url = item
                .get("browser_download_url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if name.is_empty() || name.len() > 256 {
                continue;
            }
            if validate_release_asset_url(&asset_url, &version, &name).is_err() {
                continue;
            }
            assets.push(ReleaseAsset {
                name,
                browser_download_url: asset_url,
                size: item.get("size").and_then(Value::as_u64).unwrap_or(0),
            });
        }
    }
    Ok(ReleaseInfo {
        version,
        name: payload
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        published_at: payload
            .get("published_at")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        url,
        excerpt: plain_release_excerpt(&body),
        migration_warning,
        assets,
    })
}

pub fn select_release_assets(
    release: &ReleaseInfo,
) -> UpdateResult<(&ReleaseAsset, &ReleaseAsset)> {
    let expected_name = format!(
        "DeepTutor-{}-portable-{}.zip",
        release.version,
        architecture_label()
    );
    let archive = release
        .assets
        .iter()
        .find(|asset| asset.name.eq_ignore_ascii_case(&expected_name))
        .ok_or_else(|| {
            format!(
                "Release {} 未提供当前架构的可信便携包 {}",
                release.version, expected_name
            )
        })?;
    if archive.size > MAX_ARCHIVE_BYTES {
        return Err(format!(
            "官方更新包大小 {} 超过允许上限 {}",
            archive.size, MAX_ARCHIVE_BYTES
        ));
    }
    let checksum_name = format!("{}.sha256", archive.name);
    let checksum = release
        .assets
        .iter()
        .find(|asset| asset.name.eq_ignore_ascii_case(&checksum_name))
        .ok_or_else(|| {
            format!(
                "Release {} 缺少更新包校验文件 {}",
                release.version, checksum_name
            )
        })?;
    if checksum.size > MAX_CHECKSUM_BYTES {
        return Err("官方更新校验文件大小异常".to_string());
    }
    Ok((archive, checksum))
}

pub fn installation_info(paths: &PortablePaths) -> InstallationInfo {
    let automatic = portable_installation(paths);
    InstallationInfo {
        mode: if automatic { "portable" } else { "source" }.to_string(),
        current_version: CURRENT_VERSION.to_string(),
        automatic_update: automatic,
        command: if automatic {
            String::new()
        } else {
            "请从官方 Release 下载便携包并完整覆盖程序文件".to_string()
        },
        reason: if automatic {
            "安装目录内的纯 Rust 便携版；更新前会验证官方附件、SHA-256 和架构。".to_string()
        } else {
            "当前不是可直接自更新的发布版便携安装，开发目录中的程序不会执行自替换。".to_string()
        },
    }
}

pub fn update_available(release: &ReleaseInfo) -> bool {
    version_tuple(&release.version) > version_tuple(CURRENT_VERSION)
}

fn portable_installation(paths: &PortablePaths) -> bool {
    if cfg!(debug_assertions) {
        return false;
    }
    if !paths.root.join("portable-manifest.json").is_file() {
        return false;
    }
    let Ok(executable) = std::env::current_exe() else {
        return false;
    };
    let Ok(executable) = executable.canonicalize() else {
        return false;
    };
    let Ok(root) = paths.root.canonicalize() else {
        return false;
    };
    let expected = expected_executable_name();
    executable.parent() == Some(root.as_path())
        && executable
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case(&expected))
}

pub fn expected_executable_name() -> String {
    if cfg!(windows) {
        "DeepTutor.exe".to_string()
    } else {
        "DeepTutor".to_string()
    }
}

fn architecture_label() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "X64",
        "x86" => "X86",
        "aarch64" => "ARM64",
        _ => "UNKNOWN",
    }
}

fn normalize_stable_version(value: &str) -> UpdateResult<String> {
    let trimmed = value.trim();
    let without_prefix = trimmed.strip_prefix('v').unwrap_or(trimmed);
    if Regex::new(r"^\d+\.\d+\.\d+$")
        .expect("固定正则有效")
        .is_match(without_prefix)
    {
        Ok(without_prefix.to_string())
    } else {
        Err("更新版本必须是稳定的三段式语义版本".to_string())
    }
}

fn version_tuple(value: &str) -> (u64, u64, u64) {
    let normalized = value.trim().trim_start_matches('v');
    let mut parts = normalized.split('.');
    (
        parts.next().and_then(|part| part.parse().ok()).unwrap_or(0),
        parts.next().and_then(|part| part.parse().ok()).unwrap_or(0),
        parts.next().and_then(|part| part.parse().ok()).unwrap_or(0),
    )
}

fn validate_release_url(value: &str) -> UpdateResult<()> {
    if value.is_empty() || !value.starts_with(RELEASE_PAGE_PREFIX) {
        return Err("官方 Release 页面地址无效".to_string());
    }
    let url = Url::parse(value).map_err(|error| format!("官方 Release 地址无效：{error}"))?;
    if !redirect_target_is_trusted(&url)
        || url.host_str() != Some("github.com")
        || !url
            .path()
            .starts_with(&format!("/{RELEASE_REPOSITORY}/releases/"))
    {
        return Err("官方 Release 页面不在受信任的代码仓库中".to_string());
    }
    Ok(())
}

fn release_url_matches_version(value: &str, version: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    let prefix = format!("/{RELEASE_REPOSITORY}/releases/tag/");
    let Some(tag) = url.path().strip_prefix(&prefix) else {
        return false;
    };
    normalize_stable_version(tag).ok().as_deref() == Some(version)
}

fn validate_release_asset_url(value: &str, version: &str, asset_name: &str) -> UpdateResult<()> {
    let url = Url::parse(value).map_err(|error| format!("Release 附件地址无效：{error}"))?;
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || url.username() != ""
        || url.password().is_some()
        || !matches!(url.port(), None | Some(443))
    {
        return Err("Release 附件不在受信任的 HTTPS 主机上".to_string());
    }
    let Some(tail) = url.path().strip_prefix(RELEASE_ASSET_PATH_PREFIX) else {
        return Err("Release 附件路径不在官方仓库下载目录中".to_string());
    };
    let Some((tag, name)) = tail.split_once('/') else {
        return Err("Release 附件路径无效".to_string());
    };
    if name != asset_name
        || name.contains('/')
        || asset_name.contains('\\')
        || normalize_stable_version(tag).ok().as_deref() != Some(version)
    {
        return Err("Release 附件名称或版本不匹配".to_string());
    }
    Ok(())
}

fn redirect_target_is_trusted(url: &Url) -> bool {
    if url.scheme() != "https" || url.username() != "" || url.password().is_some() {
        return false;
    }
    if !matches!(url.port(), None | Some(443)) {
        return false;
    }
    matches!(
        url.host_str(),
        Some("api.github.com")
            | Some("github.com")
            | Some("objects.githubusercontent.com")
            | Some("release-assets.githubusercontent.com")
            | Some("github-releases.githubusercontent.com")
    )
}

fn plain_release_excerpt(body: &str) -> String {
    let mut text = Regex::new(r"(?s)<!--.*?-->")
        .expect("固定正则有效")
        .replace_all(body, "")
        .into_owned();
    for pattern in [
        r"(?s)```.*?```",
        r"(?m)^\s{0,3}#{1,6}\s+.*$",
        r"(?m)^\s*(?:-{3,}|\*{3,}|_{3,})\s*$",
        r"<https?://[^>]+>",
        r"<[^>]+>",
    ] {
        text = Regex::new(pattern)
            .expect("固定正则有效")
            .replace_all(&text, "")
            .into_owned();
    }
    for pattern in [r"!\[([^]]*)\]\([^)]+\)", r"\[([^]]+)\]\([^)]+\)"] {
        text = Regex::new(pattern)
            .expect("固定正则有效")
            .replace_all(&text, "$1")
            .into_owned();
    }
    text = Regex::new(r"(?m)^\s*(?:>|[-+*]|\d+[.)])\s+")
        .expect("固定正则有效")
        .replace_all(&text, "")
        .into_owned();
    text = Regex::new(r"[*_~`]")
        .expect("固定正则有效")
        .replace_all(&text, "")
        .into_owned();
    let summary = text
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .filter(|line| {
            !Regex::new(r"(?i)^(?:release date|published)\s*:")
                .expect("固定正则有效")
                .is_match(line)
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if summary.chars().count() <= 640 {
        summary
    } else {
        format!(
            "{}...",
            summary.chars().take(637).collect::<String>().trim_end()
        )
    }
}

async fn read_response_limited(response: reqwest::Response, limit: u64) -> UpdateResult<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        return Err("远程更新响应超过允许大小".to_string());
    }
    let mut stream = response.bytes_stream();
    let mut data = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("读取远程更新响应失败：{error}"))?;
        if data.len() as u64 + chunk.len() as u64 > limit {
            return Err("远程更新响应超过允许大小".to_string());
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn validate_job(job: &UpdateJob) -> UpdateResult<()> {
    if job.schema_version != 1
        || job.id.len() != 32
        || !job.id.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("更新任务标识无效".to_string());
    }
    normalize_stable_version(&job.current_version)?;
    normalize_stable_version(&job.target_version)?;
    if !matches!(
        job.status.as_str(),
        "pending" | "handoff" | "running" | "restarting" | "succeeded" | "failed"
    ) {
        return Err("更新任务状态无效".to_string());
    }
    if job.executable_name.is_empty()
        || job.executable_name.len() > 128
        || job
            .executable_name
            .chars()
            .any(|character| matches!(character, '/' | '\\' | ':'))
    {
        return Err("更新任务的可执行文件名无效".to_string());
    }
    validate_release_url(&job.release_url)?;
    validate_release_asset_url(&job.asset_url, &job.target_version, &job.asset_name)?;
    validate_release_asset_url(
        &job.checksum_asset_url,
        &job.target_version,
        &job.checksum_asset_name,
    )?;
    Ok(())
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> UpdateResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("无法创建更新状态目录：{error}"))?;
    }
    let temporary = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    let bytes =
        serde_json::to_vec_pretty(value).map_err(|error| format!("序列化更新状态失败：{error}"))?;
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| format!("无法创建更新临时状态：{error}"))?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| format!("无法写入更新临时状态：{error}"))?;
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("无法原子保存更新状态：{error}"));
    }
    Ok(())
}

pub fn create_job(
    paths: &UpdatePaths,
    release: &ReleaseInfo,
    archive: &ReleaseAsset,
    checksum: &ReleaseAsset,
) -> UpdateResult<UpdateJob> {
    let current = normalize_stable_version(CURRENT_VERSION)?;
    let target = normalize_stable_version(&release.version)?;
    if version_tuple(&target) <= version_tuple(&current) {
        return Err("当前已是最新稳定版本".to_string());
    }
    let executable_name = expected_executable_name();
    let job = UpdateJob {
        schema_version: 1,
        id: Uuid::new_v4().simple().to_string(),
        status: "pending".to_string(),
        current_version: current,
        target_version: target,
        created_at: now(),
        started_at: None,
        finished_at: None,
        error: None,
        restart_count: 0,
        release_url: release.url.clone(),
        asset_name: archive.name.clone(),
        asset_url: archive.browser_download_url.clone(),
        checksum_asset_name: checksum.name.clone(),
        checksum_asset_url: checksum.browser_download_url.clone(),
        executable_name,
        worker_pid: None,
    };
    validate_job(&job)?;
    paths.claim_active(&job.id)?;
    if let Err(error) = paths.write_job(&job) {
        paths.release_active(&job.id);
        return Err(error);
    }
    Ok(job)
}

pub fn mark_job_failed(paths: &UpdatePaths, job_id: &str, error: impl Into<String>) {
    let message = error.into();
    if let Ok(Some(mut job)) = paths.load_job() {
        if job.id == job_id {
            job.status = "failed".to_string();
            job.finished_at = Some(now());
            job.error = Some(truncate_message(&message, 1000));
            let _ = paths.write_job(&job);
        }
    }
    paths.release_active(job_id);
}

pub async fn prepare_and_handoff(portable: &PortablePaths, job_id: &str) -> UpdateResult<()> {
    let paths = UpdatePaths::new(portable);
    paths.prepare()?;
    let mut job = paths
        .load_job()?
        .ok_or_else(|| "找不到待执行的更新任务".to_string())?;
    if job.id != job_id || job.status != "pending" {
        return Err("更新任务已变化，无法继续".to_string());
    }
    job.status = "running".to_string();
    job.started_at = Some(now());
    job.error = None;
    paths.write_job(&job)?;

    if let Err(error) = download_and_extract(&paths, &job).await {
        mark_job_failed(&paths, &job.id, &error);
        return Err(error);
    }

    job.status = "handoff".to_string();
    paths.write_job(&job)?;
    let pid = match spawn_update_worker(portable, &paths, &job) {
        Ok(pid) => pid,
        Err(error) => {
            mark_job_failed(&paths, &job.id, &error);
            return Err(error);
        }
    };
    job.worker_pid = Some(pid);
    if let Err(error) = paths.write_job(&job) {
        mark_job_failed(&paths, &job.id, &error);
        return Err(error);
    }
    Ok(())
}

async fn download_and_extract(paths: &UpdatePaths, job: &UpdateJob) -> UpdateResult<()> {
    let job_root = paths.jobs.join(&job.id);
    let download_root = job_root.join("download");
    let package_root = job_root.join("package");
    for directory in [&job_root, &download_root] {
        fs::create_dir_all(directory).map_err(|error| format!("无法创建更新暂存目录：{error}"))?;
    }
    remove_path_if_exists(&package_root)?;

    let client = updater_client()?;
    let checksum_path = download_root.join("archive.sha256");
    let archive_path = download_root.join("portable.zip");
    download_url_to_file(
        &client,
        &job.checksum_asset_url,
        &checksum_path,
        MAX_CHECKSUM_BYTES,
    )
    .await?;
    let expected = parse_checksum_file(&checksum_path, &job.asset_name)?;
    let actual =
        download_url_to_file(&client, &job.asset_url, &archive_path, MAX_ARCHIVE_BYTES).await?;
    if expected != actual {
        let _ = fs::remove_file(&archive_path);
        return Err("官方更新包 SHA-256 校验失败，已拒绝安装".to_string());
    }
    extract_verified_package(&archive_path, &package_root, &job.target_version)?;
    Ok(())
}

async fn download_url_to_file(
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    limit: u64,
) -> UpdateResult<String> {
    let parsed = Url::parse(url).map_err(|error| format!("更新下载地址无效：{error}"))?;
    if !redirect_target_is_trusted(&parsed) {
        return Err("更新下载地址不在受信任主机上".to_string());
    }
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("下载官方更新文件失败：{error}"))?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(format!(
            "官方更新下载返回 HTTP {}",
            response.status().as_u16()
        ));
    }
    if !redirect_target_is_trusted(response.url()) {
        return Err("官方更新下载跳转到了不受信任的主机".to_string());
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        return Err("官方更新文件超过允许大小".to_string());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("无法创建更新下载目录：{error}"))?;
    }
    let partial = destination.with_extension(format!("part-{}", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)
        .map_err(|error| format!("无法创建更新下载文件：{error}"))?;
    let mut hasher = Sha256::new();
    let mut written = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("读取官方更新下载失败：{error}"))?;
        written = written.saturating_add(chunk.len() as u64);
        if written > limit {
            drop(file);
            let _ = fs::remove_file(&partial);
            return Err("官方更新文件超过允许大小".to_string());
        }
        hasher.update(&chunk);
        if let Err(error) = file.write_all(&chunk) {
            drop(file);
            let _ = fs::remove_file(&partial);
            return Err(format!("写入更新下载文件失败：{error}"));
        }
    }
    file.sync_all()
        .map_err(|error| format!("同步更新下载文件失败：{error}"))?;
    drop(file);
    if destination.exists() {
        fs::remove_file(destination)
            .map_err(|error| format!("无法替换旧的更新下载文件：{error}"))?;
    }
    fs::rename(&partial, destination).map_err(|error| format!("无法保存更新下载文件：{error}"))?;
    Ok(hex::encode(hasher.finalize()))
}

fn parse_checksum_file(path: &Path, expected_name: &str) -> UpdateResult<String> {
    let raw = fs::read(path).map_err(|error| format!("无法读取官方 SHA-256 文件：{error}"))?;
    if raw.len() as u64 > MAX_CHECKSUM_BYTES {
        return Err("官方 SHA-256 文件超过允许大小".to_string());
    }
    let text = std::str::from_utf8(&raw)
        .map_err(|_| "官方 SHA-256 文件必须是 UTF-8 或 ASCII".to_string())?;
    let mut fields = text.split_whitespace();
    let hash = fields.next().unwrap_or("").to_ascii_lowercase();
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("官方 SHA-256 文件格式无效".to_string());
    }
    if let Some(name) = fields.next() {
        let name = name.trim_start_matches('*');
        if !name.is_empty() && !name.eq_ignore_ascii_case(expected_name) {
            return Err("SHA-256 文件对应的更新包名称不匹配".to_string());
        }
    }
    Ok(hash)
}

fn extract_verified_package(
    archive_path: &Path,
    package_root: &Path,
    expected_version: &str,
) -> UpdateResult<()> {
    let archive_file =
        File::open(archive_path).map_err(|error| format!("无法打开更新压缩包：{error}"))?;
    let mut archive = ZipArchive::new(archive_file)
        .map_err(|error| format!("官方更新包不是有效 ZIP：{error}"))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err("官方更新包包含过多文件".to_string());
    }
    let extract_root = package_root
        .parent()
        .ok_or_else(|| "更新包暂存路径无效".to_string())?
        .join("extract");
    remove_path_if_exists(&extract_root)?;
    fs::create_dir_all(&extract_root).map_err(|error| format!("无法创建更新解压目录：{error}"))?;

    let mut total = 0u64;
    let mut seen = std::collections::HashSet::new();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("无法读取更新包条目：{error}"))?;
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err("官方更新包不能包含符号链接".to_string());
        }
        let relative = entry
            .enclosed_name()
            .ok_or_else(|| "官方更新包包含越界路径".to_string())?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        for component in relative.components() {
            if !matches!(component, Component::Normal(_)) {
                return Err("官方更新包包含无效路径".to_string());
            }
        }
        if !seen.insert(relative.clone()) {
            return Err("官方更新包包含重复路径".to_string());
        }
        total = total.saturating_add(entry.size());
        if total > MAX_EXTRACTED_BYTES {
            return Err("官方更新包解压后体积超过允许上限".to_string());
        }
        let destination = extract_root.join(&relative);
        if entry.is_dir() {
            fs::create_dir_all(&destination)
                .map_err(|error| format!("无法创建更新解压目录：{error}"))?;
            continue;
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("无法创建更新解压目录：{error}"))?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .map_err(|error| format!("无法创建更新解压文件：{error}"))?;
        std::io::copy(&mut entry, &mut file)
            .map_err(|error| format!("无法解压官方更新包：{error}"))?;
        file.sync_all()
            .map_err(|error| format!("无法同步更新解压文件：{error}"))?;
    }

    let normalized = normalize_extracted_root(&extract_root)?;
    validate_portable_package(&normalized, expected_version)?;
    if package_root.exists() {
        remove_path_if_exists(package_root)?;
    }
    fs::rename(&normalized, package_root)
        .map_err(|error| format!("无法准备更新安装目录：{error}"))?;
    Ok(())
}

fn normalize_extracted_root(root: &Path) -> UpdateResult<PathBuf> {
    if root.join("portable-manifest.json").is_file() {
        return Ok(root.to_path_buf());
    }
    let entries = fs::read_dir(root)
        .map_err(|error| format!("无法读取更新解压目录：{error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("无法读取更新解压目录：{error}"))?;
    if entries.len() == 1
        && entries[0]
            .file_type()
            .map(|kind| kind.is_dir())
            .unwrap_or(false)
    {
        let nested = entries[0].path();
        if nested.join("portable-manifest.json").is_file() {
            return Ok(nested);
        }
    }
    Err("官方更新包缺少 portable-manifest.json".to_string())
}

fn validate_portable_package(root: &Path, expected_version: &str) -> UpdateResult<()> {
    let executable = expected_executable_name();
    let required = [
        executable.as_str(),
        "portable-manifest.json",
        "resources/web/index.html",
    ];
    for relative in required {
        let required_path = root.join(relative);
        if !required_path.is_file() {
            return Err(format!(
                "官方更新包缺少必要文件：{relative}（检查路径：{}）",
                required_path.display()
            ));
        }
    }
    let manifest_raw = fs::read(root.join("portable-manifest.json"))
        .map_err(|error| format!("无法读取便携清单：{error}"))?;
    if manifest_raw.len() > 1024 * 1024 {
        return Err("便携清单超过允许大小".to_string());
    }
    let manifest: Value = serde_json::from_slice(&manifest_raw)
        .map_err(|error| format!("便携清单 JSON 无效：{error}"))?;
    if manifest.get("product").and_then(Value::as_str) != Some("DeepTutor")
        || manifest.get("runtime").and_then(Value::as_str) != Some("rust")
        || manifest.get("version").and_then(Value::as_str) != Some(expected_version)
        || !manifest
            .get("architecture")
            .and_then(Value::as_str)
            .is_some_and(|value| value.eq_ignore_ascii_case(architecture_label()))
    {
        return Err("官方便携包的产品、运行时、版本或架构信息不匹配".to_string());
    }
    let allowed_top = [
        executable.as_str(),
        "resources",
        "portable-manifest.json",
        "PORTABLE-README.txt",
        "LICENSE",
        "THIRD_PARTY_NOTICES.md",
    ];
    for entry in fs::read_dir(root).map_err(|error| format!("无法检查官方更新包目录：{error}"))?
    {
        let entry = entry.map_err(|error| format!("无法检查官方更新包目录：{error}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !allowed_top
            .iter()
            .any(|allowed| name.eq_ignore_ascii_case(allowed))
        {
            return Err(format!("官方更新包包含未授权的顶层文件：{name}"));
        }
    }
    Ok(())
}

fn spawn_update_worker(
    portable: &PortablePaths,
    paths: &UpdatePaths,
    job: &UpdateJob,
) -> UpdateResult<u32> {
    let worker_dir = paths.workers.join(&job.id);
    fs::create_dir_all(&worker_dir).map_err(|error| format!("无法创建更新代理目录：{error}"))?;
    let helper = worker_dir.join(expected_executable_name());
    fs::copy(
        std::env::current_exe().map_err(|error| format!("无法定位当前程序：{error}"))?,
        &helper,
    )
    .map_err(|error| format!("无法复制安装目录内的更新代理：{error}"))?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log)
        .map_err(|error| format!("无法创建更新日志：{error}"))?;
    let stderr = log
        .try_clone()
        .map_err(|error| format!("无法复制更新日志句柄：{error}"))?;
    let mut command = Command::new(&helper);
    command
        .arg(UPDATE_MARKER_ARG)
        .arg("--install-root")
        .arg(&portable.root)
        .arg("--update-root")
        .arg(&paths.root)
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .arg("--job-id")
        .arg(&job.id)
        .current_dir(&portable.root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    detach_command(&mut command);
    let child = command
        .spawn()
        .map_err(|error| format!("无法启动安装目录内的更新代理：{error}"))?;
    Ok(child.id())
}

fn detach_command(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
}

struct InstallRecord {
    target: PathBuf,
    backup: Option<PathBuf>,
}

struct InstallTransaction {
    records: Vec<InstallRecord>,
}

impl InstallTransaction {
    fn rollback(&self) -> UpdateResult<()> {
        let mut failures = Vec::new();
        for record in self.records.iter().rev() {
            if let Err(error) = remove_path_if_exists(&record.target) {
                failures.push(error);
            }
            if let Some(backup) = record.backup.as_ref() {
                if backup.exists() {
                    if let Some(parent) = record.target.parent() {
                        if let Err(error) = fs::create_dir_all(parent) {
                            failures.push(format!("无法恢复回滚目录：{error}"));
                            continue;
                        }
                    }
                    if let Err(error) = rename_with_retry(backup, &record.target, 60) {
                        failures.push(format!("无法恢复 {}：{error}", record.target.display()));
                    }
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(format!("更新回滚未完全成功：{}", failures.join("；")))
        }
    }
}

fn install_portable_update(
    install_root: &Path,
    update_paths: &UpdatePaths,
    job: &UpdateJob,
) -> UpdateResult<InstallTransaction> {
    let package = update_paths.jobs.join(&job.id).join("package");
    validate_portable_package(&package, &job.target_version)?;
    let backup_root = update_paths.backups.join(&job.id);
    remove_path_if_exists(&backup_root)?;
    fs::create_dir_all(&backup_root).map_err(|error| format!("无法创建更新回滚目录：{error}"))?;

    let mut items = vec![
        "resources".to_string(),
        job.executable_name.clone(),
        "PORTABLE-README.txt".to_string(),
        "portable-manifest.json".to_string(),
        "LICENSE".to_string(),
        "THIRD_PARTY_NOTICES.md".to_string(),
    ];
    items.dedup();
    let mut transaction = InstallTransaction {
        records: Vec::new(),
    };
    for relative in items {
        let staged = package.join(&relative);
        if !staged.exists() {
            continue;
        }
        let target = install_root.join(&relative);
        let backup = backup_root.join(&relative);
        let had_target = target.exists();
        if had_target {
            if let Some(parent) = backup.parent() {
                if let Err(error) = fs::create_dir_all(parent) {
                    let rollback = transaction.rollback();
                    return Err(match rollback {
                        Ok(()) => format!("无法创建回滚目录：{error}"),
                        Err(rollback_error) => {
                            format!("无法创建回滚目录：{error}；{rollback_error}")
                        }
                    });
                }
            }
            if let Err(error) = rename_with_retry(&target, &backup, PARENT_EXIT_TIMEOUT_SECONDS) {
                let rollback = transaction.rollback();
                return Err(match rollback {
                    Ok(()) => format!("无法备份 {}：{error}", target.display()),
                    Err(rollback_error) => {
                        format!("无法备份 {}：{error}；{rollback_error}", target.display())
                    }
                });
            }
        }
        transaction.records.push(InstallRecord {
            target: target.clone(),
            backup: had_target.then_some(backup),
        });
        if let Err(error) = rename_with_retry(&staged, &target, PARENT_EXIT_TIMEOUT_SECONDS) {
            let rollback = transaction.rollback();
            return Err(match rollback {
                Ok(()) => format!("无法安装 {}：{error}", target.display()),
                Err(rollback_error) => {
                    format!("无法安装 {}：{error}；{rollback_error}", target.display())
                }
            });
        }
    }
    Ok(transaction)
}

pub fn run_worker_if_requested() -> bool {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if !arguments.iter().any(|value| value == UPDATE_MARKER_ARG) {
        return false;
    }
    let result = (|| -> UpdateResult<()> {
        let install_root = argument_value(&arguments, "--install-root")?;
        let update_root = argument_value(&arguments, "--update-root")?;
        let parent_pid = argument_value(&arguments, "--parent-pid")?
            .parse::<u32>()
            .map_err(|_| "更新代理收到无效的父进程号".to_string())?;
        let job_id = argument_value(&arguments, "--job-id")?;
        run_update_worker(
            Path::new(&install_root),
            Path::new(&update_root),
            parent_pid,
            &job_id,
        )
    })();
    if let Err(error) = result {
        let log_path = arguments
            .iter()
            .position(|value| value == "--update-root")
            .and_then(|index| arguments.get(index + 1))
            .map(|root| PathBuf::from(root).join("worker.log"));
        if let Some(path) = log_path {
            append_update_log(&path, &format!("更新代理失败：{error}"));
        }
    }
    true
}

fn run_update_worker(
    install_root: &Path,
    update_root: &Path,
    parent_pid: u32,
    job_id: &str,
) -> UpdateResult<()> {
    let install_root = install_root
        .canonicalize()
        .map_err(|error| format!("更新代理无法解析安装目录：{error}"))?;
    let update_root = update_root
        .canonicalize()
        .map_err(|error| format!("更新代理无法解析更新目录：{error}"))?;
    let expected_update_root = install_root.join("data").join("user").join("update");
    if update_root != expected_update_root {
        return Err("更新代理拒绝操作安装目录之外的更新路径".to_string());
    }
    let update_paths = UpdatePaths {
        root: update_root.clone(),
        state: update_root.join("state.json"),
        cache: update_root.join("release-cache.json"),
        active: update_root.join("active"),
        jobs: update_root.join("jobs"),
        backups: update_root.join("backups"),
        workers: update_root.join("workers"),
        log: update_root.join("worker.log"),
    };
    let mut job = update_paths
        .load_job()?
        .ok_or_else(|| "更新代理找不到持久化任务".to_string())?;
    if job.id != job_id || job.status != "handoff" {
        return Err("更新代理读取到的任务不是待交接状态".to_string());
    }
    if job.executable_name != expected_executable_name() {
        return Err("更新任务与当前程序文件名不匹配".to_string());
    }
    append_update_log(
        &update_paths.log,
        &format!(
            "更新代理已启动：job={} target={} parent={}",
            job.id, job.target_version, parent_pid
        ),
    );
    wait_for_process_exit(parent_pid, Duration::from_secs(PARENT_EXIT_TIMEOUT_SECONDS))?;
    job.status = "running".to_string();
    job.started_at = Some(now());
    update_paths.write_job(&job)?;

    let transaction = match install_portable_update(&install_root, &update_paths, &job) {
        Ok(transaction) => transaction,
        Err(error) => {
            mark_job_failed(&update_paths, &job.id, &error);
            restore_old_application(&install_root, &job, &update_paths);
            return Err(error);
        }
    };
    job.status = "restarting".to_string();
    job.restart_count = job.restart_count.saturating_add(1);
    if let Err(error) = update_paths.write_job(&job) {
        let rollback = transaction.rollback();
        mark_job_failed(&update_paths, &job.id, &error);
        restore_old_application(&install_root, &job, &update_paths);
        return Err(match rollback {
            Ok(()) => error,
            Err(rollback_error) => format!("{error}；{rollback_error}"),
        });
    }
    let mut child = match launch_application(&install_root, &job.executable_name) {
        Ok(child) => child,
        Err(error) => {
            let rollback = transaction.rollback();
            mark_job_failed(&update_paths, &job.id, &error);
            restore_old_application(&install_root, &job, &update_paths);
            return Err(match rollback {
                Ok(()) => error,
                Err(rollback_error) => format!("{error}；{rollback_error}"),
            });
        }
    };
    let deadline = Instant::now() + Duration::from_secs(WORKER_START_TIMEOUT_SECONDS);
    loop {
        if let Ok(Some(current)) = update_paths.load_job() {
            if current.id == job.id && current.status == "succeeded" {
                cleanup_success_artifacts(&update_paths, &job.id);
                append_update_log(&update_paths.log, "新版本已成功启动，更新完成");
                return Ok(());
            }
            if current.id == job.id && current.status == "failed" {
                break;
            }
        }
        if child
            .try_wait()
            .map_err(|error| format!("无法检查新版本进程状态：{error}"))?
            .is_some()
        {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let rollback = transaction.rollback();
    let error = match rollback {
        Ok(()) => "新版本未在规定时间内完成启动，已恢复旧版本".to_string(),
        Err(rollback_error) => {
            format!("新版本启动失败，且旧版本恢复不完整：{rollback_error}")
        }
    };
    mark_job_failed(&update_paths, &job.id, &error);
    restore_old_application(&install_root, &job, &update_paths);
    Err(error)
}

fn restore_old_application(root: &Path, job: &UpdateJob, paths: &UpdatePaths) {
    if let Err(error) = launch_application(root, &job.executable_name) {
        append_update_log(
            &paths.log,
            &format!("更新失败后无法自动重启旧版本：{error}"),
        );
    }
}

fn launch_application(root: &Path, executable_name: &str) -> UpdateResult<std::process::Child> {
    if executable_name.is_empty()
        || executable_name
            .chars()
            .any(|character| matches!(character, '/' | '\\' | ':'))
    {
        return Err("拒绝启动无效的程序文件名".to_string());
    }
    let executable = root.join(executable_name);
    if !executable.is_file() {
        return Err(format!("安装目录缺少可执行文件 {}", executable.display()));
    }
    let mut command = Command::new(&executable);
    command
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_command(&mut command);
    command
        .spawn()
        .map_err(|error| format!("无法启动 {}：{error}", executable.display()))
}

pub fn reconcile_startup(portable: &PortablePaths) -> UpdateResult<()> {
    let paths = UpdatePaths::new(portable);
    paths.prepare()?;
    let Some(mut job) = paths.load_job()? else {
        return Ok(());
    };
    match job.status.as_str() {
        "restarting" if job.target_version == CURRENT_VERSION => {
            job.status = "succeeded".to_string();
            job.finished_at = Some(now());
            job.error = None;
            paths.write_job(&job)?;
            paths.release_active(&job.id);
            cleanup_success_artifacts(&paths, &job.id);
        }
        "pending" | "handoff" | "running" => {
            let alive = job.worker_pid.is_some_and(process_is_alive);
            if !alive {
                job.status = "failed".to_string();
                job.finished_at = Some(now());
                job.error = Some("更新代理未完成交接，DeepTutor 已恢复使用当前版本。".to_string());
                paths.write_job(&job)?;
                paths.release_active(&job.id);
            }
        }
        "succeeded" | "failed" => paths.release_active(&job.id),
        _ => {}
    }
    Ok(())
}

fn cleanup_success_artifacts(paths: &UpdatePaths, job_id: &str) {
    let _ = remove_path_if_exists(&paths.backups.join(job_id));
    let _ = remove_path_if_exists(&paths.jobs.join(job_id));
}

fn wait_for_process_exit(pid: u32, timeout: Duration) -> UpdateResult<()> {
    if pid == 0 || pid == std::process::id() {
        return Err("更新代理收到无效的父进程号".to_string());
    }
    let deadline = Instant::now() + timeout;
    while process_is_alive(pid) {
        if Instant::now() >= deadline {
            return Err("等待主程序退出超时，未修改任何程序文件".to_string());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn process_is_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        if pid == 0 {
            return false;
        }
        // SAFETY: 句柄由 OpenProcess 返回，并在读取退出码后立即关闭。
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut exit_code = 0u32;
            let success = GetExitCodeProcess(handle, &mut exit_code);
            CloseHandle(handle);
            success != 0 && exit_code == STILL_ACTIVE as u32
        }
    }
    #[cfg(unix)]
    {
        if pid == 0 {
            return false;
        }
        // SAFETY: kill(pid, 0) 只探测进程是否存在，不发送终止信号。
        let result = unsafe { libc::kill(pid as i32, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
        false
    }
}

fn rename_with_retry(source: &Path, destination: &Path, timeout_seconds: u64) -> UpdateResult<()> {
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    loop {
        match fs::rename(source, destination) {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
                ) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn remove_path_if_exists(path: &Path) -> UpdateResult<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("无法检查 {}：{error}", path.display())),
    };
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).map_err(|error| format!("无法删除 {}：{error}", path.display()))
    } else {
        fs::remove_file(path).map_err(|error| format!("无法删除 {}：{error}", path.display()))
    }
}

fn argument_value(arguments: &[String], name: &str) -> UpdateResult<String> {
    let positions = arguments
        .iter()
        .enumerate()
        .filter_map(|(index, value)| (value == name).then_some(index))
        .collect::<Vec<_>>();
    if positions.len() != 1 {
        return Err(format!("更新代理参数 {name} 缺失或重复"));
    }
    arguments
        .get(positions[0] + 1)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| format!("更新代理参数 {name} 缺少值"))
}

fn append_update_log(path: &Path, message: &str) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} {message}", now());
    }
}

fn truncate_message(value: &str, maximum_chars: usize) -> String {
    if value.chars().count() <= maximum_chars {
        value.to_string()
    } else {
        value.chars().take(maximum_chars).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn portable_fixture(root: &Path) -> PortablePaths {
        let data = root.join("data");
        PortablePaths {
            root: root.to_path_buf(),
            home: root.join("home"),
            data: data.clone(),
            cache: root.join("cache"),
            temp: root.join("temp"),
            logs: root.join("logs"),
            webview: root.join("cache").join("webview2"),
            uploads: data.join("uploads"),
            outputs: data.join("outputs"),
            database: data.join("deeptutor.sqlite3"),
        }
    }

    fn write_test_package(root: &Path, version: &str, executable: &str) {
        fs::create_dir_all(root.join("resources/web")).expect("创建资源目录");
        fs::write(root.join(executable), b"new executable").expect("写入测试可执行文件");
        fs::write(root.join("resources/web/index.html"), b"new web").expect("写入测试前端");
        let manifest = json!({
            "product": "DeepTutor",
            "version": version,
            "runtime": "rust",
            "architecture": architecture_label()
        });
        fs::write(
            root.join("portable-manifest.json"),
            serde_json::to_vec(&manifest).expect("序列化清单"),
        )
        .expect("写入清单");
    }

    #[test]
    fn versions_are_strict_and_ordered() {
        assert_eq!(normalize_stable_version("v1.7.0").unwrap(), "1.7.0");
        assert!(normalize_stable_version("1.7").is_err());
        assert!(version_tuple("1.8.0") > version_tuple("1.7.9"));
    }

    #[test]
    fn release_asset_urls_are_limited_to_the_official_release() {
        let valid = "https://github.com/HKUDS/DeepTutor/releases/download/v1.8.0/DeepTutor-1.8.0-portable-X64.zip";
        assert!(
            validate_release_asset_url(valid, "1.8.0", "DeepTutor-1.8.0-portable-X64.zip").is_ok()
        );
        let invalid = "https://github.com/attacker/DeepTutor/releases/download/v1.8.0/portable.zip";
        assert!(validate_release_asset_url(invalid, "1.8.0", "portable.zip").is_err());
    }

    #[test]
    fn checksum_parser_requires_a_matching_asset_name() {
        let root = std::env::temp_dir().join(format!("deeptutor-sha-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).expect("创建测试目录");
        let path = root.join("checksum");
        fs::write(
            &path,
            format!("{}  DeepTutor-1.8.0-portable-X64.zip\n", "a".repeat(64)),
        )
        .expect("写入校验文件");
        assert_eq!(
            parse_checksum_file(&path, "DeepTutor-1.8.0-portable-X64.zip").unwrap(),
            "a".repeat(64)
        );
        assert!(parse_checksum_file(&path, "other.zip").is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn job_state_can_be_replaced_atomically() {
        let root = std::env::temp_dir().join(format!("deeptutor-state-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).expect("创建测试目录");
        let path = root.join("state.json");
        write_json_atomic(&path, &json!({"value": 1})).expect("写入第一版状态");
        write_json_atomic(&path, &json!({"value": 2})).expect("替换状态");
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["value"], 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn install_and_rollback_keep_user_data_untouched() {
        let root = std::env::temp_dir().join(format!("deeptutor-install-test-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("resources/web")).expect("创建旧资源");
        fs::create_dir_all(root.join("data")).expect("创建数据目录");
        fs::write(root.join("DeepTutor.exe"), b"old executable").expect("写入旧程序");
        fs::write(root.join("resources/web/index.html"), b"old web").expect("写入旧前端");
        fs::write(root.join("data/user-data.txt"), b"keep me").expect("写入用户数据");
        let portable = portable_fixture(&root);
        let paths = UpdatePaths::new(&portable);
        paths.prepare().expect("创建更新目录");
        let package = paths.jobs.join("a".repeat(32)).join("package");
        write_test_package(&package, "1.8.0", &expected_executable_name());
        let job = UpdateJob {
            schema_version: 1,
            id: "a".repeat(32),
            status: "running".to_string(),
            current_version: "1.7.0".to_string(),
            target_version: "1.8.0".to_string(),
            created_at: now(),
            started_at: Some(now()),
            finished_at: None,
            error: None,
            restart_count: 0,
            release_url: "https://github.com/HKUDS/DeepTutor/releases/tag/v1.8.0".to_string(),
            asset_name: "DeepTutor-1.8.0-portable-X64.zip".to_string(),
            asset_url: "https://github.com/HKUDS/DeepTutor/releases/download/v1.8.0/DeepTutor-1.8.0-portable-X64.zip".to_string(),
            checksum_asset_name: "DeepTutor-1.8.0-portable-X64.zip.sha256".to_string(),
            checksum_asset_url: "https://github.com/HKUDS/DeepTutor/releases/download/v1.8.0/DeepTutor-1.8.0-portable-X64.zip.sha256".to_string(),
            executable_name: expected_executable_name(),
            worker_pid: None,
        };
        let transaction = install_portable_update(&root, &paths, &job).expect("安装测试更新");
        assert_eq!(
            fs::read(root.join("DeepTutor.exe")).unwrap(),
            b"new executable"
        );
        assert_eq!(
            fs::read(root.join("data/user-data.txt")).unwrap(),
            b"keep me"
        );
        transaction.rollback().expect("回滚测试更新");
        assert_eq!(
            fs::read(root.join("DeepTutor.exe")).unwrap(),
            b"old executable"
        );
        assert_eq!(
            fs::read(root.join("resources/web/index.html")).unwrap(),
            b"old web"
        );
        assert_eq!(
            fs::read(root.join("data/user-data.txt")).unwrap(),
            b"keep me"
        );
        let _ = fs::remove_dir_all(root);
    }
}
