import { useEffect, useRef } from "react";
import { listen, type EventCallback } from "@tauri-apps/api/event";

/**
 * Subscribe to a Tauri event for the lifetime of the component.
 *
 * Handles two failure modes the naive `listen().then(un => un())` pattern
 * has in dev:
 *   - Cleanup fires before `listen()` resolves (StrictMode double-mount,
 *     fast re-renders): we defer the unlisten until the promise settles.
 *   - `un()` throws because Tauri's internal listener map was cleared out
 *     from under us (Vite HMR module re-evaluation): we swallow it — the
 *     listener is effectively gone either way.
 *
 * The handler is captured in a ref so a new reference each render doesn't
 * re-subscribe.
 */
export function useTauriEvent<T>(
  event: string,
  handler: EventCallback<T>,
): void {
  const handlerRef = useRef(handler);
  handlerRef.current = handler;

  useEffect(() => {
    let disposed = false;
    let un: (() => void) | null = null;

    listen<T>(event, (e) => handlerRef.current(e))
      .then((fn) => {
        if (disposed) {
          try {
            fn();
          } catch {}
        } else {
          un = fn;
        }
      })
      .catch(() => {});

    return () => {
      disposed = true;
      try {
        un?.();
      } catch {}
      un = null;
    };
  }, [event]);
}
