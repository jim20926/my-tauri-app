import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { confirm } from "@tauri-apps/plugin-dialog";
import "./App.css";
import {
  useWatchProgress,
  type ProgressSaveReason,
  type SaveWatchProgressRequest,
  type WatchProgress,
} from "./watchProgress";

const CATALOG_URL = "http://localhost:3001/videos.json";
const HEALTH_URL = "http://localhost:3001/health";
const WATCH_PROGRESS_URL = "http://localhost:3001/watch-progress";

type VideoDefinition = {
  id: string;
  title: string;
  description: string;
  filename: string;
  sizeBytes: number;
  sourceUrl: string;
};

type DownloadRequest = {
  videoId: string;
  title: string;
  description: string;
  sourceUrl: string;
  sizeBytes: number;
};

type DownloadRecord = {
  videoId: string;
  title: string;
  description: string;
  sourceUrl: string;
  sizeBytes: number;
  fileName: string;
  downloadedAt: string;
};

type PlaybackInfo = {
  sessionId: string;
  url: string;
};

type DownloadError = { code: string; message: string };

type DownloadEvent =
  | { type: "started"; videoId: string; totalBytes: number | null }
  | {
      type: "progress";
      videoId: string;
      downloadedBytes: number;
      totalBytes: number | null;
      percentage: number | null;
    }
  | { type: "completed"; videoId: string; record: DownloadRecord }
  | { type: "failed"; videoId: string; error: DownloadError };

type DownloadMap = Record<string, DownloadRecord>;

type WatchProgressSyncToken = Pick<WatchProgress, "videoId" | "updatedAt">;
type WatchProgressMap = Record<string, WatchProgress>;
type ServerStatus = "checking" | "online" | "offline";
type SyncStatus = "idle" | "syncing" | "synced" | "error";

function formatBytes(bytes: number) {
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function formatDate(value: string) {
  const timestamp = Number(value);
  const date = Number.isFinite(timestamp) ? new Date(timestamp * 1000) : new Date(value);
  return new Intl.DateTimeFormat("zh-TW", {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  }).format(date);
}

function getInvokeError(error: unknown, fallback: string) {
  if (typeof error === "string") return error;
  if (error && typeof error === "object" && "message" in error) return String(error.message);
  return fallback;
}

function recordToVideo(record: DownloadRecord): VideoDefinition {
  return {
    id: record.videoId,
    title: record.title,
    description: record.description,
    filename: record.fileName,
    sizeBytes: record.sizeBytes,
    sourceUrl: record.sourceUrl,
  };
}

function formatPlaybackTime(seconds: number) {
  const safeSeconds = Math.max(0, Math.floor(seconds));
  const minutes = Math.floor(safeSeconds / 60);
  const remainingSeconds = safeSeconds % 60;
  return `${minutes}:${String(remainingSeconds).padStart(2, "0")}`;
}

function App() {
  const [catalog, setCatalog] = useState<VideoDefinition[]>([]);
  const [downloads, setDownloads] = useState<DownloadMap>({});
  const [watchProgress, setWatchProgress] = useState<WatchProgressMap>({});
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [serverStatus, setServerStatus] = useState<ServerStatus>("checking");
  const [activeDownloadId, setActiveDownloadId] = useState<string | null>(null);
  const [downloadProgress, setDownloadProgress] = useState<number | null>(0);
  const [downloadedBytes, setDownloadedBytes] = useState(0);
  const [errorMessage, setErrorMessage] = useState<string | null>(null);
  const [playbackUrl, setPlaybackUrl] = useState<string | null>(null);
  const [playbackErrorMessage, setPlaybackErrorMessage] = useState<string | null>(null);
  const [localDataLoading, setLocalDataLoading] = useState(true);
  const [localDataError, setLocalDataError] = useState<string | null>(null);
  const [remoteErrorMessage, setRemoteErrorMessage] = useState<string | null>(null);
  const [syncStatus, setSyncStatus] = useState<SyncStatus>("idle");
  const [syncErrorMessage, setSyncErrorMessage] = useState<string | null>(null);

  const downloadsRef = useRef<DownloadMap>({});
  const serverStatusRef = useRef<ServerStatus>("checking");
  const watchProgressRef = useRef<WatchProgressMap>({});
  const progressQueueRef = useRef<Record<string, SaveWatchProgressRequest>>({});
  const progressSaveInFlightRef = useRef(false);
  const syncRetryTimerRef = useRef<number | null>(null);

  useEffect(() => {
    downloadsRef.current = downloads;
  }, [downloads]);

  useEffect(() => {
    serverStatusRef.current = serverStatus;
  }, [serverStatus]);

  useEffect(() => {
    watchProgressRef.current = watchProgress;
  }, [watchProgress]);

  const visibleVideos = useMemo(() => {
    if (serverStatus === "online") return catalog;
    return Object.values(downloads).map(recordToVideo);
  }, [catalog, downloads, serverStatus]);

  const selectedVideo = visibleVideos.find((video) => video.id === selectedId) ?? null;
  const selectedDownload = selectedVideo ? downloads[selectedVideo.id] : undefined;

  const drainProgressSaves = useCallback(async () => {
    if (progressSaveInFlightRef.current) return;
    const [videoId, request] = Object.entries(progressQueueRef.current)[0] ?? [];
    if (!videoId || !request) return;

    progressSaveInFlightRef.current = true;
    delete progressQueueRef.current[videoId];
    let savedSuccessfully = false;
    try {
      const saved = await invoke<WatchProgress>("save_watch_progress", { request });
      setWatchProgress((current) => ({ ...current, [saved.videoId]: saved }));
      savedSuccessfully = true;
    } catch (error) {
      progressQueueRef.current[videoId] = request;
      setErrorMessage(getInvokeError(error, "觀看進度儲存失敗"));
    } finally {
      progressSaveInFlightRef.current = false;
      if (savedSuccessfully && Object.keys(progressQueueRef.current).length > 0) {
        void drainProgressSaves();
      }
    }
  }, []);

  const queueProgressSave = useCallback((snapshot: SaveWatchProgressRequest, _reason: ProgressSaveReason) => {
    if (serverStatusRef.current !== "offline" || !downloadsRef.current[snapshot.videoId]) return;
    progressQueueRef.current[snapshot.videoId] = snapshot;
    void drainProgressSaves();
  }, [drainProgressSaves]);

  const {
    videoProps,
    resumePrompt,
    continuePlayback,
    restartPlayback,
    flushProgress,
    prepareForSwitch,
  } = useWatchProgress({
    videoId: selectedVideo?.id ?? null,
    sourceKey: playbackUrl,
    enabled: Boolean(selectedDownload) && serverStatus === "offline",
    persistedProgress: selectedVideo ? watchProgress[selectedVideo.id] ?? null : null,
    onSnapshot: queueProgressSave,
  });

  const loadLocalData = useCallback(async () => {
    setLocalDataLoading(true);
    setLocalDataError(null);
    setErrorMessage(null);
    setRemoteErrorMessage(null);
    setServerStatus("checking");

    const catalogPromise: Promise<{ catalog: VideoDefinition[] | null; error: string | null }> = fetch(CATALOG_URL, {
      cache: "no-store",
    })
      .then(async (response) => {
        if (!response.ok) throw new Error(`Catalog request failed: ${response.status}`);
        return { catalog: (await response.json()) as VideoDefinition[], error: null };
      })
      .catch((error) => ({
        catalog: null,
        error: error instanceof Error ? error.message : "無法載入遠端影片資料",
      }));

    try {
      const [storedRecords, storedProgress, catalogResult] = await Promise.all([
        invoke<DownloadRecord[]>("list_downloads"),
        invoke<WatchProgress[]>("list_watch_progress"),
        catalogPromise,
      ]);
      const storedDownloads = Object.fromEntries(storedRecords.map((record) => [record.videoId, record]));
      const storedWatchProgress = Object.fromEntries(storedProgress.map((record) => [record.videoId, record]));
      setDownloads(storedDownloads);
      setWatchProgress(storedWatchProgress);
      setSelectedId((current) => current ?? Object.keys(storedDownloads)[0] ?? catalogResult.catalog?.[0]?.id ?? null);

      if (catalogResult.catalog) {
        setCatalog(catalogResult.catalog);
        setServerStatus("online");
      } else {
        setCatalog([]);
        setServerStatus("offline");
        setRemoteErrorMessage(`遠端影片資料載入失敗：${catalogResult.error ?? "服務無法使用"}`);
      }
      setLocalDataLoading(false);
    } catch (error) {
      setLocalDataError(getInvokeError(error, "本機資料載入失敗，請重新載入"));
      setLocalDataLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadLocalData();
  }, [loadLocalData]);

  useEffect(() => {
    let disposed = false;
    let activeSessionId: string | null = null;
    setPlaybackUrl(null);
    setPlaybackErrorMessage(null);

    if (!selectedDownload) {
      if (selectedVideo && serverStatus === "online") setPlaybackUrl(selectedVideo.sourceUrl);
      return () => {
        disposed = true;
      };
    }

    void invoke<PlaybackInfo>("open_playback", { videoId: selectedDownload.videoId })
      .then((playback) => {
        if (disposed) {
          void invoke("close_playback", { sessionId: playback.sessionId });
          return;
        }
        activeSessionId = playback.sessionId;
        setPlaybackUrl(playback.url);
      })
      .catch((error) => {
        if (!disposed) {
          setErrorMessage(getInvokeError(error, "無法開啟加密影片"));
        }
      });

    return () => {
      disposed = true;
      if (activeSessionId) {
        void invoke("close_playback", { sessionId: activeSessionId });
      }
    };
  }, [selectedDownload?.videoId, selectedVideo?.sourceUrl, serverStatus]);

  const refreshCatalog = useCallback(async () => {
    flushProgress();
    setServerStatus("checking");
    setErrorMessage(null);
    setRemoteErrorMessage(null);

    try {
      const healthResponse = await fetch(HEALTH_URL, { cache: "no-store" });
      if (!healthResponse.ok) throw new Error(`Health request failed: ${healthResponse.status}`);
      const response = await fetch(CATALOG_URL, { cache: "no-store" });
      if (!response.ok) throw new Error(`Catalog request failed: ${response.status}`);
      const remoteCatalog = (await response.json()) as VideoDefinition[];
      setCatalog(remoteCatalog);
      setServerStatus("online");
      setSelectedId((current) => current ?? remoteCatalog[0]?.id ?? null);
    } catch (error) {
      setCatalog([]);
      setServerStatus("offline");
      setRemoteErrorMessage(`遠端資料載入失敗：${error instanceof Error ? error.message : "服務無法使用"}`);
      setSelectedId((current) => (current && downloads[current] ? current : Object.keys(downloads)[0] ?? null));
    }
  }, [downloads, flushProgress]);

  useEffect(() => {
    const handleOnline = () => {
      void refreshCatalog();
    };
    const handleOffline = () => {
      setCatalog([]);
      setServerStatus("offline");
      setRemoteErrorMessage("網路連線中斷，已切換至離線模式。");
    };
    window.addEventListener("online", handleOnline);
    window.addEventListener("offline", handleOffline);
    return () => {
      window.removeEventListener("online", handleOnline);
      window.removeEventListener("offline", handleOffline);
    };
  }, [refreshCatalog]);

  async function syncWatchProgress() {
    const pending = Object.values(watchProgressRef.current).filter((record) => record.syncState === "pending");
    if (pending.length === 0) return;

    setSyncStatus("syncing");
    setSyncErrorMessage(null);
    const sent: WatchProgressSyncToken[] = pending.map(({ videoId, updatedAt }) => ({ videoId, updatedAt }));
    try {
      const response = await fetch(WATCH_PROGRESS_URL, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ version: 1, progress: pending }),
      });
      if (!response.ok) throw new Error(`Sync request failed: ${response.status}`);
      const payload = (await response.json()) as { version: number; progress: WatchProgress[] };
      if (payload.version !== 1 || !Array.isArray(payload.progress)) throw new Error("同步回應格式不正確");

      const sentByVideoId = new Map(sent.map((item) => [item.videoId, item.updatedAt]));
      const accepted = sent.filter((item) =>
        payload.progress.some((record) => record.videoId === item.videoId && record.updatedAt === item.updatedAt),
      );
      const conflicts = payload.progress.filter((record) =>
        sentByVideoId.has(record.videoId) && sentByVideoId.get(record.videoId) !== record.updatedAt,
      );
      if (accepted.length > 0) {
        await invoke("mark_watch_progress_synced", { items: accepted });
      }
      if (conflicts.length > 0) {
        await invoke("apply_remote_watch_progress", { request: { records: conflicts, sent } });
      }
      const latest = await invoke<WatchProgress[]>("list_watch_progress");
      setWatchProgress(Object.fromEntries(latest.map((record) => [record.videoId, record])));
      setSyncStatus("synced");
    } catch (error) {
      setSyncStatus("error");
      setSyncErrorMessage("觀看進度尚未同步，將於稍後重試。");
      if (syncRetryTimerRef.current) window.clearTimeout(syncRetryTimerRef.current);
      syncRetryTimerRef.current = window.setTimeout(() => void syncWatchProgress(), 10_000);
    }
  }

  useEffect(() => {
    if (!localDataLoading && serverStatus === "online") void syncWatchProgress();
  }, [localDataLoading, serverStatus]);

  function selectVideo(videoId: string) {
    prepareForSwitch();
    setSelectedId(videoId);
  }

  async function downloadVideo(video: VideoDefinition) {
    if (activeDownloadId) return;

    setActiveDownloadId(video.id);
    setDownloadProgress(0);
    setDownloadedBytes(0);
    setErrorMessage(null);

    const channel = new Channel<DownloadEvent>();
    channel.onmessage = (event) => {
      if (event.videoId !== video.id) return;
      if (event.type === "progress") {
        setDownloadedBytes(event.downloadedBytes);
        setDownloadProgress(event.percentage);
      }
      if (event.type === "failed") {
        setErrorMessage(`${event.error.code}: ${event.error.message}`);
      }
    };

    const request: DownloadRequest = {
      videoId: video.id,
      title: video.title,
      description: video.description,
      sizeBytes: video.sizeBytes,
      sourceUrl: video.sourceUrl,
    };

    try {
      const record = await invoke<DownloadRecord>("download_video", { request, onEvent: channel });
      setDownloads((current) => ({ ...current, [record.videoId]: record }));
      setSelectedId(record.videoId);
    } catch (error) {
      setErrorMessage(getInvokeError(error, "下載影片失敗"));
    } finally {
      setActiveDownloadId(null);
      setDownloadProgress(0);
      setDownloadedBytes(0);
    }
  }

  async function deleteDownload(video: VideoDefinition) {
    if (!downloads[video.id]) return;

    const confirmed = await confirm(`確定要刪除「${video.title}」的離線檔案嗎？`, {
      title: "刪除離線檔案",
      kind: "warning",
    });
    if (!confirmed) return;

    try {
      await invoke("delete_download", { videoId: video.id });
      setDownloads((current) => {
        const next = { ...current };
        delete next[video.id];
        return next;
      });
      setWatchProgress((current) => {
        const next = { ...current };
        delete next[video.id];
        return next;
      });
      delete progressQueueRef.current[video.id];
      if (selectedId === video.id) setSelectedId(null);
    } catch (error) {
      setErrorMessage(getInvokeError(error, "刪除離線檔案失敗"));
    }
  }

  if (localDataLoading) {
    return (
      <main className="app-shell state-screen" aria-live="polite">
        <div className="state-card">
          <span className="eyebrow accent-text">LOCAL ARCHIVE / LOADING</span>
          <h1>正在載入觀看進度</h1>
          <p>進度載入完成前，播放器會暫時鎖定。</p>
        </div>
      </main>
    );
  }

  if (localDataError) {
    return (
      <main className="app-shell state-screen" role="alert">
        <div className="state-card is-error">
          <span className="eyebrow">LOCAL ARCHIVE / ERROR</span>
          <h1>無法載入本機資料</h1>
          <p>{localDataError}</p>
          <button className="download-button" onClick={() => void loadLocalData()}>重新載入</button>
        </div>
      </main>
    );
  }

  const statusLabel = {
    checking: "檢查伺服器中",
    online: "伺服器在線",
    offline: "離線模式",
  }[serverStatus];

  return (
    <main className="app-shell">
      <header className="topbar">
        <div className="brand-lockup">
          <span className="brand-mark">O</span>
          <div>
            <p className="eyebrow">OFFLINE CINEMA / 01</p>
            <h1>片庫</h1>
          </div>
        </div>
        <div className="status-cluster">
          <span className={`status-dot status-${serverStatus}`} aria-hidden="true" />
          <span>{statusLabel}</span>
          <button className="refresh-button" onClick={() => void refreshCatalog()} disabled={serverStatus === "checking"}>
            重新整理
          </button>
        </div>
      </header>

      <section className="hero-grid">
        <div className="hero-copy">
          <p className="eyebrow accent-text">YOUR PERSONAL ARCHIVE</p>
          <h2>把想看的，<em>留在身邊。</em></h2>
          <p className="hero-description">
            連線時保存影片，斷線後繼續觀看。你的離線片庫會記住每一個已完成的下載。
          </p>
        </div>
        <div className="hero-note">
          <span className="note-index">NOTE 01</span>
          <p>已下載的檔案儲存在這台電腦的 App 專用資料夾，不會隨伺服器離線而消失。</p>
        </div>
      </section>

      {errorMessage && <div className="error-banner">{errorMessage}</div>}
      {remoteErrorMessage && (
        <div className="error-banner remote-error" role="alert">
          <span>{remoteErrorMessage} 已切換至離線模式。</span>
          <button className="refresh-button" onClick={() => void refreshCatalog()} disabled={serverStatus === "checking"}>
            重新連線
          </button>
        </div>
      )}
      {syncStatus === "error" && syncErrorMessage && (
        <div className="error-banner sync-error" role="status">
          <span>{syncErrorMessage}</span>
          <button className="refresh-button" onClick={() => void syncWatchProgress()}>立即重試</button>
        </div>
      )}

      <section className="content-grid">
        <aside className="library-panel" aria-label="影片清單">
          <div className="section-heading">
            <div>
              <p className="eyebrow">{serverStatus === "online" ? "AVAILABLE NOW" : "SAVED LOCALLY"}</p>
              <h3>{serverStatus === "online" ? "所有影片" : "已下載影片"}</h3>
            </div>
            <span className="count-badge">{visibleVideos.length.toString().padStart(2, "0")}</span>
          </div>

          <div className="video-list">
            {visibleVideos.map((video, index) => {
              const record = downloads[video.id];
              const progress = watchProgress[video.id];
              const isSelected = selectedId === video.id;
              const isDownloading = activeDownloadId === video.id;

              return (
                <button
                  className={`video-row ${isSelected ? "is-selected" : ""}`}
                  key={video.id}
                  onClick={() => selectVideo(video.id)}
                >
                  <span className="row-number">{String(index + 1).padStart(2, "0")}</span>
                  <span className="video-row-copy">
                    <strong>{video.title}</strong>
                    <small>
                      {record ? `已保存 · ${formatDate(record.downloadedAt)}` : formatBytes(video.sizeBytes)}
                    </small>
                  </span>
                  <span className={`row-state ${record ? "is-saved" : ""}`}>
                    {isDownloading
                      ? `${downloadProgress ?? 0}%`
                      : progress?.completed
                        ? "看完"
                        : progress
                          ? `${formatPlaybackTime(progress.positionSeconds)} / ${formatPlaybackTime(progress.durationSeconds)}`
                          : record
                            ? "離線"
                            : "未下載"}
                  </span>
                </button>
              );
            })}
            {visibleVideos.length === 0 && (
              <div className="empty-list">
                <span>—</span>
                <p>尚無離線影片</p>
                <small>啟動測試伺服器後即可載入片單。</small>
              </div>
            )}
          </div>
        </aside>

        <section className="player-panel" aria-label="影片播放器">
          {selectedVideo ? (
            <>
              <div className="player-frame">
                {playbackUrl ? (
                  <video
                    {...videoProps}
                    key={`${selectedVideo.id}:${playbackUrl}`}
                    controls
                    src={playbackUrl}
                    preload="metadata"
                    onError={() => setPlaybackErrorMessage("播放失敗：無法讀取離線影片，請查看日志或重新下載。")}
                  >
                    你的瀏覽器不支援影片播放。
                  </video>
                ) : (
                  <div className="player-placeholder">
                    <span className="play-glyph">▶</span>
                    <p>下載後即可離線播放</p>
                  </div>
                )}
                {resumePrompt?.videoId === selectedVideo.id && (
                  <div className="resume-prompt" role="dialog" aria-label="繼續播放">
                    <p>上次看到 {formatPlaybackTime(resumePrompt.positionSeconds)}</p>
                    <div className="resume-actions">
                      <button className="download-button" onClick={continuePlayback}>繼續播放</button>
                      <button className="refresh-button" onClick={restartPlayback}>從頭播放</button>
                    </div>
                  </div>
                )}
                {playbackErrorMessage && <div className="player-error" role="alert">{playbackErrorMessage}</div>}
                <span className="player-tape">ARCHIVE / {selectedVideo.id.toUpperCase()}</span>
              </div>

              <div className="player-meta">
                <div>
                  <p className="eyebrow">NOW SELECTED</p>
                  <h3>{selectedVideo.title}</h3>
                  <p className="video-description">{selectedVideo.description}</p>
                </div>
                <div className="action-stack">
                  {selectedDownload ? (
                    <button className="delete-button" onClick={() => void deleteDownload(selectedVideo)}>
                      刪除離線檔案
                    </button>
                  ) : (
                    <button
                      className="download-button"
                      onClick={() => void downloadVideo(selectedVideo)}
                      disabled={Boolean(activeDownloadId) || serverStatus !== "online"}
                    >
                      {activeDownloadId === selectedVideo.id
                        ? `下載中 ${downloadProgress ?? `${formatBytes(downloadedBytes)} 已下載`}`
                        : "下載到離線片庫"}
                    </button>
                  )}
                  <span className="file-detail">MP4 · {formatBytes(selectedVideo.sizeBytes)}</span>
                </div>
              </div>
            </>
          ) : (
            <div className="no-selection">
              <span className="large-index">00</span>
              <h3>選一支影片開始</h3>
              <p>你的片庫會在這裡播放。</p>
            </div>
          )}
        </section>
      </section>

      <footer className="footer-bar">
        <span>LOCAL STORAGE / APP DATA</span>
        <span>{Object.keys(downloads).length} 部影片已保存</span>
      </footer>
    </main>
  );
}

export default App;
