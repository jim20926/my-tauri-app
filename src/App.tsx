import { useEffect, useMemo, useState } from "react";
import { Channel, convertFileSrc, invoke } from "@tauri-apps/api/core";
import { confirm } from "@tauri-apps/plugin-dialog";
import "./App.css";

const CATALOG_URL = "http://localhost:3001/videos.json";

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
  localPath: string;
  downloadedAt: string;
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
type ServerStatus = "checking" | "online" | "offline";

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

function App() {
  const [catalog, setCatalog] = useState<VideoDefinition[]>([]);
  const [downloads, setDownloads] = useState<DownloadMap>({});
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [serverStatus, setServerStatus] = useState<ServerStatus>("checking");
  const [activeDownloadId, setActiveDownloadId] = useState<string | null>(null);
  const [downloadProgress, setDownloadProgress] = useState<number | null>(0);
  const [downloadedBytes, setDownloadedBytes] = useState(0);
  const [errorMessage, setErrorMessage] = useState<string | null>(null);

  const visibleVideos = useMemo(() => {
    if (serverStatus === "online") return catalog;
    return Object.values(downloads).map(recordToVideo);
  }, [catalog, downloads, serverStatus]);

  const selectedVideo = visibleVideos.find((video) => video.id === selectedId) ?? null;
  const selectedDownload = selectedVideo ? downloads[selectedVideo.id] : undefined;
  const playbackUrl = selectedDownload
    ? convertFileSrc(selectedDownload.localPath)
    : selectedVideo && serverStatus === "online"
      ? selectedVideo.sourceUrl
      : null;

  useEffect(() => {
    let cancelled = false;

    async function initialize() {
      const downloadsPromise = invoke<DownloadRecord[]>("list_downloads").catch(() => []);
      const catalogPromise = fetch(CATALOG_URL)
        .then(async (response) => {
          if (!response.ok) throw new Error(`Catalog request failed: ${response.status}`);
          return (await response.json()) as VideoDefinition[];
        })
        .catch(() => null);
      const [storedRecords, remoteCatalog] = await Promise.all([downloadsPromise, catalogPromise]);
      if (cancelled) return;

      const storedDownloads = Object.fromEntries(storedRecords.map((record) => [record.videoId, record]));
      setDownloads(storedDownloads);
      setSelectedId(Object.keys(storedDownloads)[0] ?? null);

      if (remoteCatalog) {
        setCatalog(remoteCatalog);
        setServerStatus("online");
        setSelectedId((current) => current ?? remoteCatalog[0]?.id ?? null);
      } else {
        setServerStatus("offline");
      }
    }

    void initialize();
    return () => {
      cancelled = true;
    };
  }, []);

  async function refreshCatalog() {
    setServerStatus("checking");
    setErrorMessage(null);

    try {
      const response = await fetch(CATALOG_URL, { cache: "no-store" });
      if (!response.ok) throw new Error(`Catalog request failed: ${response.status}`);
      const remoteCatalog = (await response.json()) as VideoDefinition[];
      setCatalog(remoteCatalog);
      setServerStatus("online");
      setSelectedId((current) => current ?? remoteCatalog[0]?.id ?? null);
    } catch {
      setCatalog([]);
      setServerStatus("offline");
      setSelectedId((current) => (current && downloads[current] ? current : Object.keys(downloads)[0] ?? null));
    }
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
      if (selectedId === video.id) setSelectedId(null);
    } catch (error) {
      setErrorMessage(getInvokeError(error, "刪除離線檔案失敗"));
    }
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
              const isSelected = selectedId === video.id;
              const isDownloading = activeDownloadId === video.id;

              return (
                <button
                  className={`video-row ${isSelected ? "is-selected" : ""}`}
                  key={video.id}
                  onClick={() => setSelectedId(video.id)}
                >
                  <span className="row-number">{String(index + 1).padStart(2, "0")}</span>
                  <span className="video-row-copy">
                    <strong>{video.title}</strong>
                    <small>
                      {record ? `已保存 · ${formatDate(record.downloadedAt)}` : formatBytes(video.sizeBytes)}
                    </small>
                  </span>
                  <span className={`row-state ${record ? "is-saved" : ""}`}>
                    {isDownloading ? `${downloadProgress ?? 0}%` : record ? "離線" : "未下載"}
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
                  <video key={playbackUrl} controls src={playbackUrl} preload="metadata">
                    你的瀏覽器不支援影片播放。
                  </video>
                ) : (
                  <div className="player-placeholder">
                    <span className="play-glyph">▶</span>
                    <p>下載後即可離線播放</p>
                  </div>
                )}
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
