use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Multipart, Path as AxumPath, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use dbx_core::sql;
use dbx_core::sql::{SqlFileProgress, SqlFileRequest, SqlFileStatus};
use dbx_core::sql_file_import::{
    execute_sql_file_content, sql_file_error_progress, sql_file_progress as build_sql_file_progress,
    SqlFileProgressEmitter,
};
use futures::stream::Stream;
use serde::Deserialize;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio_util::sync::CancellationToken;

use crate::error::AppError;
use crate::state::WebState;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlFileExecuteWrapper {
    pub request: SqlFileRequest,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelSqlFileRequest {
    pub execution_id: String,
}

pub async fn preview_sql_file(
    State(state): State<Arc<WebState>>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, AppError> {
    let tmp_dir = state.data_dir.join("tmp");
    std::fs::create_dir_all(&tmp_dir).map_err(|e| AppError::from(e.to_string()))?;

    if let Some(field) = multipart.next_field().await.map_err(|e| AppError::from(e.to_string()))? {
        let file_name = field.file_name().unwrap_or("upload.sql").to_string();
        let data = field.bytes().await.map_err(|e| AppError::from(e.to_string()))?;

        // Generate a unique upload subdirectory so that two files with the
        // same basename (e.g. dirA/foo.sql and dirB/foo.sql) don't overwrite
        // each other in the shared tmp directory.
        let upload_id = uuid::Uuid::new_v4().to_string();
        let upload_dir = tmp_dir.join(&upload_id);
        std::fs::create_dir_all(&upload_dir).map_err(|e| AppError::from(e.to_string()))?;

        let file_path = safe_uploaded_sql_path(&upload_dir, &file_name)?;
        std::fs::write(&file_path, &data).map_err(|e| AppError::from(e.to_string()))?;

        // Schedule cleanup of the upload directory after 5 minutes so that
        // previewed-but-never-claimed files don't accumulate indefinitely.
        // When execution starts, the TTL task is aborted (claim) so the file
        // isn't deleted while queued behind other files.
        let file_path_key = file_path.to_string_lossy().to_string();
        let ttl_dir = upload_dir.clone();
        let ttl_state = state.clone();
        let ttl_key = file_path_key.clone();
        let ttl_handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(300)).await;
            let should_delete = ttl_state.sql_file_upload_ttls.write().await.remove(&ttl_key).is_some();
            if should_delete {
                let _ = std::fs::remove_dir_all(&ttl_dir);
            }
        });
        state.sql_file_upload_ttls.write().await.insert(file_path_key, ttl_handle);

        let size_bytes = data.len() as u64;
        let content = sql::decode_sql_file_bytes(&data).map_err(AppError::from)?;
        let preview: String = content.chars().take(20_000).collect();

        return Ok(Json(serde_json::json!({
            "fileName": file_name,
            "filePath": file_path.to_string_lossy(),
            "sizeBytes": size_bytes,
            "preview": preview,
            "canExecuteWithoutSelectedDatabase": dbx_core::sql_file_import::mysql_like_sql_file_can_execute_without_selected_database(&content),
        })));
    }

    Err(AppError::from("No file uploaded".to_string()))
}

pub async fn execute_sql_file(
    State(state): State<Arc<WebState>>,
    Json(body): Json<SqlFileExecuteWrapper>,
) -> Result<Json<serde_json::Value>, AppError> {
    let req = body.request;

    // Fast-fail: reject early if the connection is read-only (individual statements are also checked in do_execute)
    if let Some(name) = dbx_core::query::connection_readonly_name(&state.app, &req.connection_id).await {
        return Err(AppError::from(format!(
            "Read-only mode: connection '{}' has read-only protection enabled. SQL file execution blocked.",
            name
        )));
    }

    let execution_id = req.execution_id.clone();
    let file_path = validated_uploaded_sql_path(&state.data_dir, &req.file_path)?;
    // Claim the uploaded file: abort the preview TTL so the file isn't
    // deleted while it's queued behind other files in sequential mode.
    if let Some(handle) = state.sql_file_upload_ttls.write().await.remove(&req.file_path) {
        handle.abort();
    }
    let token = CancellationToken::new();

    {
        let mut executions = state.sql_file_executions.write().await;
        if executions.contains_key(&execution_id) {
            return Err(AppError::from(format!("SQL file execution '{execution_id}' already exists")));
        }
        executions.insert(execution_id.clone(), token.clone());
    }
    let (tx, _) = tokio::sync::broadcast::channel::<String>(256);
    state.sse_channels.write().await.insert(execution_id.clone(), tx.clone());

    let app = state.app.clone();
    let state_clone = state.clone();

    tokio::spawn(async move {
        let started_at = Instant::now();
        let state_for_emit = state_clone.clone();
        let mut progress_emitter = SqlFileProgressEmitter::new(move |progress: SqlFileProgress| {
            publish_sql_file_progress(&state_for_emit, &tx, progress);
        });
        progress_emitter.emit(build_sql_file_progress(
            &req.execution_id,
            SqlFileStatus::Started,
            0,
            0,
            0,
            0,
            started_at,
            "",
            None,
        ));
        match std::fs::metadata(&file_path) {
            Ok(meta) if meta.len() > 200 * 1024 * 1024 => {
                progress_emitter.emit(sql_file_error_progress(
                    &req.execution_id,
                    started_at,
                    format!("File too large: {} bytes (max {} bytes)", meta.len(), 200 * 1024 * 1024),
                ));
                finalize_execution(&state_clone, &req.execution_id, &file_path).await;
                return;
            }
            Err(e) => {
                progress_emitter.emit(sql_file_error_progress(&req.execution_id, started_at, e.to_string()));
                finalize_execution(&state_clone, &req.execution_id, &file_path).await;
                return;
            }
            _ => {}
        }

        let file_content = match std::fs::read(&file_path).and_then(|bytes| {
            sql::decode_sql_file_bytes(&bytes)
                .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidData, message))
        }) {
            Ok(content) => content,
            Err(e) => {
                progress_emitter.emit(sql_file_error_progress(&req.execution_id, started_at, e.to_string()));
                finalize_execution(&state_clone, &req.execution_id, &file_path).await;
                return;
            }
        };

        let result = execute_sql_file_content(&app, &req, &file_content, token, started_at, |progress| {
            progress_emitter.emit(progress);
        })
        .await;

        // If the executor returned an error (e.g. connection or prepare-stage
        // failure) without emitting a terminal progress, convert it to an
        // Error progress so late SSE subscribers receive a terminal status
        // instead of waiting until the 10-minute timeout. The emit callback
        // persists terminal progress before broadcasting it, so we just need
        // to check the store.
        if let Err(e) = result {
            let has_terminal = state_clone.sql_file_terminal_progress.read().unwrap().contains_key(&req.execution_id);
            if !has_terminal {
                progress_emitter.emit(sql_file_error_progress(&req.execution_id, started_at, e));
            }
        }

        finalize_execution(&state_clone, &req.execution_id, &file_path).await;
    });

    Ok(Json(serde_json::json!({ "executionId": execution_id })))
}

fn publish_sql_file_progress(state: &WebState, tx: &broadcast::Sender<String>, progress: SqlFileProgress) {
    if matches!(progress.status, SqlFileStatus::Done | SqlFileStatus::Error | SqlFileStatus::Cancelled) {
        state
            .sql_file_terminal_progress
            .write()
            .unwrap()
            .insert(progress.execution_id.clone(), (progress.clone(), Instant::now()));
    }
    if let Ok(json) = serde_json::to_string(&progress) {
        let _ = tx.send(json);
    }
}

/// Finalize a SQL file execution: delete the uploaded temp file, remove the
/// active execution tracking and broadcast channel, and schedule eviction of
/// the terminal progress entry after a 5-minute TTL. The terminal progress
/// itself is already persisted by the emit callback before the broadcast, so
/// this function only handles cleanup.
async fn finalize_execution(state: &Arc<WebState>, execution_id: &str, file_path: &Path) {
    // Delete the uploaded temp file and its (now empty) parent upload dir.
    let _ = std::fs::remove_file(file_path);
    if let Some(parent) = file_path.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    // Remove active execution tracking and broadcast channel.
    cleanup_sql_file_execution(state, execution_id).await;
    // Schedule eviction of the terminal progress entry after the TTL.
    let state_for_eviction = state.clone();
    let id_for_eviction = execution_id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(300)).await;
        state_for_eviction.sql_file_terminal_progress.write().unwrap().remove(&id_for_eviction);
    });
}

async fn cleanup_sql_file_execution(state: &WebState, execution_id: &str) {
    state.remove_sse_channel(execution_id).await;
    state.sql_file_executions.write().await.remove(execution_id);
}

/// TTL for terminal progress entries — how long a late subscriber can
/// retrieve the final status after the execution has completed.
const TERMINAL_PROGRESS_TTL: Duration = Duration::from_secs(300);

/// How long the GET handler waits for a channel to appear (POST still in
/// flight) before giving up.
const CHANNEL_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn sql_file_progress(
    State(state): State<Arc<WebState>>,
    AxumPath(execution_id): AxumPath<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>, AppError> {
    let deadline = Instant::now() + CHANNEL_WAIT_TIMEOUT;
    loop {
        if let Some(rx) = sql_file_progress_receiver(&state, &execution_id).await {
            return Ok(sql_file_progress_sse(state.clone(), execution_id.clone(), rx));
        }
        if Instant::now() >= deadline {
            return Err(AppError::from("Execution not found".to_string()));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn sql_file_progress_receiver(state: &WebState, execution_id: &str) -> Option<broadcast::Receiver<String>> {
    let active = {
        let channels = state.sse_channels.read().await;
        channels.get(execution_id).map(broadcast::Sender::subscribe)
    };

    if let Some(terminal) = terminal_progress_json(state, execution_id) {
        return Some(single_event_receiver(terminal));
    }

    active
}

fn terminal_progress_json(state: &WebState, execution_id: &str) -> Option<String> {
    let mut terminals = state.sql_file_terminal_progress.write().unwrap();
    terminals.retain(|_, (_, timestamp)| timestamp.elapsed() <= TERMINAL_PROGRESS_TTL);
    terminals.get(execution_id).and_then(|(progress, _)| serde_json::to_string(progress).ok())
}

fn single_event_receiver(data: String) -> broadcast::Receiver<String> {
    let (tx, rx) = broadcast::channel(1);
    let _ = tx.send(data);
    drop(tx);
    rx
}

fn sql_file_progress_sse(
    state: Arc<WebState>,
    execution_id: String,
    mut rx: broadcast::Receiver<String>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let stream = async_stream::stream! {
        let mut last_data: Option<String> = None;
        loop {
            match rx.recv().await {
                Ok(data) => {
                    last_data = Some(data.clone());
                    yield Ok(Event::default().data(data));
                }
                Err(RecvError::Lagged(_)) => {
                    if let Some(terminal) = terminal_progress_json(&state, &execution_id) {
                        if last_data.as_deref() != Some(terminal.as_str()) {
                            yield Ok(Event::default().data(terminal));
                        }
                        break;
                    }
                }
                Err(RecvError::Closed) => {
                    if let Some(terminal) = terminal_progress_json(&state, &execution_id) {
                        if last_data.as_deref() != Some(terminal.as_str()) {
                            yield Ok(Event::default().data(terminal));
                        }
                    }
                    break;
                }
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub async fn cancel_sql_file(
    State(state): State<Arc<WebState>>,
    Json(req): Json<CancelSqlFileRequest>,
) -> Json<serde_json::Value> {
    let executions = state.sql_file_executions.read().await;
    if let Some(token) = executions.get(&req.execution_id) {
        token.cancel();
        Json(serde_json::json!({ "cancelled": true }))
    } else {
        Json(serde_json::json!({ "cancelled": false }))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseUploadRequest {
    pub file_path: String,
}

/// Release an uploaded SQL file that was previewed but never executed (e.g.
/// the user removed it from the batch list or closed the dialog). Deletes the
/// temp file and its unique upload subdirectory so the tmp tree doesn't grow
/// without bound. Idempotent: deleting a missing file is a no-op, so calling
/// this after `finalize_execution` already cleaned up is harmless.
pub async fn release_sql_file_upload(
    State(state): State<Arc<WebState>>,
    Json(req): Json<ReleaseUploadRequest>,
) -> Json<serde_json::Value> {
    // Abort the preview TTL so it doesn't race with our explicit deletion.
    if let Some(handle) = state.sql_file_upload_ttls.write().await.remove(&req.file_path) {
        handle.abort();
    }
    if let Ok(path) = validated_uploaded_sql_path(&state.data_dir, &req.file_path) {
        let _ = std::fs::remove_file(&path);
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
    Json(serde_json::json!({ "released": true }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimUploadsRequest {
    pub file_paths: Vec<String>,
}

/// Claim all uploaded files in a batch at once, aborting their preview TTL
/// tasks. This must be called before the batch starts executing so that a
/// long-running first file doesn't cause subsequent files' TTLs to fire and
/// delete them before they are reached in sequential mode.
pub async fn claim_sql_file_uploads(
    State(state): State<Arc<WebState>>,
    Json(req): Json<ClaimUploadsRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut ttls = state.sql_file_upload_ttls.write().await;
    let unavailable: Vec<String> = req
        .file_paths
        .iter()
        .filter(|file_path| {
            !ttls.contains_key(file_path.as_str()) || validated_uploaded_sql_path(&state.data_dir, file_path).is_err()
        })
        .cloned()
        .collect();
    if !unavailable.is_empty() {
        return Err(AppError::bad_request(format!(
            "Unable to claim all SQL file uploads; unavailable paths: {}",
            serde_json::to_string(&unavailable).unwrap_or_default()
        )));
    }

    for file_path in &req.file_paths {
        if let Some(handle) = ttls.remove(file_path) {
            handle.abort();
        }
    }
    drop(ttls);
    Ok(Json(serde_json::json!({ "claimed": req.file_paths.len(), "unavailable": [] })))
}

/// Build a safe destination path for an uploaded file inside `upload_dir`,
/// using only the file's basename so that a crafted `../` name cannot escape
/// the unique upload subdirectory.
fn safe_uploaded_sql_path(upload_dir: &Path, file_name: &str) -> Result<PathBuf, AppError> {
    let basename = Path::new(file_name)
        .file_name()
        .map(|s| s.to_os_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("upload.sql"));
    Ok(upload_dir.join(basename))
}

/// Validate that a file path supplied to the execute endpoint points at a real
/// file inside the `data_dir/tmp` upload tree, preventing execution of
/// arbitrary files outside the upload area. Returns the canonicalised path.
fn validated_uploaded_sql_path(data_dir: &Path, file_path: &str) -> Result<PathBuf, AppError> {
    let tmp_root = data_dir.join("tmp");
    let path = PathBuf::from(file_path);
    let canonical_path = path.canonicalize().map_err(|e| AppError::from(e.to_string()))?;
    let canonical_tmp = tmp_root.canonicalize().map_err(|e| AppError::from(e.to_string()))?;
    if !canonical_path.starts_with(&canonical_tmp) {
        return Err(AppError::from("SQL file path is outside the upload directory".to_string()));
    }
    Ok(canonical_path)
}

#[cfg(test)]
mod tests {
    use super::{publish_sql_file_progress, safe_uploaded_sql_path, sql_file_progress, validated_uploaded_sql_path};
    use crate::state::{LoginRateLimit, WebState};
    use axum::body::to_bytes;
    use axum::extract::{Path as AxumPath, State};
    use axum::response::IntoResponse;
    use dbx_core::connection::AppState;
    use dbx_core::sql::{SqlFileProgress, SqlFileStatus};
    use dbx_core::storage::Storage;
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::{broadcast, Mutex, RwLock};

    async fn test_web_state() -> (Arc<WebState>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("dbx-web-sql-file-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let storage = Storage::open(&dir.join("storage.db")).await.unwrap();
        let app = Arc::new(AppState::new_with_plugin_dir(storage, dir.join("plugins")));
        let state = Arc::new(WebState {
            app,
            data_dir: dir.clone(),
            public_base_path: "/".to_string(),
            password_disabled: false,
            password_hash: RwLock::new(None),
            sessions: RwLock::new(HashSet::new()),
            sse_channels: RwLock::new(HashMap::new()),
            table_import_channels: RwLock::new(HashMap::new()),
            sql_file_executions: RwLock::new(HashMap::new()),
            login_rate_limit: Mutex::new(LoginRateLimit { fail_count: 0, locked_until: None }),
            export_files: RwLock::new(HashMap::new()),
            sql_file_terminal_progress: std::sync::RwLock::new(HashMap::new()),
            sql_file_upload_ttls: RwLock::new(HashMap::new()),
        });
        (state, dir)
    }

    fn make_terminal_progress(execution_id: &str) -> SqlFileProgress {
        SqlFileProgress {
            execution_id: execution_id.to_string(),
            status: SqlFileStatus::Done,
            statement_index: 1,
            success_count: 1,
            failure_count: 0,
            affected_rows: 0,
            elapsed_ms: 10,
            statement_summary: "SELECT 1".to_string(),
            error: None,
        }
    }

    fn create_uploaded_sql_file(data_dir: &std::path::Path, name: &str) -> String {
        let upload_dir = data_dir.join("tmp").join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&upload_dir).unwrap();
        let path = upload_dir.join(name);
        std::fs::write(&path, "select 1;").unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn uploaded_sql_path_uses_only_the_file_name() {
        let data_dir = std::env::temp_dir().join(format!("dbx-web-sql-file-test-{}", uuid::Uuid::new_v4()));
        let tmp_dir = data_dir.join("tmp");

        let path = match safe_uploaded_sql_path(&tmp_dir, "../outside.sql") {
            Ok(path) => path,
            Err(error) => panic!("{}", error.message),
        };

        assert_eq!(path, tmp_dir.join("outside.sql"));
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn execution_path_must_stay_inside_uploaded_tmp_dir() {
        let data_dir = std::env::temp_dir().join(format!("dbx-web-sql-file-test-{}", uuid::Uuid::new_v4()));
        let tmp_dir = data_dir.join("tmp");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let outside = data_dir.join("outside.sql");
        std::fs::write(&outside, "select 1;").unwrap();

        let result = validated_uploaded_sql_path(&data_dir, &outside.to_string_lossy());

        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(data_dir);
    }

    /// GET arrives after POST: the broadcast channel already exists. The SSE
    /// stream should subscribe and deliver the buffered progress event.
    #[tokio::test]
    async fn get_after_post_uses_active_channel() {
        let (state, dir) = test_web_state().await;
        let execution_id = "exec-channel-exists";

        // Simulate the POST handler registering a broadcast channel.
        let (tx, _rx) = broadcast::channel::<String>(256);
        state.sse_channels.write().await.insert(execution_id.to_string(), tx.clone());

        let result = sql_file_progress(State(state.clone()), AxumPath(execution_id.to_string())).await;
        assert!(result.is_ok(), "expected Ok when channel exists");
        let sse = result.unwrap_or_else(|e| panic!("expected Ok: {}", e.message));
        let response = sse.into_response();
        let body = response.into_body();

        // The receiver was created (and subscribed) inside sql_file_progress.
        // Sending now buffers the message in the receiver's queue; the stream
        // picks it up when to_bytes polls it.
        let progress = make_terminal_progress(execution_id);
        let json = serde_json::to_string(&progress).unwrap();
        let _ = tx.send(json.clone());

        // Drop all senders so the broadcast channel closes and the SSE stream
        // ends after delivering the buffered message.
        state.sse_channels.write().await.remove(execution_id);
        drop(tx);

        let bytes = tokio::time::timeout(Duration::from_secs(5), to_bytes(body, 1024 * 1024))
            .await
            .expect("to_bytes should not time out")
            .expect("to_bytes should not error");
        let body_str = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body_str.contains(&json), "SSE body should contain the progress event, got: {body_str}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// GET arrives after the task has finished: the channel was cleaned up but
    /// the terminal progress was saved. The SSE stream should deliver the
    /// terminal progress as a single event.
    #[tokio::test]
    async fn get_after_completion_uses_terminal_progress() {
        let (state, dir) = test_web_state().await;
        let execution_id = "exec-terminal";

        let progress = make_terminal_progress(execution_id);
        let expected_json = serde_json::to_string(&progress).unwrap();
        state.sql_file_terminal_progress.write().unwrap().insert(execution_id.to_string(), (progress, Instant::now()));

        let result = sql_file_progress(State(state.clone()), AxumPath(execution_id.to_string())).await;
        assert!(result.is_ok(), "expected Ok when terminal progress exists");
        let sse = result.unwrap_or_else(|e| panic!("expected Ok: {}", e.message));
        let response = sse.into_response();
        let body = response.into_body();

        // The terminal-progress stream uses a closed broadcast channel with a
        // single buffered message, so it ends naturally after delivering it.
        let bytes = tokio::time::timeout(Duration::from_secs(5), to_bytes(body, 1024 * 1024))
            .await
            .expect("to_bytes should not time out")
            .expect("to_bytes should not error");
        let body_str = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body_str.contains(&expected_json), "SSE body should contain the terminal progress, got: {body_str}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// GET arrives before POST: neither channel nor terminal progress exists.
    /// The handler should wait (polling every 100ms) until the channel
    /// appears, then subscribe. This covers the network-reordering race where
    /// the EventSource connects before the POST request arrives.
    #[tokio::test]
    async fn get_before_post_waits_for_channel() {
        let (state, dir) = test_web_state().await;
        let execution_id = "exec-wait";

        // Spawn a task that inserts the channel after a short delay,
        // simulating the POST request arriving after the GET.
        let state_clone = state.clone();
        let id_clone = execution_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let (tx, _rx) = broadcast::channel::<String>(256);
            state_clone.sse_channels.write().await.insert(id_clone, tx);
        });

        // The GET handler should wait and eventually return Ok when the channel
        // appears. Use a timeout shorter than CHANNEL_WAIT_TIMEOUT (30s) to keep
        // the test fast.
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            sql_file_progress(State(state.clone()), AxumPath(execution_id.to_string())),
        )
        .await;

        assert!(result.is_ok(), "sql_file_progress should not time out waiting for channel");
        assert!(result.unwrap().is_ok(), "expected Ok when channel appears within wait window");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn get_before_post_replays_fast_terminal_completion() {
        let (state, dir) = test_web_state().await;
        let execution_id = "exec-fast-terminal";

        let state_clone = state.clone();
        let id_clone = execution_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let (tx, _rx) = broadcast::channel::<String>(256);
            state_clone.sse_channels.write().await.insert(id_clone.clone(), tx.clone());
            publish_sql_file_progress(&state_clone, &tx, make_terminal_progress(&id_clone));
            state_clone.remove_sse_channel(&id_clone).await;
            drop(tx);
        });

        let sse = tokio::time::timeout(
            Duration::from_secs(2),
            sql_file_progress(State(state.clone()), AxumPath(execution_id.to_string())),
        )
        .await
        .expect("progress lookup should replay terminal instead of waiting for timeout")
        .unwrap_or_else(|error| panic!("expected terminal SSE: {}", error.message));
        let bytes =
            tokio::time::timeout(Duration::from_secs(2), to_bytes(sse.into_response().into_body(), 1024 * 1024))
                .await
                .expect("terminal SSE body should complete")
                .expect("terminal SSE body should be readable");
        assert!(String::from_utf8(bytes.to_vec()).unwrap().contains("\"status\":\"done\""));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn active_channel_close_rechecks_terminal_store() {
        let (state, dir) = test_web_state().await;
        let execution_id = "exec-closed-channel-terminal";
        let (tx, _rx) = broadcast::channel::<String>(256);
        state.sse_channels.write().await.insert(execution_id.to_string(), tx.clone());

        let sse = sql_file_progress(State(state.clone()), AxumPath(execution_id.to_string()))
            .await
            .unwrap_or_else(|error| panic!("expected active SSE: {}", error.message));

        state
            .sql_file_terminal_progress
            .write()
            .unwrap()
            .insert(execution_id.to_string(), (make_terminal_progress(execution_id), Instant::now()));
        state.remove_sse_channel(execution_id).await;
        drop(tx);

        let bytes =
            tokio::time::timeout(Duration::from_secs(2), to_bytes(sse.into_response().into_body(), 1024 * 1024))
                .await
                .expect("closed channel should replay terminal")
                .expect("terminal SSE body should be readable");
        assert!(String::from_utf8(bytes.to_vec()).unwrap().contains("\"status\":\"done\""));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// GET arrives with no POST and no terminal progress: the handler should
    /// NOT return immediately with an error — it should be waiting. We verify
    /// this by racing against a short timeout and expecting the handler to
    /// still be running (i.e., the timeout elapses first).
    #[tokio::test]
    async fn get_without_post_does_not_fail_immediately() {
        let (state, dir) = test_web_state().await;
        let execution_id = "exec-nothing";

        let result = tokio::time::timeout(
            Duration::from_millis(500),
            sql_file_progress(State(state.clone()), AxumPath(execution_id.to_string())),
        )
        .await;

        assert!(result.is_err(), "sql_file_progress should be waiting for channel, not returning immediately");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Late subscriber race: the task sent the terminal progress via the
    /// broadcast channel AND saved it to the terminal store, but the channel
    /// hasn't been removed yet. A new receiver created by `subscribe()` would
    /// miss the already-sent message. The GET handler must recheck the
    /// terminal store after subscribing and return the saved terminal.
    #[tokio::test]
    async fn get_after_terminal_sent_but_channel_still_active_uses_terminal_store() {
        let (state, dir) = test_web_state().await;
        let execution_id = "exec-late-subscriber";

        // Simulate: task is still "active" (channel exists) but has already
        // sent the terminal progress and saved it to the terminal store.
        let (tx, _rx) = broadcast::channel::<String>(256);
        state.sse_channels.write().await.insert(execution_id.to_string(), tx);

        let progress = make_terminal_progress(execution_id);
        let expected_json = serde_json::to_string(&progress).unwrap();
        state.sql_file_terminal_progress.write().unwrap().insert(execution_id.to_string(), (progress, Instant::now()));

        // The GET handler should find the channel, subscribe, then recheck
        // the terminal store and return the terminal progress (not the
        // channel stream which would miss the already-sent message).
        let result = sql_file_progress(State(state.clone()), AxumPath(execution_id.to_string())).await;
        assert!(result.is_ok(), "expected Ok when terminal store has entry");
        let sse = result.unwrap_or_else(|e| panic!("expected Ok: {}", e.message));
        let response = sse.into_response();
        let body = response.into_body();

        let bytes = tokio::time::timeout(Duration::from_secs(5), to_bytes(body, 1024 * 1024))
            .await
            .expect("to_bytes should not time out")
            .expect("to_bytes should not error");
        let body_str = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body_str.contains(&expected_json),
            "SSE body should contain the terminal progress from the store, got: {body_str}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Verify that the TTL claim mechanism works: when a file path is claimed
    /// (removed from the TTL map), the preview TTL handle is aborted and no
    /// longer running.
    #[tokio::test]
    async fn preview_ttl_is_aborted_when_execute_claims_file() {
        let (state, dir) = test_web_state().await;
        let file_path_key = "/tmp/fake-path/test.sql";

        // Simulate preview storing a TTL handle.
        let ttl_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ttl_ran_clone = ttl_ran.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            ttl_ran_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        state.sql_file_upload_ttls.write().await.insert(file_path_key.to_string(), handle);

        // Claim the file (as execute_sql_file would).
        if let Some(h) = state.sql_file_upload_ttls.write().await.remove(file_path_key) {
            h.abort();
        }

        // Wait long enough for the TTL to have fired if it weren't aborted.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!ttl_ran.load(std::sync::atomic::Ordering::SeqCst), "TTL task should have been aborted, not run");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: the terminal progress is persisted before it is broadcast
    /// via the channel. This test verifies that after the terminal is emitted
    /// (store has it) but
    /// before the channel is cleaned up (channel still active), a late
    /// subscriber that subscribes AFTER the broadcast will still find the
    /// terminal in the store — not miss it due to broadcast's "only post-
    /// subscribe values" semantics.
    ///
    /// This is the exact race the user identified: "terminal 已 emit、store
    /// 尚未 finalize". The fix is that the emit callback writes to the store
    /// synchronously, so there is no window where the terminal is broadcast
    /// but missing from the store.
    #[tokio::test]
    async fn terminal_emitted_but_channel_not_finalized_late_subscriber_gets_terminal() {
        let (state, dir) = test_web_state().await;
        let execution_id = "exec-terminal-emitted-not-finalized";

        // Simulate the real emit callback.
        let (tx, _old_rx) = broadcast::channel::<String>(256);
        let progress = make_terminal_progress(execution_id);
        let json = serde_json::to_string(&progress).unwrap();

        // Channel is still active (finalize hasn't run yet).
        state.sse_channels.write().await.insert(execution_id.to_string(), tx.clone());

        publish_sql_file_progress(&state, &tx, progress);

        // Now a late subscriber arrives. It subscribes AFTER the terminal was
        // broadcast, so the new receiver won't get it from broadcast. But the
        // store has it (written before the broadcast), so the
        // recheck in sql_file_progress should find and return it.
        let result = sql_file_progress(State(state.clone()), AxumPath(execution_id.to_string())).await;
        assert!(result.is_ok(), "late subscriber should get terminal from store");
        let sse = result.unwrap_or_else(|e| panic!("expected Ok: {}", e.message));
        let response = sse.into_response();
        let body = response.into_body();

        let bytes = tokio::time::timeout(Duration::from_secs(5), to_bytes(body, 1024 * 1024))
            .await
            .expect("to_bytes should not time out")
            .expect("to_bytes should not error");
        let body_str = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body_str.contains(&json),
            "SSE body should contain the terminal progress from the store (not missed), got: {body_str}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Batch claim: all file paths in a batch should have their TTL handles
    /// aborted at once, so a long-running first file doesn't let subsequent
    /// files' TTLs fire and delete them.
    #[tokio::test]
    async fn batch_claim_aborts_all_ttls() {
        use super::claim_sql_file_uploads;
        use axum::Json;
        use serde_json::json;

        let (state, dir) = test_web_state().await;
        let paths = vec![
            create_uploaded_sql_file(&dir, "a.sql"),
            create_uploaded_sql_file(&dir, "b.sql"),
            create_uploaded_sql_file(&dir, "c.sql"),
        ];

        // Insert TTL handles for all three files.
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for p in &paths {
            let ran_clone = ran.clone();
            let handle = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                ran_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            });
            state.sql_file_upload_ttls.write().await.insert(p.clone(), handle);
        }

        // Claim all at once.
        let req = super::ClaimUploadsRequest { file_paths: paths.clone() };
        let response = claim_sql_file_uploads(State(state.clone()), Json(req))
            .await
            .unwrap_or_else(|error| panic!("expected successful claim: {}", error.message));
        let v = response.0;
        assert_eq!(v["claimed"], json!(3), "should have claimed all 3 files");

        // Wait long enough for TTLs to have fired if they weren't aborted.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 0, "all TTL tasks should have been aborted");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn partial_batch_claim_fails_without_claiming_any_file() {
        use super::claim_sql_file_uploads;
        use axum::Json;

        let (state, dir) = test_web_state().await;
        let available = create_uploaded_sql_file(&dir, "available.sql");
        let unavailable = dir.join("tmp").join("missing").join("missing.sql").to_string_lossy().to_string();
        let handle = tokio::spawn(async { tokio::time::sleep(Duration::from_secs(60)).await });
        state.sql_file_upload_ttls.write().await.insert(available.clone(), handle);

        let request = super::ClaimUploadsRequest { file_paths: vec![available.clone(), unavailable.clone()] };
        let error =
            claim_sql_file_uploads(State(state.clone()), Json(request)).await.expect_err("partial claim should fail");

        assert_eq!(error.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(error.message.contains(&unavailable));
        let mut ttls = state.sql_file_upload_ttls.write().await;
        assert!(ttls.contains_key(&available), "available upload must remain unclaimed after an atomic failure");
        if let Some(handle) = ttls.remove(&available) {
            handle.abort();
        }
        drop(ttls);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
