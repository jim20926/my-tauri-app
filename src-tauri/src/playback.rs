use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{mpsc as std_mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use log::{debug, error, info};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use zeroize::Zeroizing;

use crate::crypto::{CryptoError, CryptoErrorKind, EncryptedFileReader, CHUNK_SIZE};
use crate::download;

const CACHE_CAPACITY_BYTES: usize = CHUNK_SIZE as usize * 8;
const CACHE_CHANNEL_CAPACITY: usize = 4;
const SESSION_IDLE_TTL: Duration = Duration::from_secs(5 * 60);

type ResponseBody = BoxBody<Bytes, Infallible>;
type HttpResponse = Response<ResponseBody>;

#[derive(Clone, Default)]
pub struct PlaybackState {
    sessions: Arc<Mutex<HashMap<String, PlaybackSession>>>,
}

#[derive(Clone)]
struct PlaybackSession {
    video_id: String,                   // 播放哪部影片
    path: PathBuf,                      // .enc 檔案位置
    original_size: u64,                 // 原始 MP4 大小
    dek: Arc<Zeroizing<[u8; 32]>>,      // 暫存在記憶體中的 DEK
    cache: Arc<ChunkCache>,             // 已解密的 chunk 快取
    last_access: Arc<Mutex<Instant>>,   // 最近一次播放請求時間
}

#[derive(Default)]
struct CacheState {
    entries: HashMap<u64, Arc<Zeroizing<Vec<u8>>>>,
    order: VecDeque<u64>,
    loading: HashSet<u64>,
    bytes: usize,
}

struct ChunkCache {
    state: Mutex<CacheState>,
    changed: Condvar,
}

impl Default for ChunkCache {
    fn default() -> Self {
        Self {
            state: Mutex::new(CacheState::default()),
            changed: Condvar::new(),
        }
    }
}

impl ChunkCache {
    #[allow(clippy::type_complexity)]
    fn get_or_load<F>(
        &self,
        chunk_index: u64,
        loader: F,
    ) -> Result<(Arc<Zeroizing<Vec<u8>>>, bool), CryptoError>
    where
        F: FnOnce() -> Result<Zeroizing<Vec<u8>>, CryptoError>,
    {
        let mut loader = Some(loader);
        loop {
            let mut state = self
                .state
                .lock()
                .map_err(|_| CryptoError::new("影片快取狀態無法使用"))?;
            if let Some(chunk) = state.entries.get(&chunk_index).cloned() {
                state.order.retain(|index| *index != chunk_index);
                state.order.push_back(chunk_index);
                return Ok((chunk, true));
            }
            if state.loading.contains(&chunk_index) {
                state = self
                    .changed
                    .wait(state)
                    .map_err(|_| CryptoError::new("影片快取等待失敗"))?;
                drop(state);
                continue;
            }
            state.loading.insert(chunk_index);
            drop(state);

            let loaded = loader.take().expect("chunk loader can only run once")();
            let mut state = self
                .state
                .lock()
                .map_err(|_| CryptoError::new("影片快取狀態無法使用"))?;
            state.loading.remove(&chunk_index);
            let chunk = match loaded {
                Ok(chunk) => Arc::new(chunk),
                Err(error) => {
                    self.changed.notify_all();
                    return Err(error);
                }
            };
            if let Some(previous) = state.entries.insert(chunk_index, chunk.clone()) {
                state.bytes = state.bytes.saturating_sub(previous.len());
            }
            state.bytes += chunk.len();
            state.order.retain(|index| *index != chunk_index);
            state.order.push_back(chunk_index);
            while state.bytes > CACHE_CAPACITY_BYTES {
                let Some(oldest) = state.order.pop_front() else {
                    break;
                };
                if let Some(evicted) = state.entries.remove(&oldest) {
                    state.bytes = state.bytes.saturating_sub(evicted.len());
                }
            }
            self.changed.notify_all();
            return Ok((chunk, false));
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackInfo {
    pub session_id: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlaybackError {
    pub code: String,
    pub message: String,
}

impl PlaybackError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

pub struct PlaybackServer {
    port: u16,
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl PlaybackServer {
    pub fn start(state: PlaybackState) -> Result<Self, String> {
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("playback-http".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!("無法建立影片 HTTP runtime：{error}")));
                        return;
                    }
                };
                if let Err(error) = runtime.block_on(run_server(state, shutdown_rx, ready_tx)) {
                    error!(target: "playback", "operation=http_server_failed error={error}");
                }
            })
            .map_err(|error| format!("無法啟動影片 HTTP thread：{error}"))?;

        match ready_rx.recv() {
            Ok(Ok(port)) => {
                info!(target: "playback", "operation=http_server_started bind=127.0.0.1 port={port}");
                Ok(Self {
                    port,
                    shutdown: Mutex::new(Some(shutdown_tx)),
                    thread: Mutex::new(Some(thread)),
                })
            }
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(error) => {
                let _ = thread.join();
                Err(format!("影片 HTTP server 未回報啟動狀態：{error}"))
            }
        }
    }

    fn url_for(&self, session_id: &str) -> String {
        format!(
            "http://127.0.0.1:{}/playback/{session_id}/video.mp4",
            self.port
        )
    }
}

impl Drop for PlaybackServer {
    fn drop(&mut self) {
        if let Ok(mut shutdown) = self.shutdown.lock() {
            if let Some(sender) = shutdown.take() {
                let _ = sender.send(());
            }
        }
        if let Ok(mut thread) = self.thread.lock() {
            if let Some(handle) = thread.take() {
                let _ = handle.join();
            }
        }
        info!(target: "playback", "operation=http_server_stopped");
    }
}

async fn run_server(
    state: PlaybackState,
    mut shutdown: oneshot::Receiver<()>,
    ready_tx: std_mpsc::SyncSender<Result<u16, String>>,
) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|error| format!("無法綁定影片 HTTP server：{error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("無法取得影片 HTTP server port：{error}"))?
        .port();
    ready_tx
        .send(Ok(port))
        .map_err(|_| "影片 HTTP server 啟動回報失敗".to_string())?;

    let mut cleanup = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = cleanup.tick() => state.cleanup_idle_sessions(),
            result = listener.accept() => {
                let (stream, peer) = result.map_err(|error| format!("影片 HTTP accept 失敗：{error}"))?;
                let state = state.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let state = state.clone();
                        async move { Ok::<_, Infallible>(handle_request(&state, request)) }
                    });
                    if let Err(error) = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        debug!(target: "playback", "operation=http_connection_closed peer={peer} error={error}");
                    }
                });
            }
        }
    }
    Ok(())
}

fn session_id() -> Result<String, PlaybackError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| PlaybackError::new("SESSION_ERROR", error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[tauri::command]
pub fn open_playback(
    app: tauri::AppHandle,
    state: tauri::State<'_, PlaybackState>,
    server: tauri::State<'_, PlaybackServer>,
    video_id: String,
) -> Result<PlaybackInfo, PlaybackError> {
    info!(target: "playback", "operation=open_start video_id={video_id}");
    let result: Result<PlaybackInfo, PlaybackError> = (|| {
        let record = download::load_index(&app)
            .map_err(|error| PlaybackError::new(&error.code, error.message))?
            .into_iter()
            .find(|record| record.video_id == video_id)
            .ok_or_else(|| PlaybackError::new("NOT_FOUND", "找不到這支已加密的離線影片"))?;
        let path = download::encrypted_path(&app, &record.video_id)
            .map_err(|error| PlaybackError::new(&error.code, error.message))?;
        let reader = EncryptedFileReader::open(&path, &record.video_id)
            .map_err(|error| PlaybackError::new("PLAYBACK_KEY_ERROR", error.message))?;
        let original_size = reader.original_size();
        let dek = Arc::new(reader.clone_dek());
        let session_id = session_id()?;
        state
            .sessions
            .lock()
            .map_err(|_| PlaybackError::new("SESSION_ERROR", "播放工作階段狀態無法使用"))?
            .insert(
                session_id.clone(),
                PlaybackSession {
                    video_id: video_id.clone(),
                    path,
                    original_size,
                    dek,
                    cache: Arc::new(ChunkCache::default()),
                    last_access: Arc::new(Mutex::new(Instant::now())),
                },
            );

        Ok(PlaybackInfo {
            url: server.url_for(&session_id),
            session_id,
        })
    })();
    match result {
        Ok(info) => {
            info!(target: "playback", "operation=open_complete video_id={video_id}");
            Ok(info)
        }
        Err(error) => {
            error!(
                target: "playback",
                "operation=open_failed video_id={} code={} message={}",
                video_id,
                error.code,
                error.message
            );
            Err(error)
        }
    }
}

#[tauri::command]
pub fn close_playback(
    state: tauri::State<'_, PlaybackState>,
    session_id: String,
) -> Result<(), PlaybackError> {
    info!(target: "playback", "operation=close_start");
    let result: Result<bool, PlaybackError> = state
        .sessions
        .lock()
        .map_err(|_| PlaybackError::new("SESSION_ERROR", "播放工作階段狀態無法使用"))
        .map(|mut sessions| sessions.remove(&session_id).is_some());
    match result {
        Ok(removed) => {
            info!(target: "playback", "operation=close_complete removed={removed}");
            Ok(())
        }
        Err(error) => {
            error!(
                target: "playback",
                "operation=close_failed code={} message={}",
                error.code,
                error.message
            );
            Err(error)
        }
    }
}

impl PlaybackState {
    fn cleanup_idle_sessions(&self) {
        let Ok(mut sessions) = self.sessions.lock() else {
            error!(target: "playback", "operation=session_cleanup_failed reason=state_unavailable");
            return;
        };
        let before = sessions.len();
        sessions.retain(|_, session| {
            session
                .last_access
                .lock()
                .map(|last_access| last_access.elapsed() < SESSION_IDLE_TTL)
                .unwrap_or(false)
        });
        let removed = before.saturating_sub(sessions.len());
        if removed > 0 {
            info!(target: "playback", "operation=session_cleanup removed={removed}");
        }
    }

    #[allow(clippy::result_large_err)]
    fn session(&self, session_id: &str) -> Result<Option<PlaybackSession>, HttpResponse> {
        let session = self
            .sessions
            .lock()
            .map_err(|_| {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "播放工作階段狀態無法使用",
                )
            })?
            .get(session_id)
            .cloned();
        if let Some(session) = &session {
            if let Ok(mut last_access) = session.last_access.lock() {
                *last_access = Instant::now();
            }
        }
        Ok(session)
    }
}

fn path_shape(path: &str) -> String {
    let segments = path.trim_start_matches('/').split('/').collect::<Vec<_>>();
    let token = segments.get(1).copied().unwrap_or_default();
    format!(
        "segment_count={} prefix_ok={} token_length={} token_hex={} filename_ok={} exact_shape={}",
        segments.len(),
        segments.first().copied() == Some("playback"),
        token.len(),
        !token.is_empty() && token.chars().all(|character| character.is_ascii_hexdigit()),
        segments.get(2).copied() == Some("video.mp4"),
        segments.len() == 3,
    )
}

fn parse_path(path: &str) -> Option<String> {
    let mut segments = path.trim_start_matches('/').split('/');
    if segments.next()? != "playback" {
        return None;
    }
    let session_id = segments.next()?;
    if session_id.len() != 64
        || !session_id
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return None;
    }
    if segments.next()? != "video.mp4" || segments.next().is_some() {
        return None;
    }
    Some(session_id.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ByteRange {
    start: u64,
    end: u64,
    partial: bool,
}

fn parse_range(value: Option<&str>, size: u64) -> Result<ByteRange, ()> {
    if size == 0 {
        return Err(());
    }
    let Some(value) = value else {
        return Ok(ByteRange {
            start: 0,
            end: size,
            partial: false,
        });
    };
    let Some(spec) = value.strip_prefix("bytes=") else {
        return Err(());
    };
    if spec.contains(',') {
        return Err(());
    }
    let Some((start, end)) = spec.split_once('-') else {
        return Err(());
    };
    if start.is_empty() {
        let suffix = end.parse::<u64>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        return Ok(ByteRange {
            start: size.saturating_sub(suffix),
            end: size,
            partial: true,
        });
    }
    let range_start = start.parse::<u64>().map_err(|_| ())?;
    if range_start >= size {
        return Err(());
    }
    let requested_end = if end.is_empty() {
        size
    } else {
        end.parse::<u64>()
            .map_err(|_| ())?
            .saturating_add(1)
            .min(size)
    };
    if requested_end <= range_start {
        return Err(());
    }
    Ok(ByteRange {
        start: range_start,
        end: requested_end,
        partial: true,
    })
}

fn full_body(data: impl Into<Bytes>) -> ResponseBody {
    Full::new(data.into()).boxed()
}

fn error_response(status: StatusCode, message: &str) -> HttpResponse {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .header("cache-control", "no-store")
        .body(full_body(Bytes::copy_from_slice(message.as_bytes())))
        .unwrap()
}

fn diagnostic_error_response(status: StatusCode, reason: &str, message: &str) -> HttpResponse {
    debug!(
        target: "playback",
        "[PLAYBACK-DIAG] operation=http_early_return status={} reason={}",
        status.as_u16(),
        reason
    );
    error_response(status, message)
}

fn crypto_error_response(error: CryptoError) -> HttpResponse {
    let (status, message) = match error.kind {
        CryptoErrorKind::NotFound => (StatusCode::NOT_FOUND, "影片檔案不存在"),
        CryptoErrorKind::KeyUnavailable => (StatusCode::FORBIDDEN, "無法取得影片解密金鑰"),
        CryptoErrorKind::InvalidFormat | CryptoErrorKind::Corrupt => {
            (StatusCode::UNPROCESSABLE_ENTITY, "影片加密檔案驗證失敗")
        }
        CryptoErrorKind::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "影片無法讀取"),
    };
    error!(target: "playback", "operation=http_failed error={error}");
    error_response(status, message)
}

#[allow(clippy::explicit_auto_deref)]
fn load_chunk(
    session: &PlaybackSession,
    chunk_index: u64,
) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    let mut reader =
        EncryptedFileReader::open_with_dek(&session.path, &session.video_id, &**session.dek)?;
    reader.read_chunk(chunk_index)
}

fn cached_chunk(
    session: &PlaybackSession,
    chunk_index: u64,
) -> Result<Arc<Zeroizing<Vec<u8>>>, CryptoError> {
    let (chunk, hit) = session
        .cache
        .get_or_load(chunk_index, || load_chunk(session, chunk_index))?;
    debug!(
        target: "playback",
        "operation=chunk_cache {} video_id={} chunk_index={} bytes={}",
        if hit { "hit" } else { "miss" },
        session.video_id,
        chunk_index,
        chunk.len()
    );
    Ok(chunk)
}

fn stream_range(
    session: PlaybackSession,
    range: ByteRange,
    sender: mpsc::Sender<Result<Frame<Bytes>, Infallible>>,
) {
    let first_chunk = range.start / CHUNK_SIZE;
    let last_chunk = (range.end - 1) / CHUNK_SIZE;
    for chunk_index in first_chunk..=last_chunk {
        let chunk = match cached_chunk(&session, chunk_index) {
            Ok(chunk) => chunk,
            Err(error) => {
                error!(
                    target: "playback",
                    "operation=stream_failed video_id={} chunk_index={} error={error}",
                    session.video_id,
                    chunk_index
                );
                return;
            }
        };
        let chunk_start = chunk_index * CHUNK_SIZE;
        let from = range.start.max(chunk_start) - chunk_start;
        let to = range.end.min(chunk_start + chunk.len() as u64) - chunk_start;
        let bytes = Bytes::copy_from_slice(&chunk[from as usize..to as usize]);
        if sender.blocking_send(Ok(Frame::data(bytes))).is_err() {
            info!(
                target: "playback",
                "operation=stream_cancelled video_id={} chunk_index={}",
                session.video_id,
                chunk_index
            );
            return;
        }
    }
    debug!(
        target: "playback",
        "operation=stream_complete video_id={} start={} end={} bytes={}",
        session.video_id,
        range.start,
        range.end,
        range.end - range.start
    );
}

fn response_for_range(
    request: &Request<Incoming>,
    session: PlaybackSession,
    range: ByteRange,
) -> HttpResponse {
    let response_length = range.end - range.start;
    let status = if range.partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let mut builder = Response::builder()
        .status(status)
        .header("content-type", "video/mp4")
        .header("content-length", response_length.to_string())
        .header("accept-ranges", "bytes")
        .header("cache-control", "no-store, no-cache, must-revalidate");
    if range.partial {
        builder = builder.header(
            "content-range",
            format!(
                "bytes {}-{}/{}",
                range.start,
                range.end - 1,
                session.original_size
            ),
        );
    }
    if request.method() == Method::HEAD {
        return builder.body(full_body(Bytes::new())).unwrap();
    }

    if let Err(error) = cached_chunk(&session, range.start / CHUNK_SIZE) {
        return crypto_error_response(error);
    }
    let (sender, receiver) = mpsc::channel(CACHE_CHANNEL_CAPACITY);
    let Ok(_) = std::thread::Builder::new()
        .name("playback-stream".to_string())
        .spawn({
            let session = session.clone();
            move || stream_range(session, range, sender)
        })
    else {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "stream thread failed");
    };
    let body = StreamBody::new(ReceiverStream::new(receiver)).boxed();
    builder.body(body).unwrap()
}

fn handle_request(state: &PlaybackState, request: Request<Incoming>) -> HttpResponse {
    let started = Instant::now();
    let method = request.method().clone();
    let range = request
        .headers()
        .get("range")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("none");
    debug!(
        target: "playback",
        "operation=http_request method={} path=video.mp4 range={}",
        method,
        range
    );
    if method != Method::GET && method != Method::HEAD {
        return diagnostic_error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "只支援 GET 或 HEAD",
        );
    }
    let Some(session_id) = parse_path(request.uri().path()) else {
        debug!(
            target: "playback",
            "[PLAYBACK-DIAG] operation=route_rejected reason=invalid_path {}",
            path_shape(request.uri().path())
        );
        return diagnostic_error_response(
            StatusCode::NOT_FOUND,
            "invalid_path",
            "播放工作階段不存在",
        );
    };
    debug!(
        target: "playback",
        "[PLAYBACK-DIAG] operation=route_accepted session_token_length={} path_shape=expected",
        session_id.len()
    );
    let session = match state.session(&session_id) {
        Ok(Some(session)) => {
            debug!(
                target: "playback",
                "[PLAYBACK-DIAG] operation=session_lookup result=hit"
            );
            session
        }
        Ok(None) => {
            debug!(
                target: "playback",
                "[PLAYBACK-DIAG] operation=session_lookup result=miss"
            );
            return diagnostic_error_response(
                StatusCode::NOT_FOUND,
                "session_not_found",
                "播放工作階段不存在",
            );
        }
        Err(response) => return response,
    };
    let range_header = request
        .headers()
        .get("range")
        .and_then(|value| value.to_str().ok());
    let range = match parse_range(range_header, session.original_size) {
        Ok(range) => {
            debug!(
                target: "playback",
                "[PLAYBACK-DIAG] operation=range_accepted start={} end={} partial={}",
                range.start,
                range.end,
                range.partial
            );
            range
        }
        Err(()) => {
            debug!(
                target: "playback",
                "[PLAYBACK-DIAG] operation=range_rejected reason=invalid_range header_present={}",
                range_header.is_some()
            );
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(
                    "content-range",
                    format!("bytes */{}", session.original_size),
                )
                .header("cache-control", "no-store")
                .body(full_body(Bytes::new()))
                .unwrap();
        }
    };
    debug!(
        target: "playback",
        "[PLAYBACK-DIAG] operation=first_chunk_validation result=start chunk_index={}",
        range.start / CHUNK_SIZE
    );
    let response = response_for_range(&request, session, range);
    debug!(
        target: "playback",
        "operation=http_response status={} content_length={} duration_ms={}",
        response.status().as_u16(),
        response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("unknown"),
        started.elapsed().as_millis()
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn ranges_support_full_and_partial_requests() {
        assert_eq!(
            parse_range(None, 10 * 1024 * 1024),
            Ok(ByteRange {
                start: 0,
                end: 10 * 1024 * 1024,
                partial: false,
            })
        );
        assert_eq!(
            parse_range(Some("bytes=2-7"), 100),
            Ok(ByteRange {
                start: 2,
                end: 8,
                partial: true,
            })
        );
    }

    #[test]
    fn invalid_multi_ranges_are_rejected() {
        assert!(parse_range(Some("bytes=0-1,2-3"), 100).is_err());
        assert!(parse_range(Some("bytes=10-9"), 100).is_err());
    }

    #[test]
    fn playback_path_accepts_prefixed_session_route() {
        let session_id = "a".repeat(64);
        let path = format!("/playback/{session_id}/video.mp4");

        assert_eq!(parse_path(&path), Some(session_id));
    }

    #[test]
    fn cache_reuses_a_verified_chunk() {
        let cache = ChunkCache::default();
        let calls = AtomicUsize::new(0);
        let first = cache
            .get_or_load(0, || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Zeroizing::new(vec![1_u8, 2_u8, 3_u8]))
            })
            .unwrap();
        let second = cache
            .get_or_load(0, || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Zeroizing::new(vec![9_u8]))
            })
            .unwrap();
        assert!(!first.1);
        assert!(second.1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(second.0.as_slice(), &[1_u8, 2_u8, 3_u8]);
    }
}
