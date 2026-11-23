//! Core HTTP transport, resilience (401 refresh-retry, 429/502/503/504 Retry-After backoff),
//! `$batch` execution, single-flight `/me` profile resolution, and streaming downloads.

use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use rand::Rng;
use reqwest::{Method, StatusCode};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use super::GraphClient;
use crate::models::UserProfile;

const MAX_BATCH_SIZE: usize = 20;
pub(crate) const MAX_THROTTLE_RETRIES: usize = 3;
const MAX_RETRY_AFTER_SECS: u64 = 30;
const STANDARD_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MEDIA_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Serialize)]
pub struct BatchSubRequest {
    pub id: String,
    pub method: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BatchSubResponse {
    pub id: String,
    pub status: u16,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub body: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct BatchResponseEnvelope {
    #[serde(default)]
    responses: Vec<BatchSubResponse>,
}

#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
pub(crate) struct GraphListPage<T> {
    #[serde(default = "Vec::new")]
    pub value: Vec<T>,
    #[serde(rename = "@odata.nextLink", default)]
    pub next_link: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphUser {
    id: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(default)]
    mail: Option<String>,
    #[serde(rename = "userPrincipalName", default)]
    user_principal_name: Option<String>,
    #[serde(rename = "jobTitle", default)]
    job_title: Option<String>,
    #[serde(default)]
    department: Option<String>,
}

impl From<GraphUser> for UserProfile {
    fn from(u: GraphUser) -> Self {
        let email = u
            .mail
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                u.user_principal_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            });
        let display_name = u
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| email.clone())
            .unwrap_or_else(|| u.id.clone());
        Self {
            id: u.id,
            display_name,
            email,
            job_title: u
                .job_title
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            department: u
                .department
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        }
    }
}

#[derive(Debug)]
pub(crate) enum GraphGetOrGone<T> {
    Ok(T),
    Gone {
        location: Option<String>,
        detail: String,
    },
}

#[derive(Clone)]
enum RequestBody {
    Empty,
    Json(serde_json::Value),
    Bytes {
        data: Arc<[u8]>,
        content_type: Option<String>,
    },
}

impl GraphClient {
    pub(crate) fn resolve_url(&self, path_or_url: &str) -> String {
        if path_or_url.starts_with("https://") || path_or_url.starts_with("http://") {
            return path_or_url.to_string();
        }
        format!(
            "{}/{}",
            self.config.graph_base_url.trim_end_matches('/'),
            path_or_url.trim_start_matches('/')
        )
    }

    pub async fn get_me(&self) -> Result<UserProfile> {
        let user: GraphUser = self
            .get_json("/me?$select=id,displayName,mail,userPrincipalName,jobTitle,department")
            .await?;
        Ok(user.into())
    }

    async fn resolve_my_user_id_inner(&self) -> Result<String> {
        if let Some(id) = self.my_user_id.as_deref().filter(|s| !s.trim().is_empty()) {
            return Ok(id.to_string());
        }
        if let Some(id) = self
            .cached_my_user_id
            .get()
            .filter(|s| !s.trim().is_empty())
        {
            return Ok(id.to_string());
        }
        let _guard = self.resolve_user_mutex.lock().await;
        if let Some(id) = self
            .cached_my_user_id
            .get()
            .filter(|s| !s.trim().is_empty())
        {
            return Ok(id.to_string());
        }
        if let Some(tok) = self.auth.current_tokens().await {
            if let Some(id) = tok.user_id.filter(|s| !s.trim().is_empty()) {
                let _ = self.cached_my_user_id.set(id.clone());
                return Ok(id);
            }
        }
        let me = self
            .get_me()
            .await
            .context("Failed to determine current user ID (/me).")?;
        if !me.id.trim().is_empty() {
            let _ = self.cached_my_user_id.set(me.id.clone());
        }
        let _ = self
            .auth
            .update_identity(me.id.clone(), me.display_name.clone(), me.email.clone())
            .await;
        Ok(me.id)
    }

    /// Resolves current user's Entra object ID using cached field, shared `OnceLock`, stored tokens, or `/me`.
    pub async fn resolve_my_user_id(&self) -> Option<String> {
        self.resolve_my_user_id_inner().await.ok()
    }

    pub async fn require_my_user_id(&self) -> Result<String> {
        self.resolve_my_user_id_inner().await
    }

    pub async fn get_json<T: DeserializeOwned>(&self, path_or_url: &str) -> Result<T> {
        self.get_json_with_headers(path_or_url, &[]).await
    }

    pub(crate) fn resolve_delta_url(&self, path_or_url: &str) -> Result<String> {
        let base = reqwest::Url::parse(&self.config.graph_base_url)
            .context("Invalid Graph API base URL")?;
        let resolved = reqwest::Url::parse(&self.resolve_url(path_or_url))
            .context("Invalid Graph delta URL")?;
        if resolved.origin() != base.origin()
            || !resolved.username().is_empty()
            || resolved.password().is_some()
        {
            anyhow::bail!("Graph delta URL does not belong to the configured Graph origin");
        }
        Ok(resolved.into())
    }

    /// Executes a Graph GET request that preserves HTTP `410 Gone` and its `Location` header
    /// for `driveItem: delta` resync handling.
    pub(crate) async fn get_json_or_gone<T: DeserializeOwned>(
        &self,
        path_or_url: &str,
    ) -> Result<GraphGetOrGone<T>> {
        let url = self.resolve_delta_url(path_or_url)?;
        let resp = self
            .execute_with_retry_inner(
                Method::GET,
                &url,
                RequestBody::Empty,
                &[],
                true,
                STANDARD_REQUEST_TIMEOUT,
                true,
            )
            .await?;
        if resp.status() == StatusCode::GONE {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let err_body = resp.text().await.unwrap_or_default();
            let detail = format_graph_error(StatusCode::GONE, &err_body);
            return Ok(GraphGetOrGone::Gone { location, detail });
        }
        let bytes = resp
            .bytes()
            .await
            .context("Failed to read Graph response body")?;
        let parsed = serde_json::from_slice(&bytes)
            .with_context(|| format!("Failed to deserialize Graph JSON ({url})"))?;
        Ok(GraphGetOrGone::Ok(parsed))
    }

    /// Executes a Graph GET request with extra headers, including 401 refresh-retry and 429/502/503/504 backoff.
    pub async fn get_json_with_headers<T: DeserializeOwned>(
        &self,
        path_or_url: &str,
        extra_headers: &[(&str, &str)],
    ) -> Result<T> {
        let url = self.resolve_url(path_or_url);
        let owned_headers: Vec<(&str, String)> = extra_headers
            .iter()
            .map(|(k, v)| (*k, (*v).to_string()))
            .collect();
        let resp = self
            .execute_with_retry(
                Method::GET,
                &url,
                RequestBody::Empty,
                &owned_headers,
                true,
                STANDARD_REQUEST_TIMEOUT,
            )
            .await?;
        let bytes = resp
            .bytes()
            .await
            .context("Failed to read Graph response body")?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("Failed to deserialize Graph JSON ({url})"))
    }

    pub async fn post_json<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path_or_url: &str,
        body: &B,
    ) -> Result<T> {
        self.post_json_with_headers(path_or_url, body, &[]).await
    }

    pub async fn post_json_with_headers<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path_or_url: &str,
        body: &B,
        extra_headers: &[(&str, String)],
    ) -> Result<T> {
        let url = self.resolve_url(path_or_url);
        let json_val = serde_json::to_value(body)?;
        let resp = self
            .execute_with_retry(
                Method::POST,
                &url,
                RequestBody::Json(json_val),
                extra_headers,
                true,
                STANDARD_REQUEST_TIMEOUT,
            )
            .await?;
        let bytes = resp
            .bytes()
            .await
            .context("Failed to read Graph response body")?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("Failed to deserialize Graph JSON ({url})"))
    }

    pub async fn post_json_empty<B: Serialize + ?Sized>(
        &self,
        path_or_url: &str,
        body: &B,
    ) -> Result<()> {
        let url = self.resolve_url(path_or_url);
        let json_val = serde_json::to_value(body)?;
        self.execute_with_retry(
            Method::POST,
            &url,
            RequestBody::Json(json_val),
            &[],
            true,
            STANDARD_REQUEST_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    pub async fn patch_json<B: Serialize + ?Sized>(
        &self,
        path_or_url: &str,
        body: &B,
    ) -> Result<()> {
        self.patch_json_response(path_or_url, body, &[]).await?;
        Ok(())
    }

    /// Executes a Graph PATCH request with JSON body and deserializes an optional JSON response (returning `None` on empty/204 body).
    pub async fn patch_json_optional<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path_or_url: &str,
        body: &B,
    ) -> Result<Option<T>> {
        let resp = self.patch_json_response(path_or_url, body, &[]).await?;
        let bytes = resp
            .bytes()
            .await
            .context("Failed to read Graph PATCH response body")?;
        if bytes.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(None);
        }
        let parsed = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "Failed to deserialize Graph PATCH JSON ({})",
                self.resolve_url(path_or_url)
            )
        })?;
        Ok(Some(parsed))
    }

    pub(crate) async fn patch_json_response<B: Serialize + ?Sized>(
        &self,
        path_or_url: &str,
        body: &B,
        extra_headers: &[(&str, String)],
    ) -> Result<reqwest::Response> {
        let url = self.resolve_url(path_or_url);
        self.execute_with_retry(
            Method::PATCH,
            &url,
            RequestBody::Json(serde_json::to_value(body)?),
            extra_headers,
            true,
            STANDARD_REQUEST_TIMEOUT,
        )
        .await
    }

    pub async fn post_empty(&self, path_or_url: &str) -> Result<()> {
        let url = self.resolve_url(path_or_url);
        self.execute_with_retry(
            Method::POST,
            &url,
            RequestBody::Empty,
            &[],
            true,
            STANDARD_REQUEST_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    pub async fn delete_req(&self, path_or_url: &str) -> Result<()> {
        let url = self.resolve_url(path_or_url);
        self.execute_with_retry(
            Method::DELETE,
            &url,
            RequestBody::Empty,
            &[],
            true,
            STANDARD_REQUEST_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    pub async fn put_bytes<T: DeserializeOwned>(
        &self,
        path_or_url: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<T> {
        self.put_bytes_with_headers(path_or_url, bytes, content_type, &[])
            .await
    }

    pub async fn put_bytes_with_headers<T: DeserializeOwned>(
        &self,
        path_or_url: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
        extra_headers: &[(&str, String)],
    ) -> Result<T> {
        let url = self.resolve_url(path_or_url);
        let resp = self
            .execute_with_retry(
                Method::PUT,
                &url,
                RequestBody::Bytes {
                    data: Arc::from(bytes),
                    content_type: content_type.map(str::to_string),
                },
                extra_headers,
                true,
                MEDIA_REQUEST_TIMEOUT,
            )
            .await?;
        let resp_bytes = resp
            .bytes()
            .await
            .context("Failed to read Graph PUT response body")?;
        serde_json::from_slice(&resp_bytes)
            .with_context(|| format!("Failed to deserialize Graph PUT JSON ({url})"))
    }

    /// Uploads a chunk to a pre-authenticated `createUploadSession` URL without Bearer header.
    pub(crate) async fn put_upload_chunk(
        &self,
        upload_url: &str,
        chunk: &[u8],
        start: u64,
        end_inclusive: u64,
        total: u64,
    ) -> Result<(StatusCode, serde_json::Value)> {
        let headers = vec![
            (
                "Content-Range",
                format!("bytes {start}-{end_inclusive}/{total}"),
            ),
            ("Content-Length", chunk.len().to_string()),
        ];
        let resp = self
            .execute_with_retry(
                Method::PUT,
                upload_url,
                RequestBody::Bytes {
                    data: Arc::from(chunk),
                    content_type: Some("application/octet-stream".to_string()),
                },
                &headers,
                false,
                MEDIA_REQUEST_TIMEOUT,
            )
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let val = if text.trim().is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
        };
        Ok((status, val))
    }

    pub async fn get_bytes(&self, path_or_url: &str) -> Result<Vec<u8>> {
        let url = self.resolve_url(path_or_url);
        let resp = self
            .execute_with_retry(
                Method::GET,
                &url,
                RequestBody::Empty,
                &[],
                true,
                MEDIA_REQUEST_TIMEOUT,
            )
            .await?;
        let bytes = resp
            .bytes()
            .await
            .context("Failed to read Graph binary response")?;
        Ok(bytes.to_vec())
    }

    pub async fn get_bytes_with_limit(
        &self,
        url_or_path: &str,
        use_bearer: bool,
        max_bytes: usize,
    ) -> Result<Vec<u8>> {
        let url = self.resolve_url(url_or_path);
        let resp = self
            .execute_with_retry(
                Method::GET,
                &url,
                RequestBody::Empty,
                &[],
                use_bearer,
                MEDIA_REQUEST_TIMEOUT,
            )
            .await?;
        if let Some(len) = resp.content_length() {
            if len > max_bytes as u64 {
                bail!(
                    "Response size ({len} bytes) exceeds in-memory limit ({max_bytes} bytes) for {url}"
                );
            }
        }
        let mut out = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk_res) = stream.next().await {
            let chunk = chunk_res.context("Failed to read Graph binary stream chunk")?;
            if out.len().saturating_add(chunk.len()) > max_bytes {
                bail!(
                    "Response body exceeded in-memory limit ({max_bytes} bytes) for {url}"
                );
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// Streams a Graph or external asset download directly to `dest_path` using a 64 KiB `BufWriter`.
    pub async fn get_to_file(
        &self,
        url_or_path: &str,
        dest_path: &Path,
        use_bearer: bool,
    ) -> Result<u64> {
        let url = self.resolve_url(url_or_path);
        let resp = self
            .execute_with_retry(
                Method::GET,
                &url,
                RequestBody::Empty,
                &[],
                use_bearer,
                MEDIA_REQUEST_TIMEOUT,
            )
            .await?;

        let file = tokio::fs::File::create(dest_path)
            .await
            .with_context(|| format!("Failed to create destination file: {}", dest_path.display()))?;
        let mut writer = tokio::io::BufWriter::with_capacity(64 * 1024, file);
        let mut stream = resp.bytes_stream();
        let mut total_written: u64 = 0;

        while let Some(chunk_res) = stream.next().await {
            let chunk = match chunk_res {
                Ok(c) => c,
                Err(err) => {
                    let _ = tokio::fs::remove_file(dest_path).await;
                    return Err(err).context("Failed to stream download chunk");
                }
            };
            if let Err(err) = writer.write_all(&chunk).await {
                let _ = tokio::fs::remove_file(dest_path).await;
                return Err(err).with_context(|| {
                    format!("Failed to write downloaded bytes to {}", dest_path.display())
                });
            }
            total_written += chunk.len() as u64;
        }
        if let Err(err) = writer.flush().await {
            let _ = tokio::fs::remove_file(dest_path).await;
            return Err(err).with_context(|| {
                format!("Failed to flush downloaded file {}", dest_path.display())
            });
        }
        Ok(total_written)
    }

    pub async fn get_text(&self, path_or_url: &str) -> Result<String> {
        let url = self.resolve_url(path_or_url);
        let resp = self
            .execute_with_retry(
                Method::GET,
                &url,
                RequestBody::Empty,
                &[],
                true,
                STANDARD_REQUEST_TIMEOUT,
            )
            .await?;
        resp.text().await.context("Failed to read Graph text response")
    }

    pub async fn post_bytes_json<T: DeserializeOwned>(
        &self,
        path_or_url: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<T> {
        let url = self.resolve_url(path_or_url);
        let resp = self
            .execute_with_retry(
                Method::POST,
                &url,
                RequestBody::Bytes {
                    data: Arc::from(bytes),
                    content_type: Some(content_type.to_string()),
                },
                &[],
                true,
                STANDARD_REQUEST_TIMEOUT,
            )
            .await?;
        let resp_bytes = resp
            .bytes()
            .await
            .context("Failed to read Graph POST response body")?;
        serde_json::from_slice(&resp_bytes)
            .with_context(|| format!("Failed to deserialize Graph POST JSON ({url})"))
    }

    pub async fn patch_bytes_empty(
        &self,
        path_or_url: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<()> {
        let url = self.resolve_url(path_or_url);
        self.execute_with_retry(
            Method::PATCH,
            &url,
            RequestBody::Bytes {
                data: Arc::from(bytes),
                content_type: Some(content_type.to_string()),
            },
            &[],
            true,
            STANDARD_REQUEST_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    /// Executes `POST /$batch` in chunks of up to 20 sub-requests.
    pub async fn batch(&self, requests: &[BatchSubRequest]) -> Result<Vec<BatchSubResponse>> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let mut all = Vec::with_capacity(requests.len());
        for chunk in requests.chunks(MAX_BATCH_SIZE) {
            let payload = serde_json::json!({ "requests": chunk });
            let env: BatchResponseEnvelope = self.post_json("/$batch", &payload).await?;
            all.extend(env.responses);
        }
        Ok(all)
    }

    async fn execute_with_retry(
        &self,
        method: Method,
        url: &str,
        body: RequestBody,
        extra_headers: &[(&str, String)],
        use_bearer: bool,
        timeout: Duration,
    ) -> Result<reqwest::Response> {
        self.execute_with_retry_inner(method, url, body, extra_headers, use_bearer, timeout, false)
            .await
    }

    async fn execute_with_retry_inner(
        &self,
        method: Method,
        url: &str,
        body: RequestBody,
        extra_headers: &[(&str, String)],
        use_bearer: bool,
        timeout: Duration,
        allow_gone: bool,
    ) -> Result<reqwest::Response> {
        let mut unauthorized_retried = false;
        let mut throttle_attempt = 0usize;
        let mut transport_retried = false;

        loop {
            let mut token_used = None;
            let mut req = self.http.request(method.clone(), url).timeout(timeout);

            if use_bearer {
                let token = self.auth.valid_access_token().await?;
                req = req.bearer_auth(&token);
                token_used = Some(token);
            }

            for (name, val) in extra_headers {
                req = req.header(*name, val.as_str());
            }

            match &body {
                RequestBody::Empty => {}
                RequestBody::Json(val) => {
                    req = req.json(val);
                }
                RequestBody::Bytes { data, content_type } => {
                    if let Some(ct) = content_type {
                        req = req.header(reqwest::header::CONTENT_TYPE, ct.as_str());
                    }
                    req = req.body(data.to_vec());
                }
            }

            let resp = match req.send().await {
                Ok(r) => r,
                Err(err) => {
                    if method == Method::GET
                        && !transport_retried
                        && (err.is_connect() || err.is_timeout())
                    {
                        transport_retried = true;
                        let jitter_ms: u64 = rand::thread_rng().gen_range(0..=250);
                        tokio::time::sleep(Duration::from_millis(100 + jitter_ms)).await;
                        continue;
                    }
                    return Err(err)
                        .with_context(|| format!("Graph HTTP request failed ({method} {url})"))
                        .map_err(crate::net::annotate_tls_error);
                }
            };
            let status = resp.status();

            if status == StatusCode::UNAUTHORIZED && use_bearer && !unauthorized_retried {
                unauthorized_retried = true;
                if let Some(failed_tok) = token_used.as_deref() {
                    if self.auth.is_refreshable().await {
                        self.auth.refresh_for_failed_token(failed_tok).await?;
                        continue;
                    } else if self.auth.refresh_for_failed_token(failed_tok).await.is_ok() {
                        continue;
                    }
                }
            }

            if matches!(
                status,
                StatusCode::TOO_MANY_REQUESTS
                    | StatusCode::SERVICE_UNAVAILABLE
                    | StatusCode::BAD_GATEWAY
                    | StatusCode::GATEWAY_TIMEOUT
            ) && throttle_attempt < MAX_THROTTLE_RETRIES
            {
                let delay_secs = parse_retry_after_secs(resp.headers(), throttle_attempt);
                throttle_attempt += 1;
                let jitter_ms: u64 = rand::thread_rng().gen_range(0..=250);
                tokio::time::sleep(
                    Duration::from_secs(delay_secs) + Duration::from_millis(jitter_ms),
                )
                .await;
                continue;
            }

            if allow_gone && status == StatusCode::GONE {
                return Ok(resp);
            }

            if !status.is_success() {
                let err_body = resp.text().await.unwrap_or_default();
                let detail = format_graph_error(status, &err_body);
                bail!("{}{method} {url}): {detail}", crate::graph::GRAPH_API_ERROR_PREFIX);
            }

            return Ok(resp);
        }
    }
}

pub(crate) fn parse_retry_after_secs(
    headers: &reqwest::header::HeaderMap,
    attempt: usize,
) -> u64 {
    if let Some(raw) = headers.get(reqwest::header::RETRY_AFTER) {
        if let Ok(s) = raw.to_str() {
            if let Ok(secs) = s.trim().parse::<u64>() {
                return secs.clamp(1, MAX_RETRY_AFTER_SECS);
            }
        }
    }
    match attempt {
        0 => 1,
        1 => 2,
        _ => 4,
    }
}

fn format_graph_error(status: StatusCode, body: &str) -> String {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(err) = val.get("error") {
            let code = err.get("code").and_then(|v| v.as_str()).unwrap_or("");
            let msg = err.get("message").and_then(|v| v.as_str()).unwrap_or("");
            if !code.is_empty() || !msg.is_empty() {
                return format!("{status} [{code}] {msg}");
            }
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        status.to_string()
    } else {
        format!("{status} {trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthManager;
    use crate::config::AppConfig;

    #[test]
    fn test_resolve_url_relative_and_absolute() {
        let mut cfg = AppConfig::default();
        cfg.graph_base_url = "https://graph.microsoft.com/v1.0/".to_string();
        let auth = AuthManager::from_tokens(
            cfg.clone(),
            crate::auth::StoredTokens {
                access_token: "t".into(),
                refresh_token: None,
                expires_at: 9_999_999_999,
                client_id: "c".into(),
                tenant_id: "t".into(),
                user_id: None,
                user_display_name: None,
                user_email: None,
                auth_mode: "access_token".into(),
            },
        );
        let client = GraphClient::new(cfg, auth).unwrap();
        assert_eq!(
            client.resolve_url("/me"),
            "https://graph.microsoft.com/v1.0/me"
        );
        assert_eq!(
            client.resolve_url("me/messages"),
            "https://graph.microsoft.com/v1.0/me/messages"
        );
        assert_eq!(
            client.resolve_url("https://graph.microsoft.com/v1.0/me/messages?$skiptoken=abc"),
            "https://graph.microsoft.com/v1.0/me/messages?$skiptoken=abc"
        );
        assert_eq!(
            client.resolve_url("http://127.0.0.1:8080/next"),
            "http://127.0.0.1:8080/next"
        );
    }

    #[test]
    fn test_retry_after_header_parsing() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after_secs(&headers, 0), 1);
        assert_eq!(parse_retry_after_secs(&headers, 1), 2);
        assert_eq!(parse_retry_after_secs(&headers, 2), 4);

        headers.insert(reqwest::header::RETRY_AFTER, "7".parse().unwrap());
        assert_eq!(parse_retry_after_secs(&headers, 0), 7);
    }

    #[tokio::test]
    async fn test_graph_resilience_401_429_and_batch_chunking() {
        use axum::{
            http::{HeaderMap, StatusCode},
            routing::{get, post},
            Json, Router,
        };
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use tempfile::tempdir;

        let throttle_calls = Arc::new(AtomicUsize::new(0));
        let throttle_clone = throttle_calls.clone();
        let batch_calls = Arc::new(AtomicUsize::new(0));
        let batch_clone = batch_calls.clone();

        let app = Router::new()
            .route(
                "/tid/oauth2/v2.0/token",
                post(|| async {
                    Json(serde_json::json!({
                        "access_token": "fresh-token",
                        "refresh_token": "rt-2",
                        "expires_in": 3600
                    }))
                }),
            )
            .route(
                "/v1.0/me",
                get(|headers: HeaderMap| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if auth == "Bearer fresh-token" {
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "id": "u-1",
                                "displayName": "Alice",
                                "mail": "alice@example.com",
                                "jobTitle": "Staff Engineer",
                                "department": "Core"
                            })),
                        )
                    } else {
                        (
                            StatusCode::UNAUTHORIZED,
                            Json(serde_json::json!({
                                "error": { "code": "InvalidAuthenticationToken", "message": "expired" }
                            })),
                        )
                    }
                }),
            )
            .route(
                "/v1.0/throttled",
                get(move || {
                    let calls = throttle_clone.clone();
                    async move {
                        let n = calls.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            (
                                StatusCode::TOO_MANY_REQUESTS,
                                [("Retry-After", "1")],
                                Json(serde_json::json!({ "error": { "code": "TooManyRequests" } })),
                            )
                        } else {
                            (
                                StatusCode::OK,
                                [("Retry-After", "0")],
                                Json(serde_json::json!({ "ok": true })),
                            )
                        }
                    }
                }),
            )
            .route(
                "/v1.0/$batch",
                post(move |Json(body): Json<serde_json::Value>| {
                    let calls = batch_clone.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        let reqs = body["requests"].as_array().unwrap();
                        assert!(reqs.len() <= 20);
                        let responses: Vec<serde_json::Value> = reqs
                            .iter()
                            .map(|r| {
                                serde_json::json!({
                                    "id": r["id"],
                                    "status": 200,
                                    "body": { "echo": r["url"] }
                                })
                            })
                            .collect();
                        Json(serde_json::json!({ "responses": responses }))
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let dir = tempdir().unwrap();
        let mut cfg = AppConfig::default();
        cfg.auth_base_url = format!("http://{addr}");
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        cfg.tenant_id = "tid".into();

        let tok = crate::auth::StoredTokens {
            access_token: "stale-token".into(),
            refresh_token: Some("rt-1".into()),
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".into(),
            tenant_id: "tid".into(),
            user_id: None,
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".into(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tok)
            .with_token_path(dir.path().join("creds.json"));
        let client = GraphClient::new(cfg, auth).unwrap();

        let me = client.get_me().await.unwrap();
        assert_eq!(me.id, "u-1");
        assert_eq!(me.display_name, "Alice");
        assert_eq!(me.email.as_deref(), Some("alice@example.com"));
        assert_eq!(me.job_title.as_deref(), Some("Staff Engineer"));
        assert_eq!(me.department.as_deref(), Some("Core"));

        let val: serde_json::Value = client.get_json("/throttled").await.unwrap();
        assert_eq!(val["ok"], true);
        assert_eq!(throttle_calls.load(Ordering::SeqCst), 2);

        let sub_reqs: Vec<BatchSubRequest> = (0..25)
            .map(|i| BatchSubRequest {
                id: i.to_string(),
                method: "GET".into(),
                url: format!("/items/{i}"),
                body: None,
                headers: None,
            })
            .collect();
        let batch_resps = client.batch(&sub_reqs).await.unwrap();
        assert_eq!(batch_resps.len(), 25);
        assert_eq!(batch_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_retry_after_cap_at_30_secs() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "3600".parse().unwrap());
        assert_eq!(parse_retry_after_secs(&headers, 0), 30);

        headers.insert(reqwest::header::RETRY_AFTER, "0".parse().unwrap());
        assert_eq!(parse_retry_after_secs(&headers, 0), 1);
    }

    #[tokio::test]
    async fn test_502_504_retries_and_single_flight_resolve_my_user_id() {
        use axum::{http::StatusCode, routing::get, Json, Router};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let gateway_calls = Arc::new(AtomicUsize::new(0));
        let gw_clone = gateway_calls.clone();
        let me_calls = Arc::new(AtomicUsize::new(0));
        let me_clone = me_calls.clone();

        let app = Router::new()
            .route(
                "/v1.0/gateway-flaky",
                get(move || {
                    let calls = gw_clone.clone();
                    async move {
                        let n = calls.fetch_add(1, Ordering::SeqCst);
                        match n {
                            0 => (
                                StatusCode::BAD_GATEWAY,
                                [("Retry-After", "0")],
                                Json(serde_json::json!({ "error": { "code": "BadGateway" } })),
                            ),
                            1 => (
                                StatusCode::GATEWAY_TIMEOUT,
                                [("Retry-After", "0")],
                                Json(serde_json::json!({ "error": { "code": "GatewayTimeout" } })),
                            ),
                            _ => (
                                StatusCode::OK,
                                [("Retry-After", "0")],
                                Json(serde_json::json!({ "ok": true })),
                            ),
                        }
                    }
                }),
            )
            .route(
                "/v1.0/me",
                get(move || {
                    let calls = me_clone.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        Json(serde_json::json!({
                            "id": "single-flight-u1",
                            "displayName": "Single Flight",
                            "mail": "sf@example.com"
                        }))
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = AppConfig::default();
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tok = crate::auth::StoredTokens {
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: None,
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tok);
        let client = GraphClient::new(cfg, auth).unwrap();

        let gw_res: serde_json::Value = client.get_json("/gateway-flaky").await.unwrap();
        assert_eq!(gw_res["ok"], true);
        assert_eq!(gateway_calls.load(Ordering::SeqCst), 3);

        let (r1, r2, r3, r4) = tokio::join!(
            client.resolve_my_user_id(),
            client.resolve_my_user_id(),
            client.resolve_my_user_id(),
            client.resolve_my_user_id(),
        );
        assert_eq!(r1.as_deref(), Some("single-flight-u1"));
        assert_eq!(r2.as_deref(), Some("single-flight-u1"));
        assert_eq!(r3.as_deref(), Some("single-flight-u1"));
        assert_eq!(r4.as_deref(), Some("single-flight-u1"));
        assert_eq!(me_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_get_bytes_with_limit_rejects_oversized_response() {
        use axum::{routing::get, Router};

        let app = Router::new().route("/v1.0/big", get(|| async { vec![b'x'; 2048] }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = AppConfig::default();
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tok = crate::auth::StoredTokens {
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: Some("u1".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tok);
        let client = GraphClient::new(cfg, auth).unwrap();

        let err = client
            .get_bytes_with_limit("/big", true, 1024)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exceeds in-memory limit"));
    }
}
