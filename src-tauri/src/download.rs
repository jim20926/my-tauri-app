use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tauri::{ipc::Channel, AppHandle, Manager};

const DOWNLOAD_INDEX_FILE: &str = "downloads.json";

// 序列化 (Serialize)：把 Rust 的東西 → 变成 JSON 文字寄出去
// 反序列化 (Deserialize)：把收到的 JSON 文字 → 变回 Rust 的東西
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadRequest {
    pub video_id: String,
    pub title: String,
    pub description: String,
    pub source_url: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadRecord {
    pub video_id: String,
    pub title: String,
    pub description: String,
    pub source_url: String,
    pub size_bytes: u64,
    pub file_name: String,
    pub local_path: String,
    pub downloaded_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DownloadEvent {
    Started {
        video_id: String,
        total_bytes: Option<u64>,
    },
    Progress {
        video_id: String,
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
        percentage: Option<u8>,
    },
    Completed {
        video_id: String,
        record: DownloadRecord,
    },
    Failed {
        video_id: String,
        error: DownloadError,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct DownloadIndex {
    version: u8,
    downloads: Vec<DownloadRecord>,
}

impl DownloadError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

fn app_data_dir(app: &AppHandle) -> Result<PathBuf, DownloadError> {
    app.path()
        .app_data_dir()
        .map_err(|error| DownloadError::new("APP_DATA_ERROR", error.to_string()))
}

fn validate_request(request: &DownloadRequest) -> Result<(), DownloadError> {
    if request.video_id.is_empty()
        || !request.video_id.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
    {
        return Err(DownloadError::new("INVALID_VIDEO_ID", "影片 ID 格式不正確"));
    }

    let url = reqwest::Url::parse(&request.source_url)
        .map_err(|_| DownloadError::new("INVALID_URL", "影片網址格式不正確"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(DownloadError::new(
            "INVALID_URL",
            "只允許 HTTP 或 HTTPS 影片網址",
        ));
    }

    Ok(())
}

fn file_paths(app: &AppHandle, video_id: &str) -> Result<(PathBuf, PathBuf), DownloadError> {
    let directory = app_data_dir(app)?;
    let file_name = format!("offline-video-{video_id}.mp4");
    let final_path = directory.join(&file_name);
    let temporary_path = directory.join(format!("{file_name}.part"));
    Ok((temporary_path, final_path))
}

fn index_path(app: &AppHandle) -> Result<PathBuf, DownloadError> {
    Ok(app_data_dir(app)?.join(DOWNLOAD_INDEX_FILE))
}

fn load_index(app: &AppHandle) -> Result<Vec<DownloadRecord>, DownloadError> {
    let path = index_path(app)?;
    if !path.exists() {
        return Ok(Vec::new());
    }

    let contents = fs::read_to_string(path)
        .map_err(|error| DownloadError::new("INDEX_READ_ERROR", error.to_string()))?;
    let index = serde_json::from_str::<DownloadIndex>(&contents)
        .map_err(|error| DownloadError::new("INDEX_PARSE_ERROR", error.to_string()))?;
    Ok(index
        .downloads
        .into_iter()
        .filter(|record| Path::new(&record.local_path).exists())
        .collect())
}

fn save_index(app: &AppHandle, downloads: &[DownloadRecord]) -> Result<(), DownloadError> {
    let directory = app_data_dir(app)?;
    fs::create_dir_all(&directory)
        .map_err(|error| DownloadError::new("APP_DATA_ERROR", error.to_string()))?;

    let path = index_path(app)?;
    let temporary_path = directory.join(format!("{DOWNLOAD_INDEX_FILE}.part"));
    let index = DownloadIndex {
        version: 1,
        downloads: downloads.to_vec(),
    };
    let serialized = serde_json::to_vec_pretty(&index)
        .map_err(|error| DownloadError::new("INDEX_SERIALIZE_ERROR", error.to_string()))?;
    fs::write(&temporary_path, serialized)
        .map_err(|error| DownloadError::new("INDEX_WRITE_ERROR", error.to_string()))?;
    fs::rename(temporary_path, path)
        .map_err(|error| DownloadError::new("INDEX_RENAME_ERROR", error.to_string()))?;
    Ok(())
}

async fn download_video_inner(
    app: &AppHandle,
    request: &DownloadRequest,
    channel: &Channel<DownloadEvent>,
) -> Result<DownloadRecord, DownloadError> {
    validate_request(request)?;
    let (temporary_path, final_path) = file_paths(app, &request.video_id)?;
    let directory = app_data_dir(app)?;
    fs::create_dir_all(&directory)
        .map_err(|error| DownloadError::new("APP_DATA_ERROR", error.to_string()))?;
    let _ = fs::remove_file(&temporary_path);

    let client = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| DownloadError::new("HTTP_CLIENT_ERROR", error.to_string()))?;
    let response = client
        .get(&request.source_url)
        .send()
        .await
        .map_err(|error| DownloadError::new("NETWORK_ERROR", error.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(DownloadError::new(
            "HTTP_ERROR",
            format!("影片伺服器回應 HTTP {}", status.as_u16()),
        ));
    }

    let total_bytes = response
        .content_length()
        .or((request.size_bytes > 0).then_some(request.size_bytes));
    channel
        .send(DownloadEvent::Started {
            video_id: request.video_id.clone(),
            total_bytes,
        })
        .map_err(|error| DownloadError::new("CHANNEL_ERROR", error.to_string()))?;

    let file = File::create(&temporary_path)
        .map_err(|error| DownloadError::new("FILE_CREATE_ERROR", error.to_string()))?;
    let mut writer = BufWriter::new(file);
    let mut response = response;
    let mut downloaded_bytes = 0_u64;

    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| DownloadError::new("NETWORK_ERROR", error.to_string()))?
    {
        writer
            .write_all(&chunk)
            .map_err(|error| DownloadError::new("FILE_WRITE_ERROR", error.to_string()))?;
        downloaded_bytes += chunk.len() as u64;
        let percentage = total_bytes.map(|total| {
            if total == 0 {
                0
            } else {
                ((downloaded_bytes.saturating_mul(100) / total).min(100)) as u8
            }
        });
        channel
            .send(DownloadEvent::Progress {
                video_id: request.video_id.clone(),
                downloaded_bytes,
                total_bytes,
                percentage,
            })
            .map_err(|error| DownloadError::new("CHANNEL_ERROR", error.to_string()))?;
    }

    writer
        .flush()
        .map_err(|error| DownloadError::new("FILE_WRITE_ERROR", error.to_string()))?;
    let file = writer
        .into_inner()
        .map_err(|error| DownloadError::new("FILE_WRITE_ERROR", error.to_string()))?;
    file.sync_all()
        .map_err(|error| DownloadError::new("FILE_WRITE_ERROR", error.to_string()))?;
    fs::rename(&temporary_path, &final_path)
        .map_err(|error| DownloadError::new("FILE_RENAME_ERROR", error.to_string()))?;

    let record = DownloadRecord {
        video_id: request.video_id.clone(),
        title: request.title.clone(),
        description: request.description.clone(),
        source_url: request.source_url.clone(),
        size_bytes: request.size_bytes,
        file_name: format!("offline-video-{}.mp4", request.video_id),
        local_path: final_path.to_string_lossy().into_owned(),
        downloaded_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string(),
    };
    let mut downloads = load_index(app)?;
    downloads.retain(|existing| existing.video_id != request.video_id);
    downloads.push(record.clone());
    save_index(app, &downloads)?;

    Ok(record)
}

#[tauri::command]
pub async fn download_video(
    app: AppHandle,
    request: DownloadRequest,
    on_event: Channel<DownloadEvent>,
) -> Result<DownloadRecord, DownloadError> {
    let video_id = request.video_id.clone();
    let result = download_video_inner(&app, &request, &on_event).await;

    match result {
        Ok(record) => {
            on_event
                .send(DownloadEvent::Completed {
                    video_id,
                    record: record.clone(),
                })
                .map_err(|error| DownloadError::new("CHANNEL_ERROR", error.to_string()))?;
            Ok(record)
        }
        Err(error) => {
            if let Ok((temporary_path, _)) = file_paths(&app, &request.video_id) {
                let _ = fs::remove_file(temporary_path);
            }
            let _ = on_event.send(DownloadEvent::Failed {
                video_id,
                error: error.clone(),
            });
            Err(error)
        }
    }
}

#[tauri::command]
pub fn list_downloads(app: AppHandle) -> Result<Vec<DownloadRecord>, DownloadError> {
    load_index(&app)
}

#[tauri::command]
pub fn delete_download(app: AppHandle, video_id: String) -> Result<(), DownloadError> {
    let mut downloads = load_index(&app)?;
    let Some(record) = downloads.iter().find(|record| record.video_id == video_id) else {
        return Err(DownloadError::new("NOT_FOUND", "找不到這支離線影片"));
    };

    fs::remove_file(&record.local_path)
        .map_err(|error| DownloadError::new("FILE_DELETE_ERROR", error.to_string()))?;
    downloads.retain(|existing| existing.video_id != video_id);
    save_index(&app, &downloads)
}
