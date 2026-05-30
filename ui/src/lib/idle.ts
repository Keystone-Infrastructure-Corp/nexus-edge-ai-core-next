// v0.1.36 — client-side idle nudge.
//
// Server is authoritative: the engine refresh handler rejects with
// 401 + body `{"code":"idle_expired"}` once 20 minutes have elapsed
// since the chain's `last_active_at`. This hook is the client-side
// companion — it watches for input events on the document, resets a
// local timer, and proactively triggers a logout (without waiting
// for the next refresh round-trip) once the idle window elapses.
//
// We intentionally use the same threshold as the server (20 min)
// minus a 30 s safety margin so the client clears state slightly
// before the next refresh would have been refused, keeping the UX
// consistent ("you've been signed out") instead of surfacing as a
// silent failed request.
import { useEffect, useRef } from "react";

const IDLE_MS = 20 * 60 * 1000 - 30 * 1000;
const ACTIVITY_EVENTS = [
  "mousedown",
  "keydown",
  "touchstart",
  "scroll",
  "visibilitychange",
] as const;

export function useIdleLogout(enabled: boolean, onIdle: () => void): void {
  const onIdleRef = useRef(onIdle);
  onIdleRef.current = onIdle;

  useEffect(() => {
    if (!enabled) return;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const reset = () => {
      if (timer !== null) clearTimeout(timer);
      timer = setTimeout(() => onIdleRef.current(), IDLE_MS);
    };
    for (const ev of ACTIVITY_EVENTS) {
      window.addEventListener(ev, reset, { passive: true });
    }
    reset();
    return () => {
      if (timer !== null) clearTimeout(timer);
      for (const ev of ACTIVITY_EVENTS) {
        window.removeEventListener(ev, reset);
      }
    };
  }, [enabled]);
}
