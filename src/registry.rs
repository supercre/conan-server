use crate::{
    model::{Claim, Report, Target, safe_component},
    store::{Scope, Store},
};
use anyhow::Result;
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, State as AxumState},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use subtle::ConstantTimeEq;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

#[derive(Clone)]
pub struct State(Arc<Inner>);
struct Inner {
    store: Store,
    targets: Vec<Target>,
    publish_token: String,
    worker_token: String,
    max_upload: u64,
}
impl State {
    pub fn new(
        root: PathBuf,
        targets: Vec<Target>,
        publish_token: String,
        worker_token: String,
        max_upload: u64,
    ) -> Result<Self> {
        Ok(Self(Arc::new(Inner {
            store: Store::open(root)?,
            targets,
            publish_token,
            worker_token,
            max_upload,
        })))
    }
}

struct ApiError(StatusCode, String);
impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self(StatusCode::BAD_REQUEST, error.to_string())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}
type ApiResult = std::result::Result<Response, ApiError>;
fn files_index(parts: &[&str]) -> Option<usize> {
    [6, 10]
        .into_iter()
        .find(|&i| parts.get(i) == Some(&"files"))
}
fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "Not found".into())
}
fn unauthorized() -> ApiError {
    ApiError(StatusCode::UNAUTHORIZED, "Authentication required".into())
}
fn equal(a: &str, b: &str) -> bool {
    bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

#[derive(PartialEq)]
enum Role {
    Publisher,
    Worker,
}
fn role(state: &State, headers: &HeaderMap) -> std::result::Result<Role, ApiError> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .ok_or_else(unauthorized)?;
    if equal(token, &state.0.publish_token) {
        Ok(Role::Publisher)
    } else if equal(token, &state.0.worker_token) {
        Ok(Role::Worker)
    } else {
        Err(unauthorized())
    }
}
fn require_role(
    state: &State,
    headers: &HeaderMap,
    expected: Role,
) -> std::result::Result<(), ApiError> {
    if role(state, headers)? == expected {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::FORBIDDEN,
            "This token cannot perform that operation".into(),
        ))
    }
}

pub fn router(state: State) -> Router {
    Router::new()
        .route("/", get(|| async { Json(json!({"name":"conan-server","version":env!("CARGO_PKG_VERSION"),"protocol":"Conan 2","status":"prototype"})) }))
        .route("/health", get(|| async { Json(json!({"status":"ok"})) }))
        .route("/v1/ping", get(ping))
        .route("/v2/users/authenticate", get(authenticate))
        .route("/v2/users/check_credentials", get(credentials))
        .route("/v2/conans/{*path}", get(conan).put(conan))
        .route("/api/targets", get(targets))
        .route("/api/jobs", get(jobs))
        .route("/api/jobs/claim", post(claim))
        .route("/api/jobs/{id}/heartbeat", post(heartbeat))
        .route("/api/jobs/{id}/complete", post(complete))
        .route("/api/jobs/{id}/retry", post(retry))
        .layer(DefaultBodyLimit::max(65536))
        .with_state(state)
}

async fn ping() -> Response {
    (
        [
            ("x-conan-server-capabilities", "revisions"),
            ("x-conan-server-version", env!("CARGO_PKG_VERSION")),
        ],
        "",
    )
        .into_response()
}

async fn authenticate(AxumState(state): AxumState<State>, headers: HeaderMap) -> ApiResult {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Basic "))
        .ok_or_else(unauthorized)?;
    let bytes = STANDARD.decode(auth).map_err(|_| unauthorized())?;
    let basic = String::from_utf8(bytes).map_err(|_| unauthorized())?;
    let (user, password) = basic.split_once(':').ok_or_else(unauthorized)?;
    let expected = match user {
        "publisher" => &state.0.publish_token,
        "worker" => &state.0.worker_token,
        _ => return Err(unauthorized()),
    };
    if !equal(password, expected) {
        return Err(unauthorized());
    }
    Ok(expected.clone().into_response())
}
async fn credentials(AxumState(state): AxumState<State>, headers: HeaderMap) -> ApiResult {
    role(&state, &headers)?;
    Ok("ok".into_response())
}
async fn targets(AxumState(state): AxumState<State>) -> Json<Vec<Target>> {
    Json(state.0.targets.clone())
}
async fn jobs(AxumState(state): AxumState<State>, headers: HeaderMap) -> ApiResult {
    role(&state, &headers)?;
    Ok(Json(state.0.store.jobs()?).into_response())
}
async fn claim(
    AxumState(state): AxumState<State>,
    headers: HeaderMap,
    Json(claim): Json<Claim>,
) -> ApiResult {
    require_role(&state, &headers, Role::Worker)?;
    Ok(Json(state.0.store.claim(&claim)?).into_response())
}
async fn heartbeat(
    AxumState(state): AxumState<State>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(report): Json<Report>,
) -> ApiResult {
    require_role(&state, &headers, Role::Worker)?;
    if !state.0.store.heartbeat(id, &report.lease)? {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Lease is no longer valid".into(),
        ));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}
async fn complete(
    AxumState(state): AxumState<State>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(report): Json<Report>,
) -> ApiResult {
    require_role(&state, &headers, Role::Worker)?;
    if !state
        .0
        .store
        .complete(id, &report.lease, report.success, &report.message)?
    {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Lease is no longer valid".into(),
        ));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}
async fn retry(
    AxumState(state): AxumState<State>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> ApiResult {
    require_role(&state, &headers, Role::Publisher)?;
    if !state.0.store.retry(id)? {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Only failed jobs can be retried".into(),
        ));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn conan(
    AxumState(state): AxumState<State>,
    Path(path): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> ApiResult {
    let parts: Vec<_> = path.split('/').collect();
    if method == Method::PUT {
        let index = files_index(&parts).ok_or_else(not_found)?;
        if parts.len() != index + 2 {
            return Err(not_found());
        }
        let scope = Scope::parse(&parts[..index])?;
        let name = parts[index + 1];
        let who = role(&state, &headers)?;
        if scope.is_recipe() && who != Role::Publisher {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                "Only publishers may upload recipes".into(),
            ));
        }
        Store::validate_filename(&scope, name)?;
        if !scope.is_recipe() && state.0.store.snapshot(&parts[..6].join("/"))?.is_none() {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "Upload the complete recipe before its binaries".into(),
            ));
        }
        return upload(&state, &scope, name, headers, body).await;
    }
    if parts.as_slice() == ["search"] {
        let pattern = query.get("q").map(String::as_str).unwrap_or("*");
        let matcher = globset::GlobBuilder::new(pattern)
            .case_insensitive(query.get("ignorecase").is_none_or(|s| s != "False"))
            .build()
            .map_err(anyhow::Error::from)?
            .compile_matcher();
        let recipes: Vec<_> = state
            .0
            .store
            .recipes()?
            .into_iter()
            .filter(|r| matcher.is_match(r))
            .collect();
        return Ok(Json(json!({"results":recipes})).into_response());
    }
    if parts.len() < 5 || !parts[..4].iter().all(|s| safe_component(s)) {
        return Err(not_found());
    }
    let mut base = parts[..4].to_vec();
    base.extend(["revisions", "placeholder"]);
    let reference = Scope::parse(&base)?.recipe;
    if parts.len() == 5 && ["latest", "revisions"].contains(&parts[4]) {
        return revision_response(
            state.0.store.revisions(&reference, None, None)?,
            parts[4] == "latest",
        );
    }
    if parts.len() >= 6 && parts[4] == "revisions" {
        let scope = Scope::parse(&parts[..6])?;
        if parts.len() == 7 && parts[6] == "search" {
            if state.0.store.snapshot(&scope.key)?.is_none() {
                return Err(not_found());
            }
            return Ok(Json(state.0.store.packages(&reference, &scope.revision)?).into_response());
        }
        if parts.len() == 9 && parts[6] == "packages" && ["latest", "revisions"].contains(&parts[8])
        {
            return revision_response(
                state
                    .0
                    .store
                    .revisions(&reference, Some(&scope.revision), Some(parts[7]))?,
                parts[8] == "latest",
            );
        }
        if let Some(index) = files_index(&parts) {
            let scope = Scope::parse(&parts[..index])?;
            if parts.len() == index + 1 {
                return Ok(
                    Json(state.0.store.snapshot(&scope.key)?.ok_or_else(not_found)?)
                        .into_response(),
                );
            }
            if parts.len() == index + 2 {
                Store::validate_filename(&scope, parts[index + 1])?;
                let (path, size, hash) = state
                    .0
                    .store
                    .file(&scope.key, parts[index + 1])?
                    .ok_or_else(not_found)?;
                let file = tokio::fs::File::open(path)
                    .await
                    .map_err(anyhow::Error::from)?;
                return Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header(header::CONTENT_LENGTH, size)
                    .header(header::ETAG, format!("\"{hash}\""))
                    .header("x-checksum-sha256", hash)
                    .body(Body::from_stream(ReaderStream::new(file)))
                    .map_err(anyhow::Error::from)?);
            }
        }
    }
    Err(not_found())
}

fn revision_response(revisions: Vec<Value>, latest: bool) -> ApiResult {
    if revisions.is_empty() {
        return Err(not_found());
    }
    Ok(Json(if latest {
        revisions[0].clone()
    } else {
        json!({"revisions":revisions})
    })
    .into_response())
}

async fn upload(
    state: &State,
    scope: &Scope,
    name: &str,
    headers: HeaderMap,
    body: Body,
) -> ApiResult {
    let limit = if name.ends_with(".tgz") {
        state.0.max_upload
    } else {
        8 * 1024 * 1024
    };
    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .is_some_and(|n| n > limit)
    {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Artifact exceeds upload limit".into(),
        ));
    }
    let temp = state
        .0
        .store
        .root
        .join("staging")
        .join(uuid::Uuid::new_v4().to_string());
    let result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await
            .map_err(anyhow::Error::from)?;
        let mut stream = body.into_data_stream();
        let mut hasher = Sha256::new();
        let mut size = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(anyhow::Error::from)?;
            size += chunk.len() as u64;
            if size > limit {
                return Err(ApiError(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Artifact exceeds upload limit".into(),
                ));
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(anyhow::Error::from)?;
        }
        file.sync_all().await.map_err(anyhow::Error::from)?;
        drop(file);
        let hash = format!("{:x}", hasher.finalize());
        if let Some(expected) = headers
            .get("x-checksum-sha256")
            .and_then(|h| h.to_str().ok())
        {
            if !expected.eq_ignore_ascii_case(&hash) {
                return Err(ApiError(StatusCode::BAD_REQUEST, "SHA256 mismatch".into()));
            }
        }
        let destination = state.0.store.root.join("blobs").join(&hash);
        // hard_link is atomic and does not replace an existing blob on any platform.
        match tokio::fs::hard_link(&temp, &destination).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(anyhow::Error::from(e).into()),
        }
        state
            .0
            .store
            .commit_file(scope, name, &hash, size, &state.0.targets)
            .map_err(|e| ApiError(StatusCode::CONFLICT, e.to_string()))?;
        tracing::info!(reference=%scope.full_ref(), artifact=name, bytes=size, "artifact stored");
        Ok(StatusCode::CREATED.into_response())
    }
    .await;
    let _ = tokio::fs::remove_file(temp).await;
    result
}
