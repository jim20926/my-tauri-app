use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

const WATCH_PROGRESS_INDEX_FILE: &str = "watch-progress.json";
const WATCH_PROGRESS_VERSION: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SyncState {
    Pending,
    Synced,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchProgressRecord {
    pub video_id: String,
    pub position_seconds: f64,
    pub duration_seconds: f64,
    pub completed: bool,
    pub updated_at: String,
    pub sync_state: SyncState,
    pub last_synced_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveWatchProgressRequest {
    pub video_id: String,
    pub position_seconds: f64,
    pub duration_seconds: f64,
    pub completed: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchProgressSyncToken {
    pub video_id: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyRemoteProgressRequest {
    pub records: Vec<WatchProgressRecord>,
    pub sent: Vec<WatchProgressSyncToken>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchProgressError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WatchProgressIndex {
    version: u8,
    progress: Vec<WatchProgressRecord>,
}

impl WatchProgressError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

fn app_data_dir(app: &AppHandle) -> Result<PathBuf, WatchProgressError> {
    app.path()
        .app_data_dir()
        .map_err(|error| WatchProgressError::new("APP_DATA_ERROR", error.to_string()))
}

fn index_path(app: &AppHandle) -> Result<PathBuf, WatchProgressError> {
    Ok(app_data_dir(app)?.join(WATCH_PROGRESS_INDEX_FILE))
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

fn validate_video_id(video_id: &str) -> Result<(), WatchProgressError> {
    if video_id.is_empty()
        || !video_id.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
    {
        return Err(WatchProgressError::new(
            "INVALID_VIDEO_ID",
            "影片 ID 格式不正確",
        ));
    }
    Ok(())
}

fn load_index(app: &AppHandle) -> Result<Vec<WatchProgressRecord>, WatchProgressError> {
    let path = index_path(app)?;
    if !path.exists() {
        return Ok(Vec::new());
    }

    let contents = fs::read_to_string(path)
        .map_err(|error| WatchProgressError::new("INDEX_READ_ERROR", error.to_string()))?;
    let index = serde_json::from_str::<WatchProgressIndex>(&contents)
        .map_err(|error| WatchProgressError::new("INDEX_PARSE_ERROR", error.to_string()))?;
    if index.version != WATCH_PROGRESS_VERSION {
        return Err(WatchProgressError::new(
            "INDEX_VERSION_ERROR",
            "觀看進度檔案版本不支援",
        ));
    }
    Ok(index.progress)
}

fn save_index(app: &AppHandle, progress: &[WatchProgressRecord]) -> Result<(), WatchProgressError> {
    let directory = app_data_dir(app)?;
    fs::create_dir_all(&directory)
        .map_err(|error| WatchProgressError::new("APP_DATA_ERROR", error.to_string()))?;

    let path = index_path(app)?;
    let temporary_path = directory.join(format!("{WATCH_PROGRESS_INDEX_FILE}.part"));
    let index = WatchProgressIndex {
        version: WATCH_PROGRESS_VERSION,
        progress: progress.to_vec(),
    };
    let serialized = serde_json::to_vec_pretty(&index)
        .map_err(|error| WatchProgressError::new("INDEX_SERIALIZE_ERROR", error.to_string()))?;
    fs::write(&temporary_path, serialized)
        .map_err(|error| WatchProgressError::new("INDEX_WRITE_ERROR", error.to_string()))?;
    fs::rename(temporary_path, path)
        .map_err(|error| WatchProgressError::new("INDEX_RENAME_ERROR", error.to_string()))?;
    Ok(())
}

#[tauri::command]
pub fn list_watch_progress(app: AppHandle) -> Result<Vec<WatchProgressRecord>, WatchProgressError> {
    load_index(&app)
}

#[tauri::command]
pub fn save_watch_progress(
    app: AppHandle,
    request: SaveWatchProgressRequest,
) -> Result<WatchProgressRecord, WatchProgressError> {
    validate_video_id(&request.video_id)?;
    if !request.position_seconds.is_finite()
        || !request.duration_seconds.is_finite()
        || request.position_seconds < 0.0
        || request.duration_seconds <= 0.0
    {
        return Err(WatchProgressError::new(
            "INVALID_PROGRESS",
            "影片播放進度格式不正確",
        ));
    }

    let position_seconds = request
        .position_seconds
        .min(request.duration_seconds)
        .max(0.0);
    let record = WatchProgressRecord {
        video_id: request.video_id,
        position_seconds: if request.completed {
            request.duration_seconds
        } else {
            position_seconds
        },
        duration_seconds: request.duration_seconds,
        completed: request.completed,
        updated_at: now(),
        sync_state: SyncState::Pending,
        last_synced_at: None,
    };

    let mut progress = load_index(&app)?;
    progress.retain(|existing| existing.video_id != record.video_id);
    progress.push(record.clone());
    save_index(&app, &progress)?;
    Ok(record)
}

#[tauri::command]
pub fn mark_watch_progress_synced(
    app: AppHandle,
    items: Vec<WatchProgressSyncToken>,
) -> Result<Vec<WatchProgressRecord>, WatchProgressError> {
    let tokens = items
        .into_iter()
        .map(|item| (item.video_id, item.updated_at))
        .collect::<HashMap<_, _>>();
    let synced_at = now();
    let mut progress = load_index(&app)?;
    for record in &mut progress {
        if tokens
            .get(&record.video_id)
            .is_some_and(|updated_at| updated_at == &record.updated_at)
        {
            record.sync_state = SyncState::Synced;
            record.last_synced_at = Some(synced_at.clone());
        }
    }
    save_index(&app, &progress)?;
    Ok(progress)
}

#[tauri::command]
pub fn apply_remote_watch_progress(
    app: AppHandle,
    request: ApplyRemoteProgressRequest,
) -> Result<Vec<WatchProgressRecord>, WatchProgressError> {
    let sent = request
        .sent
        .into_iter()
        .map(|item| (item.video_id, item.updated_at))
        .collect::<HashMap<_, _>>();
    let synced_at = now();
    let mut progress = load_index(&app)?;

    for remote in request.records {
        validate_video_id(&remote.video_id)?;
        let can_replace = progress
            .iter()
            .find(|local| local.video_id == remote.video_id)
            .map(|local| {
                sent.get(&local.video_id)
                    .is_some_and(|sent_at| sent_at == &local.updated_at)
            })
            .unwrap_or(true);
        if !can_replace {
            continue;
        }

        let mut synced_record = remote;
        synced_record.sync_state = SyncState::Synced;
        synced_record.last_synced_at = Some(synced_at.clone());
        progress.retain(|local| local.video_id != synced_record.video_id);
        progress.push(synced_record);
    }

    save_index(&app, &progress)?;
    Ok(progress)
}

pub(crate) fn delete_progress(app: &AppHandle, video_id: &str) -> Result<(), WatchProgressError> {
    validate_video_id(video_id)?;
    let mut progress = load_index(app)?;
    let original_len = progress.len();
    progress.retain(|record| record.video_id != video_id);
    if progress.len() != original_len {
        save_index(app, &progress)?;
    }
    Ok(())
}
