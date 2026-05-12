// Typed EventSource wrapper. Auto-reconnects with exponential backoff and
// hands every payload to the caller already cast to T (callers should still
// validate at runtime if they don't trust the engine).

export interface SseHandle {
  close(): void;
}

export function subscribeSse<T>(
  path: string,
  onMessage: (msg: T) => void,
  onError?: (err: Event) => void,
): SseHandle {
  let es: EventSource | null = null;
  let backoff = 1000;
  let closed = false;

  const open = () => {
    if (closed) return;
    es = new EventSource(path);
    es.onmessage = (ev) => {
      backoff = 1000;
      try {
        onMessage(JSON.parse(ev.data) as T);
      } catch {
        // Drop malformed payload; the engine should never send one.
      }
    };
    es.onerror = (ev) => {
      onError?.(ev);
      es?.close();
      es = null;
      if (!closed) {
        const delay = Math.min(backoff, 30_000);
        backoff *= 2;
        setTimeout(open, delay);
      }
    };
  };
  open();

  return {
    close() {
      closed = true;
      es?.close();
      es = null;
    },
  };
}
