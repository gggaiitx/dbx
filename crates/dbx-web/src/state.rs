use dbx_core::connection::AppState;
use dbx_core::sql::SqlFileProgress;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, watch, Mutex, RwLock};
use tokio_util::sync::CancellationToken;

pub struct LoginRateLimit {
    pub fail_count: u32,
    pub locked_until: Option<std::time::Instant>,
}

pub struct WebState {
    pub app: Arc<AppState>,
    pub data_dir: PathBuf,
    pub public_base_path: String,
    pub password_disabled: bool,
    pub password_hash: RwLock<Option<String>>,
    pub sessions: RwLock<HashSet<String>>,
    pub sse_channels: RwLock<HashMap<String, broadcast::Sender<String>>>,
    pub table_import_channels: RwLock<HashMap<String, watch::Sender<String>>>,
    pub sql_file_executions: RwLock<HashMap<String, CancellationToken>>,
    pub login_rate_limit: Mutex<LoginRateLimit>,
    /// Table export temp files: export_id -> (file_path, format)
    pub export_files: RwLock<HashMap<String, (String, String)>>,
    /// Terminal progress for SQL file executions that have already finished,
    /// so late SSE subscribers (GET arrives after the task completed and the
    /// channel was cleaned up) can still retrieve the final status. The
    /// `Instant` records when the terminal progress was stored; entries older
    /// than the TTL are evicted lazily on read.
    pub sql_file_terminal_progress: RwLock<HashMap<String, (SqlFileProgress, std::time::Instant)>>,
}

impl WebState {
    pub async fn remove_sse_channel(&self, id: &str) {
        self.sse_channels.write().await.remove(id);
    }
}
