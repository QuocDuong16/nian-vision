import { useCallback, useEffect, useRef } from "react";
import { desktopError, invokeDesktop, isTauri } from "../lib/tauri";
import type { DesktopError, PtzCapabilities, PtzDirection, PtzMovement } from "../lib/tauri";

type OwnedMovement = {
  localGeneration: number;
  backend: PtzMovement;
};

export function PtzControls({
  cameraId,
  capabilities,
  error,
  onError,
}: {
  cameraId: string;
  capabilities: PtzCapabilities | null;
  error: DesktopError | null;
  onError: (error: DesktopError | null) => void;
}) {
  const mountedRef = useRef(true);
  const localGenerationRef = useRef(0);
  const desiredGenerationRef = useRef<number | null>(null);
  const ownedRef = useRef<OwnedMovement | null>(null);
  const renewTimerRef = useRef<number | null>(null);

  const clearRenewTimer = useCallback(() => {
    if (renewTimerRef.current !== null) {
      window.clearTimeout(renewTimerRef.current);
      renewTimerRef.current = null;
    }
  }, []);

  const stopBackend = useCallback((movement: PtzMovement) => {
    if (!isTauri()) return;
    void invokeDesktop<void>("ptz_stop", {
      input: { camera_id: cameraId, generation: movement.generation },
    }).catch(() => undefined);
  }, [cameraId]);

  const release = useCallback(() => {
    localGenerationRef.current += 1;
    desiredGenerationRef.current = null;
    clearRenewTimer();
    const owned = ownedRef.current;
    ownedRef.current = null;
    if (owned) stopBackend(owned.backend);
  }, [clearRenewTimer, stopBackend]);

  const scheduleRenew = useCallback((owned: OwnedMovement) => {
    clearRenewTimer();
    const delay = Math.max(100, owned.backend.renew_after_ms);
    renewTimerRef.current = window.setTimeout(() => {
      if (
        !mountedRef.current ||
        desiredGenerationRef.current !== owned.localGeneration ||
        ownedRef.current?.backend.generation !== owned.backend.generation
      ) return;
      void invokeDesktop<void>("ptz_renew", {
        input: { camera_id: cameraId, generation: owned.backend.generation },
      })
        .then(() => {
          if (
            mountedRef.current &&
            desiredGenerationRef.current === owned.localGeneration &&
            ownedRef.current?.backend.generation === owned.backend.generation
          ) scheduleRenew(owned);
        })
        .catch((cause) => {
          if (ownedRef.current?.backend.generation === owned.backend.generation) {
            ownedRef.current = null;
            desiredGenerationRef.current = null;
            clearRenewTimer();
            if (mountedRef.current) onError(desktopError(cause));
          }
        });
    }, delay);
  }, [cameraId, clearRenewTimer, onError]);

  const begin = useCallback(async (direction: PtzDirection) => {
    if (!isTauri() || !capabilities?.ptz_supported) return;
    if ((direction === "zoom_in" || direction === "zoom_out") && !capabilities.zoom_supported) return;

    release();
    const localGeneration = localGenerationRef.current + 1;
    localGenerationRef.current = localGeneration;
    desiredGenerationRef.current = localGeneration;
    onError(null);
    try {
      const backend = await invokeDesktop<PtzMovement>("ptz_move", {
        input: { camera_id: cameraId, direction },
      });
      if (!mountedRef.current || desiredGenerationRef.current !== localGeneration) {
        stopBackend(backend);
        return;
      }
      const owned = { localGeneration, backend };
      ownedRef.current = owned;
      scheduleRenew(owned);
    } catch (cause) {
      if (mountedRef.current && desiredGenerationRef.current === localGeneration) {
        desiredGenerationRef.current = null;
        onError(desktopError(cause));
      }
    }
  }, [cameraId, capabilities, onError, release, scheduleRenew, stopBackend]);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      release();
    };
  }, [release]);

  if (!capabilities) {
    return <div className="ptz-status muted">PTZ: checking…</div>;
  }
  if (!capabilities.configured) {
    return <div className="ptz-status muted">PTZ: not configured</div>;
  }
  if (!capabilities.pan_tilt_supported) {
    return <div className="ptz-status muted">PTZ: unsupported</div>;
  }

  const button = (label: string, direction: PtzDirection, className?: string) => (
    <button
      type="button"
      className={className}
      aria-label={`PTZ ${label}`}
      onPointerDown={(event) => {
        event.preventDefault();
        event.currentTarget.setPointerCapture?.(event.pointerId);
        void begin(direction);
      }}
      onPointerUp={release}
      onPointerCancel={release}
      onPointerLeave={release}
      onLostPointerCapture={release}
      onKeyDown={(event) => {
        if ((event.key === " " || event.key === "Enter") && !event.repeat) {
          event.preventDefault();
          void begin(direction);
        }
      }}
      onKeyUp={(event) => {
        if (event.key === " " || event.key === "Enter") {
          event.preventDefault();
          release();
        }
      }}
      onBlur={release}
      onClick={(event) => event.preventDefault()}
    >
      {label}
    </button>
  );

  return (
    <div className="ptz-panel" aria-label={`PTZ controls for ${cameraId}`}>
      <div className="ptz-pad">
        <span />
        {button("Up", "up")}
        <span />
        {button("Left", "left")}
        <span className="ptz-center" aria-hidden="true">●</span>
        {button("Right", "right")}
        <span />
        {button("Down", "down")}
        <span />
      </div>
      {capabilities.zoom_supported && (
        <div className="ptz-zoom">
          {button("Zoom out", "zoom_out")}
          {button("Zoom in", "zoom_in")}
        </div>
      )}
      {error ? <div className="ptz-error" role="alert">PTZ: {error.message}</div> : (
        <div className="ptz-status muted">PTZ: {capabilities.state ?? "ready"}</div>
      )}
    </div>
  );
}
