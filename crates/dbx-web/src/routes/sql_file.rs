use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Multipart, Path as AxumPath, State};
use axum::response::sse::{Event, Sse};
use axum::Json;
use dbx_core::sql;
use dbx_core::sql::{SqlFileProgress, SqlFileRequest, SqlFileStatus};
use dbx_core::sql_file_import::{
    execute_sql_file_content, sql_file_error_progress, sql_file_progress as build_sql_file_progress,
    SqlFileProgressEmitter,
};
use futures::stream::Stream;
use serde::Deserialize;
use tokio::sync::broadcast;
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
        // previewed-but-never-executed files don't accumulate indefinitely.
        // If the file is executed before then, the execution task also
        // deletes it — double deletion is a harmless no-op.
        let ttl_dir = upload_dir.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(300)).await;
            let _ = std::fs::remove_dir_all(&ttl_dir);
        });

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
        // Capture the terminal progress so it can be saved for late SSE
        // subscribers (GET arrives after the task finished and the broadcast
        // channel was cleaned up).
        let terminal_capture = std::sync::Mutex::new(None::<SqlFileProgress>);
        let mut progress_emitter = SqlFileProgressEmitter::new(|progress: SqlFileProgress| {
            send_sql_file_progress(&tx, progress.clone());
            if matches!(progress.status, SqlFileStatus::Done | SqlFileStatus::Error | SqlFileStatus::Cancelled) {
                *terminal_capture.lock().unwrap() = Some(progress);
            }
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
                finalize_execution(&state_clone, &req.execution_id, &file_path, &terminal_capture).await;
                return;
            }
            Err(e) => {
                progress_emitter.emit(sql_file_error_progress(&req.execution_id, started_at, e.to_string()));
                finalize_execution(&state_clone, &req.execution_id, &file_path, &terminal_capture).await;
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
                finalize_execution(&state_clone, &req.execution_id, &file_path, &terminal_capture).await;
                return;
            }
        };

        let _ = execute_sql_file_content(&app, &req, &file_content, token, started_at, |progress| {
            progress_emitter.emit(progress);
        })
        .await;

        finalize_execution(&state_clone, &req.execution_id, &file_path, &terminal_capture).await;
    });

    Ok(Json(serde_json::json!({ "executionId": execution_id })))
}

fn send_sql_file_progress(tx: &broadcast::Sender<String>, progress: SqlFileProgress) {
    if let Ok(json) = serde_json::to_string(&progress) {
        let _ = tx.send(json);
    }
}

/// Finalize a SQL file execution: save the terminal progress for late SSE
/// subscribers, delete the uploaded temp file, remove the active execution
/// tracking, and schedule eviction of the terminal progress entry after a
/// 5-minute TTL.
async fn finalize_execution(
    state: &Arc<WebState>,
    execution_id: &str,
    file_path: &Path,
    terminal_capture: &std::sync::Mutex<Option<SqlFileProgress>>,
) {
    // Save terminal progress for late subscribers. Extract the value before
    // awaiting so the std::sync::MutexGuard (which is not Send) is dropped
    // before the .await point — tokio::spawn requires the future to be Send.
    let terminal = terminal_capture.lock().unwrap().take();
    if let Some(tp) = terminal {
        state.sql_file_terminal_progress.write().await.insert(execution_id.to_string(), (tp, Instant::now()));
    }
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
        state_for_eviction.sql_file_terminal_progress.write().await.remove(&id_for_eviction);
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
    // 1. Try to get an active broadcast channel — the normal case where the
    //    POST has already arrived and the task is still running.
    {
        let channels = state.sse_channels.read().await;
        if let Some(tx) = channels.get(&execution_id) {
            let rx = tx.subscribe();
            drop(channels);
            return Ok(crate::sse::sse_from_lossy_channel(rx));
        }
    }

    // 2. No active channel — the task may have already finished. Check the
    //    terminal progress store and evict stale entries while we're at it.
    {
        let mut terminals = state.sql_file_terminal_progress.write().await;
        let mut stale = Vec::new();
        for (id, (_, ts)) in terminals.iter() {
            if ts.elapsed() > TERMINAL_PROGRESS_TTL {
                stale.push(id.clone());
            }
        }
        for id in &stale {
            terminals.remove(id);
        }
        if let Some((progress, _)) = terminals.get(&execution_id) {
            let json = serde_json::to_string(progress).unwrap_or_default();
            drop(terminals);
            // Return a single-event SSE stream with the terminal progress.
            // Use a broadcast channel (immediately closed after sending) so
            // the return type matches the other branches that use
            // sse_from_lossy_channel.
            let (tx, rx) = broadcast::channel::<String>(1);
            let _ = tx.send(json);
            drop(tx);
            return Ok(crate::sse::sse_from_lossy_channel(rx));
        }
    }

    // 3. Neither active channel nor terminal progress — the POST request may
    //    still be in flight (network reordering, EventSource connected
    //    before the POST arrived). Wait briefly for the channel to appear.
    let deadline = Instant::now() + CHANNEL_WAIT_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            return Err(AppError::from("Execution not found".to_string()));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let channels = state.sse_channels.read().await;
        if let Some(tx) = channels.get(&execution_id) {
            let rx = tx.subscribe();
            drop(channels);
            return Ok(crate::sse::sse_from_lossy_channel(rx));
        }
    }
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
    if let Ok(path) = validated_uploaded_sql_path(&state.data_dir, &req.file_path) {
        let _ = std::fs::remove_file(&path);
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
    Json(serde_json::json!({ "released": true }))
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
    use super::{safe_uploaded_sql_path, sql_file_progress, validated_uploaded_sql_path};
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
            sql_file_terminal_progress: RwLock::new(HashMap::new()),
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
        state.sql_file_terminal_progress.write().await.insert(execution_id.to_string(), (progress, Instant::now()));

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
}
