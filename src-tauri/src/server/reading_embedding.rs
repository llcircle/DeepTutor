use super::*;
use tokio::time::Instant;

const REQUEST_INTERVAL: Duration = Duration::from_secs(2);
const MAX_INTERVAL: Duration = Duration::from_secs(30);
const QUERY_WAIT: Duration = Duration::from_secs(2);
const CANCELLED: &str = "向量任务已取消。";

#[derive(Default)]
pub(super) struct RequestRuntime {
    services: parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<ServiceRate>>>>,
}

struct ServiceRate {
    next_request: Instant,
    interval: Duration,
    limited: bool,
}

impl Default for ServiceRate {
    fn default() -> Self {
        Self {
            next_request: Instant::now(),
            interval: REQUEST_INTERVAL,
            limited: false,
        }
    }
}

impl RequestRuntime {
    fn service(&self, config: &EmbeddingConfig) -> Arc<tokio::sync::Mutex<ServiceRate>> {
        // 同一地址和凭据共用节流器，模型或资料集切换不能绕过限流；不保存明文密钥作为键。
        let key = revisions::hash(
            &json!([
                config.endpoint,
                config.profile["api_key"],
                super::super::super::catalog::extra_headers(&config.profile)
            ])
            .to_string(),
        );
        self.services
            .lock()
            .entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(ServiceRate::default())))
            .clone()
    }
}

fn retry_after(
    value: Option<&HeaderValue>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<Duration> {
    let value = value?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    Some(
        (at.with_timezone(&chrono::Utc) - now)
            .to_std()
            .unwrap_or(Duration::ZERO),
    )
}

fn retry_delay(status: StatusCode, attempt: u32, requested: Option<Duration>) -> Duration {
    let base = if status == StatusCode::TOO_MANY_REQUESTS {
        Duration::from_secs(5 * (1 << attempt.min(4)))
    } else {
        Duration::from_millis(500 * (1 << attempt.min(4)))
    };
    base.max(requested.unwrap_or_default())
}

fn progress(state: &AppState, workspace: Option<&str>, waiting: Option<(Duration, bool)>) {
    let Some(id) = workspace else { return };
    let Ok(mut value) = status(state, id) else {
        return;
    };
    value.as_object_mut().unwrap().remove("error");
    if let Some((delay, limited)) = waiting {
        value["status"] = json!("waiting");
        value["wait_reason"] = json!(if limited {
            "向量服务限流（HTTP 429），降速后自动继续"
        } else {
            "正在按请求间隔等待向量服务"
        });
        value["retry_at"] = json!(chrono::Duration::from_std(delay)
            .ok()
            .and_then(|delay| chrono::Utc::now().checked_add_signed(delay))
            .map(|time| time.to_rfc3339()));
    } else {
        value["status"] = json!("indexing");
        value.as_object_mut().unwrap().remove("wait_reason");
        value.as_object_mut().unwrap().remove("retry_at");
    }
    value["updated_at"] = json!(crate::storage::now());
    let _ = save_status(state, id, &value);
}

pub(super) async fn embed(
    state: &AppState,
    config: &EmbeddingConfig,
    texts: &[String],
    query: bool,
    cancel: &CancellationToken,
    workspace: Option<&str>,
) -> Result<Vec<Vec<f32>>, String> {
    let service = state.reading_index.requests.service(config);
    let mut all = Vec::new();
    let mut dimension = None;
    for batch in texts.chunks(32) {
        let body = if config.binding == "cohere" {
            json!({"model":config.model,"texts":batch,"input_type":if query {"search_query"} else {"search_document"},"embedding_types":["float"]})
        } else {
            json!({"model":config.model,"input":batch})
        };
        let mut attempt = 0;
        let payload = loop {
            let mut rate = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(CANCELLED.into()),
                guard = async {
                    if query {
                        tokio::time::timeout(QUERY_WAIT, service.lock()).await
                            .map_err(|_| "向量服务正在处理其他请求，本次使用关键词检索。".to_string())
                    } else { Ok(service.lock().await) }
                } => guard?,
            };
            let delay = rate.next_request.saturating_duration_since(Instant::now());
            if query && ((rate.limited && !delay.is_zero()) || delay > QUERY_WAIT) {
                return Err("向量服务限流冷却中，本次使用关键词检索。".into());
            }
            if delay >= Duration::from_millis(100) {
                progress(state, workspace, Some((delay, rate.limited)));
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(CANCELLED.into()),
                _ = tokio::time::sleep(delay) => {}
            }
            progress(state, workspace, None);
            // 请求开始前就预留间隔，网络失败、取消或返回坏数据也不能形成请求突发。
            rate.next_request = Instant::now() + rate.interval;
            let mut request = state
                .client
                .post(&config.endpoint)
                .timeout(Duration::from_secs(if query { 15 } else { 60 }))
                .json(&body);
            if let Some(key) = config.profile["api_key"]
                .as_str()
                .filter(|s| !s.is_empty() && *s != "********")
            {
                request = request.bearer_auth(key);
            }
            for (name, value) in super::super::super::catalog::extra_headers(&config.profile) {
                request = request.header(name, value);
            }
            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(CANCELLED.into()),
                response = request.send() => response.map_err(|e| format!("无法连接配置的向量服务：{e}"))?
            };
            let code = response.status();
            if code == StatusCode::TOO_MANY_REQUESTS || code.is_server_error() {
                let requested = retry_after(
                    response.headers().get(header::RETRY_AFTER),
                    chrono::Utc::now(),
                );
                let delay = retry_delay(code, attempt, requested).max(rate.interval);
                if code == StatusCode::TOO_MANY_REQUESTS {
                    rate.interval = (rate.interval * 2).min(MAX_INTERVAL);
                    rate.limited = true;
                }
                rate.next_request = Instant::now().checked_add(delay).ok_or_else(|| {
                    "向量服务要求的等待时间过长，请检查服务配置和配额。".to_string()
                })?;
                let max_retries = if code == StatusCode::TOO_MANY_REQUESTS {
                    4
                } else {
                    3
                };
                if !query && attempt < max_retries {
                    attempt += 1;
                    continue;
                }
            }
            if !code.is_success() {
                return Err(if code == StatusCode::TOO_MANY_REQUESTS {
                    "向量服务持续限流（HTTP 429），已保留完成页面和关键词检索。请稍后点击更新索引继续；若持续出现，请检查服务配额。".into()
                } else {
                    format!("向量服务返回 HTTP {code}，已保留关键词检索。")
                });
            }
            if response.content_length().unwrap_or(0) > 32 * 1024 * 1024 {
                return Err("向量响应超过 32 MB 限制。".into());
            }
            let bytes = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(CANCELLED.into()),
                bytes = response.bytes() => bytes.map_err(|e| format!("无法读取向量响应：{e}"))?
            };
            rate.next_request = Instant::now() + rate.interval;
            rate.limited = false;
            if bytes.len() > 32 * 1024 * 1024 {
                return Err("向量响应超过 32 MB 限制。".into());
            }
            break serde_json::from_slice::<Value>(&bytes)
                .map_err(|_| "向量服务返回无效 JSON。".to_string())?;
        };
        let vectors = decode_vectors(&payload, batch.len())?;
        let size = vectors.first().map(Vec::len).unwrap_or(0);
        if dimension.is_some_and(|d| d != size) {
            return Err("向量服务在不同批次返回了不同维度。".into());
        }
        dimension = Some(size);
        all.extend(vectors);
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(endpoint: &str) -> EmbeddingConfig {
        EmbeddingConfig {
            profile: json!({"api_key":"local-rate-fixture"}),
            model: "bge-m3".into(),
            endpoint: endpoint.into(),
            binding: "openai".into(),
            signature: "rate-test".into(),
        }
    }

    #[test]
    fn 等待头兼容秒数和日期并拒绝非法内容() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-04T04:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        for (input, seconds) in [
            ("12", 12),
            ("0", 0),
            ("Wed, 04 Nov 2026 04:00:15 GMT", 15),
            ("Wed, 04 Nov 2026 03:59:00 GMT", 0),
        ] {
            assert_eq!(
                retry_after(Some(&HeaderValue::from_str(input).unwrap()), now),
                Some(Duration::from_secs(seconds))
            );
        }
        assert!(retry_after(None, now).is_none());
        for input in ["invalid", "-1", "1.5"] {
            assert!(retry_after(Some(&HeaderValue::from_str(input).unwrap()), now).is_none());
        }
    }

    #[test]
    fn 限流退避逐次增加且不早于服务端要求() {
        assert_eq!(REQUEST_INTERVAL, Duration::from_secs(2));
        for (attempt, seconds) in [(0, 5), (1, 10), (2, 20), (3, 40)] {
            assert_eq!(
                retry_delay(StatusCode::TOO_MANY_REQUESTS, attempt, None),
                Duration::from_secs(seconds)
            );
        }
        assert_eq!(
            retry_delay(
                StatusCode::TOO_MANY_REQUESTS,
                0,
                Some(Duration::from_secs(60))
            ),
            Duration::from_secs(60)
        );
        assert_eq!(
            retry_delay(StatusCode::SERVICE_UNAVAILABLE, 0, None),
            Duration::from_millis(500)
        );
        assert_eq!(
            retry_delay(
                StatusCode::SERVICE_UNAVAILABLE,
                0,
                Some(Duration::from_secs(10))
            ),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn 相同地址和凭据跨模型共用节流但不同账户隔离() {
        let runtime = RequestRuntime::default();
        let first = config("http://127.0.0.1/custom/embeddings");
        let mut second = first.clone();
        second.model = "other-model".into();
        second.signature = "other-signature".into();
        assert!(Arc::ptr_eq(
            &runtime.service(&first),
            &runtime.service(&second)
        ));
        second.profile["api_key"] = json!("another-account");
        assert!(!Arc::ptr_eq(
            &runtime.service(&first),
            &runtime.service(&second)
        ));
    }

    #[tokio::test]
    async fn 限流冷却期间查询立即降级且等待可以取消() {
        let (state, _directory) = super::super::super::tests::pdf_state();
        let config = config("http://127.0.0.1:9/embeddings");
        let service = state.reading_index.requests.service(&config);
        {
            let mut rate = service.lock().await;
            rate.next_request = Instant::now() + Duration::from_secs(60);
            rate.limited = true;
        }
        let start = Instant::now();
        let error = embed(
            &state,
            &config,
            &["问题".into()],
            true,
            &CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();
        assert!(error.contains("限流冷却"));
        assert!(start.elapsed() < Duration::from_millis(200));
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let cancellation = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            token.cancel();
        });
        let error = embed(&state, &config, &["文档".into()], false, &cancel, None)
            .await
            .unwrap_err();
        assert_eq!(error, CANCELLED);
        assert!(start.elapsed() < Duration::from_secs(1));
        cancellation.await.unwrap();
    }

    #[tokio::test]
    async fn 并发请求串行且遵守请求间隔() {
        use axum::extract::State;
        let (state, _directory) = super::super::super::tests::pdf_state();
        let calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let app = Router::new()
            .route(
                "/embeddings",
                post(
                    |State(calls): State<Arc<parking_lot::Mutex<Vec<Instant>>>>| async move {
                        calls.lock().push(Instant::now());
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        Json(json!({"data":[{"index":0,"embedding":[1,0]}]}))
                    },
                ),
            )
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = config(&format!(
            "http://{}/embeddings",
            listener.local_addr().unwrap()
        ));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        // 缩短本测试的正常间隔；最终 EXE 验收仍使用真实两秒间隔。
        state
            .reading_index
            .requests
            .service(&config)
            .lock()
            .await
            .interval = Duration::from_millis(50);
        let cancel = CancellationToken::new();
        let texts = ["测试".into()];
        let (first, second) = tokio::join!(
            embed(&state, &config, &texts, false, &cancel, None),
            embed(&state, &config, &texts, false, &cancel, None)
        );
        first.unwrap();
        second.unwrap();
        let calls = calls.lock();
        assert_eq!(calls.len(), 2);
        assert!(calls[1].duration_since(calls[0]) >= Duration::from_millis(80));
        server.abort();
    }

    #[tokio::test]
    async fn 收到限流后按等待头重试并自适应降低后续频率() {
        use axum::extract::State;
        use axum::response::IntoResponse;
        let (state, _directory) = super::super::super::tests::pdf_state();
        let calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let app = Router::new()
            .route(
                "/embeddings",
                post(
                    |State(calls): State<Arc<parking_lot::Mutex<Vec<Instant>>>>| async move {
                        let count = {
                            let mut calls = calls.lock();
                            calls.push(Instant::now());
                            calls.len()
                        };
                        if count == 1 {
                            return (
                                StatusCode::TOO_MANY_REQUESTS,
                                [(header::RETRY_AFTER, "6")],
                                "限流",
                            )
                                .into_response();
                        }
                        Json(json!({"data":[{"index":0,"embedding":[1,0]}]})).into_response()
                    },
                ),
            )
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = config(&format!(
            "http://{}/embeddings",
            listener.local_addr().unwrap()
        ));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        embed(
            &state,
            &config,
            &["文档".into()],
            false,
            &CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        {
            let calls = calls.lock();
            assert_eq!(calls.len(), 2);
            assert!(calls[1].duration_since(calls[0]) >= Duration::from_secs(6));
        }
        assert_eq!(
            state
                .reading_index
                .requests
                .service(&config)
                .lock()
                .await
                .interval,
            Duration::from_secs(4)
        );
        server.abort();
    }
}
