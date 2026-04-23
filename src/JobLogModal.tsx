import { useEffect, useRef, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";

import type { JobEvent } from "./types";

interface Props {
  title: string;
  command: string;
  /**
   * Arguments object for `invoke(command, args)`. The modal automatically
   * adds an `onEvent` channel to this object — callers should not pre-set it.
   */
  args: Record<string, unknown>;
  onClose: () => void;
  onSuccess?: (result: unknown) => void;
}

type JobState = "running" | "success" | "failed";

export function JobLogModal({
  title,
  command,
  args,
  onClose,
  onSuccess,
}: Props) {
  const [events, setEvents] = useState<JobEvent[]>([]);
  const [state, setState] = useState<JobState>("running");
  const startedRef = useRef(false);
  const logRef = useRef<HTMLDivElement | null>(null);
  const resultRef = useRef<unknown>(null);

  useEffect(() => {
    // StrictMode double-invokes useEffect in dev — guard so we don't start
    // the job twice.
    if (startedRef.current) return;
    startedRef.current = true;

    const channel = new Channel<JobEvent>();
    channel.onmessage = (event) => {
      setEvents((prev) => [...prev, event]);
      if (event.kind === "success") setState("success");
      else if (event.kind === "failed") setState("failed");
    };

    invoke(command, { ...args, onEvent: channel })
      .then((res) => {
        resultRef.current = res;
        setState((s) => (s === "running" ? "success" : s));
      })
      .catch((e) => {
        // If backend didn't already emit Failed, synthesize one so the log
        // pane shows the error inline.
        setEvents((prev) => {
          const last = prev[prev.length - 1];
          if (last?.kind === "failed") return prev;
          return [...prev, { kind: "failed", error: String(e) }];
        });
        setState((s) => (s === "running" ? "failed" : s));
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Auto-scroll to bottom as events arrive.
  useEffect(() => {
    const el = logRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [events]);

  const handleClose = () => {
    if (state === "success") onSuccess?.(resultRef.current);
    onClose();
  };

  return (
    <div className="modal-backdrop">
      <div
        className="modal job-log-modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
      >
        <header className="job-log-header">
          <h3>{title}</h3>
          <span className={`job-state job-state-${state}`}>
            {state === "running" ? "running…" : state}
          </span>
        </header>
        <div className="job-log-lines" ref={logRef}>
          {events.length === 0 && (
            <div className="job-log-line status muted">
              Starting…
            </div>
          )}
          {events.map((e, i) => (
            <JobLogLine key={i} event={e} />
          ))}
        </div>
        <div className="modal-actions">
          <button
            type="button"
            className={state === "success" ? "primary" : ""}
            onClick={handleClose}
            disabled={state === "running"}
          >
            {state === "running"
              ? "Running…"
              : state === "success"
                ? "Done"
                : "Close"}
          </button>
        </div>
      </div>
    </div>
  );
}

function JobLogLine({ event }: { event: JobEvent }) {
  switch (event.kind) {
    case "status":
      return (
        <div className="job-log-line status">
          {event.repo && <span className="repo-tag">{event.repo}</span>}
          <span className="status-text">{event.message}</span>
        </div>
      );
    case "log":
      return (
        <div className={`job-log-line log log-${event.stream}`}>
          {event.repo && <span className="repo-tag">{event.repo}</span>}
          <span className="log-text">{event.line}</span>
        </div>
      );
    case "success":
      return <div className="job-log-line status ok">✔ done</div>;
    case "failed":
      return (
        <div className="job-log-line status err">✘ {event.error}</div>
      );
  }
}
