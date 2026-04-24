import { useEffect, useRef, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";

import type { JobEvent } from "./types";

interface Props {
  title: string;
  command: string;
  /**
   * Arguments object for `invoke(command, args)`. An `onEvent` channel is
   * added automatically — callers should not pre-set it.
   */
  args: Record<string, unknown>;
  /** Fired once the backend resolves successfully. */
  onSuccess?: (result: unknown) => void;
  /** User-initiated dismiss — only enabled after the job settles. */
  onDismiss: () => void;
}

type JobState = "running" | "success" | "failed";

/**
 * Inline pane for a streaming backend job (create / delete workspace). Renders
 * the same log stream as the old JobLogModal but without the modal chrome so
 * it can live inside a workspace detail pane or in place of one.
 */
export function JobLogPane({
  title,
  command,
  args,
  onSuccess,
  onDismiss,
}: Props) {
  const [events, setEvents] = useState<JobEvent[]>([]);
  const [state, setState] = useState<JobState>("running");
  const startedRef = useRef(false);
  const logRef = useRef<HTMLDivElement | null>(null);
  const resultRef = useRef<unknown>(null);
  const onSuccessRef = useRef(onSuccess);
  onSuccessRef.current = onSuccess;

  useEffect(() => {
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
        onSuccessRef.current?.(res);
      })
      .catch((e) => {
        setEvents((prev) => {
          const last = prev[prev.length - 1];
          if (last?.kind === "failed") return prev;
          return [...prev, { kind: "failed", error: String(e) }];
        });
        setState((s) => (s === "running" ? "failed" : s));
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const el = logRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [events]);

  return (
    <div className="job-log-pane">
      <header className="job-log-header">
        <h3>{title}</h3>
        <span className={`job-state job-state-${state}`}>
          {state === "running" ? "running…" : state}
        </span>
      </header>
      <div className="job-log-lines" ref={logRef}>
        {events.length === 0 && (
          <div className="job-log-line status muted">Starting…</div>
        )}
        {events.map((e, i) => (
          <JobLogLine key={i} event={e} />
        ))}
      </div>
      <div className="job-log-actions">
        <button
          type="button"
          onClick={onDismiss}
          disabled={state === "running"}
          autoFocus={state !== "running"}
        >
          {state === "running"
            ? "Running…"
            : state === "success"
              ? "Done"
              : "Close"}
        </button>
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
      return <div className="job-log-line status err">✘ {event.error}</div>;
  }
}
