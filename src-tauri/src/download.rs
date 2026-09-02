use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::{debug, error, info};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tauri::{ipc::Channel, AppHandle, Manager};

use crate::crypto::EncryptedFileWriter;

const DOWNLOAD_INDEX_FILE: &str = "downloads.json";

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

pub(crate) fn encrypted_path(app: &AppHandle, video_id: &str) -> Result<PathBuf, DownloadError> {
    let directory = app_data_dir(app)?;
    Ok(directory.join(format!("offline-video-{video_id}.enc")))
}

fn file_paths(app: &AppHandle, video_id: &str) -> Result<(PathBuf, PathBuf), DownloadError> {
    let final_path = encrypted_path(app, video_id)?;
    let temporary_path = final_path.with_extension("enc.part");
    Ok((temporary_path, final_path))
}

fn index_path(app: &AppHandle) -> Result<PathBuf, DownloadError> {
    Ok(app_data_dir(app)?.join(DOWNLOAD_INDEX_FILE))
}

pub(crate) fn load_index(app: &AppHandle) -> Result<Vec<DownloadRecord>, DownloadError> {
    let path = index_path(app)?;
    if !path.exists() {
        debug!(target: "download", "operation=list_index result=empty");
        return Ok(Vec::new());
    }

    let contents = fs::read_to_string(path)
        .map_err(|error| DownloadError::new("INDEX_READ_ERROR", error.to_string()))?;
    let index = serde_json::from_str::<DownloadIndex>(&contents)
        .map_err(|error| DownloadError::new("INDEX_PARSE_ERROR", error.to_string()))?;
    let downloads = index
        .downloads
        .into_iter()
        .filter(|record| {
            encrypted_path(app, &record.video_id)
                .map(|path| path.exists())
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    debug!(
        target: "download",
        "operation=list_index result=success count={}",
        downloads.len()
    );
    Ok(downloads)
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
    debug!(
        target: "download",
        "operation=save_index count={}",
        downloads.len()
    );
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
    let source_url = reqwest::Url::parse(&request.source_url)
        .map_err(|_| DownloadError::new("INVALID_URL", "影片網址格式不正確"))?;
    info!(
        target: "api",
        "operation=request method=GET host={} path={}",
        source_url.host_str().unwrap_or("unknown"),
        if source_url.path().is_empty() {
            "/"
        } else {
            source_url.path()
        }
    );
    let request_started = Instant::now();
    let response = client
        .get(&request.source_url)
        .send()
        .await
        .map_err(|error| {
            let error_kind = if error.is_timeout() {
                "timeout"
            } else if error.is_connect() {
                "connect"
            } else {
                "request"
            };
            error!(
                target: "api",
                "operation=response result=error duration_ms={} error_kind={}",
                request_started.elapsed().as_millis(),
                error_kind
            );
            DownloadError::new("NETWORK_ERROR", error.to_string())
        })?;
    let status = response.status();
    info!(
        target: "api",
        "operation=response status={} duration_ms={} content_length={}",
        status.as_u16(),
        request_started.elapsed().as_millis(),
        response.content_length().unwrap_or(0)
    );
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

    let mut writer = EncryptedFileWriter::create(&temporary_path, &request.video_id)
        .map_err(|error| DownloadError::new("ENCRYPTION_ERROR", error.to_string()))?;
    let mut response = response;
    let mut downloaded_bytes = 0_u64;
    let mut last_logged_percentage = None;

    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| DownloadError::new("NETWORK_ERROR", error.to_string()))?
    {
        writer
            .write_chunk(&chunk)
            .map_err(|error| DownloadError::new("ENCRYPTION_ERROR", error.to_string()))?;
        downloaded_bytes += chunk.len() as u64;
        let percentage = total_bytes.map(|total| {
            downloaded_bytes
                .saturating_mul(100)
                .checked_div(total)
                .unwrap_or(0)
                .min(100) as u8
        });
        channel
            .send(DownloadEvent::Progress {
                video_id: request.video_id.clone(),
                downloaded_bytes,
                total_bytes,
                percentage,
            })
            .map_err(|error| DownloadError::new("CHANNEL_ERROR", error.to_string()))?;
        if let Some(percentage) = percentage {
            let crossed_ten_percent = last_logged_percentage
                .map(|last| percentage / 10 > last / 10)
                .unwrap_or(true);
            if crossed_ten_percent || percentage == 100 {
                info!(
                    target: "download",
                    "operation=progress video_id={} percentage={} downloaded_bytes={} total_bytes={}",
                    request.video_id,
                    percentage,
                    downloaded_bytes,
                    total_bytes.unwrap_or(0)
                );
                last_logged_percentage = Some(percentage);
            }
        }
    }

    let actual_size = writer
        .finish()
        .map_err(|error| DownloadError::new("ENCRYPTION_ERROR", error.to_string()))?;
    if final_path.exists() {
        fs::remove_file(&final_path)
            .map_err(|error| DownloadError::new("FILE_REPLACE_ERROR", error.to_string()))?;
    }
    fs::rename(&temporary_path, &final_path)
        .map_err(|error| DownloadError::new("FILE_RENAME_ERROR", error.to_string()))?;

    let record = DownloadRecord {
        video_id: request.video_id.clone(),
        title: request.title.clone(),
        description: request.description.clone(),
        source_url: request.source_url.clone(),
        size_bytes: actual_size,
        file_name: format!("offline-video-{}.enc", request.video_id),
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
    info!(
        target: "download",
        "operation=start video_id={} expected_bytes={}",
        request.video_id,
        request.size_bytes
    );
    let result = download_video_inner(&app, &request, &on_event).await;

    match result {
        Ok(record) => {
            info!(
                target: "download",
                "operation=complete video_id={} bytes={} file={}",
                record.video_id, record.size_bytes, record.file_name
            );
            on_event
                .send(DownloadEvent::Completed {
                    video_id,
                    record: record.clone(),
                })
                .map_err(|error| {
                    error!(
                        target: "download",
                        "operation=complete_event_failed video_id={} code=CHANNEL_ERROR",
                        record.video_id
                    );
                    DownloadError::new("CHANNEL_ERROR", error.to_string())
                })?;
            Ok(record)
        }
        Err(error) => {
            error!(
                target: "download",
                "operation=failed video_id={} code={}",
                request.video_id, error.code
            );
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
    info!(target: "download", "operation=list_start");
    match load_index(&app) {
        Ok(downloads) => {
            info!(
                target: "download",
                "operation=list_complete count={}",
                downloads.len()
            );
            Ok(downloads)
        }
        Err(error) => {
            error!(
                target: "download",
                "operation=list_failed code={}",
                error.code
            );
            Err(error)
        }
    }
}

#[tauri::command]
pub fn delete_download(app: AppHandle, video_id: String) -> Result<(), DownloadError> {
    info!(
        target: "download",
        "operation=delete_start video_id={}",
        video_id
    );
    let result: Result<(), DownloadError> = (|| {
        let mut downloads = load_index(&app)?;
        if !downloads.iter().any(|record| record.video_id == video_id) {
            return Err(DownloadError::new("NOT_FOUND", "找不到這支離線影片"));
        }

        let path = encrypted_path(&app, &video_id)?;
        fs::remove_file(path)
            .map_err(|error| DownloadError::new("FILE_DELETE_ERROR", error.to_string()))?;
        downloads.retain(|existing| existing.video_id != video_id);
        save_index(&app, &downloads)
    })();
    match result {
        Ok(()) => {
            info!(
                target: "download",
                "operation=delete_complete video_id={}",
                video_id
            );
            Ok(())
        }
        Err(error) => {
            error!(
                target: "download",
                "operation=delete_failed video_id={} code={}",
                video_id,
                error.code
            );
            Err(error)
        }
    }
}
