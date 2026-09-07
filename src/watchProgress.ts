import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type RefObject,
  type ReactEventHandler,
} from "react";

export type SyncState = "pending" | "synced";

export type WatchProgress = {
  videoId: string;
  positionSeconds: number;
  durationSeconds: number;
  completed: boolean;
  updatedAt: string;
  syncState: SyncState;
  lastSyncedAt: string | null;
};

export type SaveWatchProgressRequest = Pick<
  WatchProgress,
  "videoId" | "positionSeconds" | "durationSeconds" | "completed"
>;

export type ProgressSaveReason = "record" | "flush";

type ResumePrompt = { videoId: string; positionSeconds: number };

type WatchProgressOptions = {
  videoId: string | null;
  sourceKey: string | null;
  enabled: boolean;
  persistedProgress: WatchProgress | null;
  onSnapshot: (snapshot: SaveWatchProgressRequest, reason: ProgressSaveReason) => void;
};

type WatchProgressVideoProps = {
  ref: RefObject<HTMLVideoElement | null>;
  onLoadedMetadata: ReactEventHandler<HTMLVideoElement>;
  onPlay: ReactEventHandler<HTMLVideoElement>;
  onTimeUpdate: ReactEventHandler<HTMLVideoElement>;
  onPause: ReactEventHandler<HTMLVideoElement>;
  onSeeking: ReactEventHandler<HTMLVideoElement>;
  onSeeked: ReactEventHandler<HTMLVideoElement>;
  onEnded: ReactEventHandler<HTMLVideoElement>;
};

function buildProgressSnapshot(videoId: string, element: HTMLVideoElement): SaveWatchProgressRequest | null {
  if (!Number.isFinite(element.duration) || element.duration <= 0 || !Number.isFinite(element.currentTime)) {
    return null;
  }
  return {
    videoId,
    positionSeconds: element.ended ? element.duration : Math.min(element.currentTime, element.duration),
    durationSeconds: element.duration,
    completed: element.ended,
  };
}

export function useWatchProgress({
  videoId,
  sourceKey,
  enabled,
  persistedProgress,
  onSnapshot,
}: WatchProgressOptions) {
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const instanceSequenceRef = useRef(0);
  const instanceKeyRef = useRef<string | null>(null);
  const activeInstanceIdRef = useRef(0);
  const transitioningRef = useRef(false);
  const restoringPositionRef = useRef(false);
  const restoreTimeoutRef = useRef<number | null>(null);
  const hasStartedRef = useRef(false);
  const userSeekToZeroRef = useRef(false);
  const userSeekingRef = useRef(false);
  const lastRecordAtRef = useRef(0);
  const persistedProgressRef = useRef<WatchProgress | null>(persistedProgress);
  const completedRef = useRef(persistedProgress?.completed ?? false);
  const resumePromptRef = useRef<ResumePrompt | null>(null);
  const [resumePrompt, setResumePrompt] = useState<ResumePrompt | null>(null);

  const instanceKey = `${videoId ?? "none"}:${sourceKey ?? "none"}`;
  if (instanceKeyRef.current !== instanceKey) {
    instanceKeyRef.current = instanceKey;
    instanceSequenceRef.current += 1;
    transitioningRef.current = false;
    restoringPositionRef.current = false;
    hasStartedRef.current = false;
    userSeekToZeroRef.current = false;
    userSeekingRef.current = false;
    lastRecordAtRef.current = 0;
    completedRef.current = persistedProgress?.completed ?? false;
  }
  const instanceId = instanceSequenceRef.current;
  activeInstanceIdRef.current = instanceId;

  useEffect(() => {
    persistedProgressRef.current = persistedProgress;
    completedRef.current = persistedProgress?.completed ?? false;
  }, [persistedProgress]);

  useEffect(() => () => {
    if (restoreTimeoutRef.current) window.clearTimeout(restoreTimeoutRef.current);
  }, []);

  const setPrompt = useCallback((next: ResumePrompt | null) => {
    resumePromptRef.current = next;
    setResumePrompt(next);
  }, []);

  const isCurrentEvent = useCallback((event: React.SyntheticEvent<HTMLVideoElement>) => (
    enabled &&
    event.currentTarget === videoRef.current &&
    activeInstanceIdRef.current === instanceId &&
    !transitioningRef.current
  ), [enabled, instanceId]);

  const clearRestoreFlag = useCallback(() => {
    restoringPositionRef.current = false;
    if (restoreTimeoutRef.current) {
      window.clearTimeout(restoreTimeoutRef.current);
      restoreTimeoutRef.current = null;
    }
  }, []);

  const restorePosition = useCallback((element: HTMLVideoElement, positionSeconds: number) => {
    clearRestoreFlag();
    restoringPositionRef.current = true;
    element.currentTime = positionSeconds;
    restoreTimeoutRef.current = window.setTimeout(clearRestoreFlag, 1_000);
  }, [clearRestoreFlag]);

  const emitSnapshot = useCallback((element: HTMLVideoElement, reason: ProgressSaveReason, allowZero = false) => {
    if (
      !enabled ||
      transitioningRef.current ||
      restoringPositionRef.current ||
      !hasStartedRef.current ||
      (completedRef.current && !element.ended && !allowZero) ||
      (!allowZero && element.currentTime <= 0 && !element.ended)
    ) {
      return;
    }
    const snapshot = videoId ? buildProgressSnapshot(videoId, element) : null;
    if (snapshot) onSnapshot(snapshot, reason);
  }, [enabled, onSnapshot, videoId]);

  const flushProgress = useCallback(() => {
    const element = videoRef.current;
    if (!element || activeInstanceIdRef.current !== instanceId) return;
    emitSnapshot(element, "flush", userSeekToZeroRef.current);
  }, [emitSnapshot, instanceId]);

  const prepareForSwitch = useCallback(() => {
    flushProgress();
    transitioningRef.current = true;
    activeInstanceIdRef.current = -1;
  }, [flushProgress]);

  const continuePlayback = useCallback(() => {
    const element = videoRef.current;
    const prompt = resumePromptRef.current;
    if (!element || !prompt || prompt.videoId !== videoId) return;
    setPrompt(null);
    hasStartedRef.current = true;
    restorePosition(element, prompt.positionSeconds);
    void element.play().catch(() => undefined);
  }, [restorePosition, setPrompt, videoId]);

  const restartPlayback = useCallback(() => {
    const element = videoRef.current;
    if (!element || !videoId || activeInstanceIdRef.current !== instanceId) return;
    setPrompt(null);
    clearRestoreFlag();
    hasStartedRef.current = true;
    completedRef.current = false;
    userSeekToZeroRef.current = true;
    element.currentTime = 0;
    emitSnapshot(element, "record", true);
    void element.play().catch(() => undefined);
  }, [clearRestoreFlag, emitSnapshot, instanceId, setPrompt, videoId]);

  const videoProps = useMemo<WatchProgressVideoProps>(() => ({
    ref: videoRef,
    onLoadedMetadata: (event) => {
      if (!isCurrentEvent(event)) return;
      const progress = persistedProgressRef.current;
      const element = event.currentTarget;
      if (
        progress &&
        !progress.completed &&
        progress.positionSeconds > 5 &&
        progress.positionSeconds < element.duration - 1
      ) {
        setPrompt({ videoId: progress.videoId, positionSeconds: progress.positionSeconds });
      } else {
        setPrompt(null);
      }
    },
    onPlay: (event) => {
      if (!isCurrentEvent(event)) return;
      hasStartedRef.current = true;
      const prompt = resumePromptRef.current;
      if (prompt && prompt.videoId === videoId) {
        setPrompt(null);
        restorePosition(event.currentTarget, prompt.positionSeconds);
      }
    },
    onTimeUpdate: (event) => {
      if (!isCurrentEvent(event) || event.currentTarget.paused) return;
      const now = Date.now();
      if (now - lastRecordAtRef.current < 4_000) return;
      lastRecordAtRef.current = now;
      emitSnapshot(event.currentTarget, "record");
    },
    onPause: (event) => {
      if (!isCurrentEvent(event)) return;
      emitSnapshot(event.currentTarget, "record", userSeekToZeroRef.current);
    },
    onSeeking: (event) => {
      if (!isCurrentEvent(event) || restoringPositionRef.current) return;
      userSeekingRef.current = true;
    },
    onSeeked: (event) => {
      if (!isCurrentEvent(event)) return;
      if (restoringPositionRef.current) {
        clearRestoreFlag();
        return;
      }
      if (userSeekingRef.current) {
        userSeekingRef.current = false;
        hasStartedRef.current = true;
        userSeekToZeroRef.current = event.currentTarget.currentTime <= 0;
        emitSnapshot(event.currentTarget, "record", userSeekToZeroRef.current);
      }
    },
    onEnded: (event) => {
      if (!isCurrentEvent(event)) return;
      hasStartedRef.current = true;
      completedRef.current = true;
      userSeekToZeroRef.current = false;
      emitSnapshot(event.currentTarget, "record");
    },
  }), [clearRestoreFlag, emitSnapshot, isCurrentEvent, restorePosition, setPrompt, videoId]);

  useEffect(() => {
    const saveWhenHidden = () => {
      if (document.visibilityState === "hidden") flushProgress();
    };
    document.addEventListener("visibilitychange", saveWhenHidden);
    return () => document.removeEventListener("visibilitychange", saveWhenHidden);
  }, [flushProgress]);

  useEffect(() => () => {
    if (activeInstanceIdRef.current === instanceId) flushProgress();
  }, [flushProgress, instanceId]);

  return {
    videoRef,
    videoProps,
    resumePrompt,
    continuePlayback,
    restartPlayback,
    flushProgress,
    prepareForSwitch,
  };
}
