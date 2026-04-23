import { useEffect, useRef } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { ClipboardAddon } from "@xterm/addon-clipboard";
import "@xterm/xterm/css/xterm.css";
import { themeToXterm, useTheme } from "./theme";

const DEFAULT_XTERM_THEME = {
  background: "#0a0a0a",
  foreground: "#e8e8e8",
};

interface Props {
  sessionId: string;
}

/**
 * xterm.js surface for a Tethys PTY session. On mount:
 *   1. Create terminal + canvas/fit/clipboard addons.
 *   2. Create a raw-bytes `Channel`.
 *   3. Call `attach_session` — returns historical scrollback, registers the
 *      channel for live fan-out.
 *   4. Write scrollback into xterm, then drain the channel straight into it.
 * Keystrokes go via `send_input`, resize events via `resize_session`.
 */
export function SessionTerminal({ sessionId }: Props) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const theme = useTheme();
  // Snapshot the current theme for the mount-time init so the main useEffect
  // doesn't need `theme` as a dep (which would rebuild xterm on every change).
  const themeRef = useRef(theme);
  themeRef.current = theme;

  useEffect(() => {
    if (!termRef.current) return;
    const next = theme ? themeToXterm(theme) : DEFAULT_XTERM_THEME;
    termRef.current.options.theme = next;
  }, [theme]);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    const initialTheme = themeRef.current
      ? themeToXterm(themeRef.current)
      : DEFAULT_XTERM_THEME;
    const term = new Terminal({
      fontFamily: '"SF Mono", ui-monospace, Menlo, monospace',
      fontSize: 13,
      theme: initialTheme,
      cursorBlink: true,
      scrollback: 10000,
      allowProposedApi: true,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.loadAddon(new ClipboardAddon());
    // Using xterm's default DOM renderer — @xterm/addon-canvas reaches into
    // v5 internals that v6 removed (`_linkifier2`), and WebGL + WKWebView
    // has known issues on macOS. DOM is plenty fast for interactive shells.
    term.open(container);
    fit.fit();

    termRef.current = term;
    fitRef.current = fit;

    // Keystrokes → backend.
    const dataSub = term.onData((data) => {
      const bytes = Array.from(new TextEncoder().encode(data));
      invoke("send_input", { sessionId, data: bytes }).catch((e) => {
        console.error("send_input failed:", e);
      });
    });

    // Resize → backend.
    const resizeSub = term.onResize(({ cols, rows }) => {
      invoke("resize_session", { sessionId, cols, rows }).catch((e) => {
        console.error("resize_session failed:", e);
      });
    });

    // Attach: get scrollback + start streaming.
    const channel = new Channel<ArrayBuffer>();
    channel.onmessage = (chunk) => {
      term.write(new Uint8Array(chunk));
    };

    let cancelled = false;
    invoke<number[]>("attach_session", { sessionId, onBytes: channel })
      .then((scrollback) => {
        if (cancelled) return;
        if (scrollback.length > 0) {
          term.write(new Uint8Array(scrollback));
        }
        // Final resize to let the backend match xterm's cols/rows right away.
        const { cols, rows } = term;
        invoke("resize_session", { sessionId, cols, rows }).catch(() => {});
      })
      .catch((e) => {
        term.write(`\r\n\x1b[31m[attach failed: ${String(e)}]\x1b[0m\r\n`);
      });

    // Fit on container resize.
    const ro = new ResizeObserver(() => {
      try {
        fit.fit();
      } catch {
        // xterm throws if the container has zero size (e.g. during transition)
      }
    });
    ro.observe(container);

    return () => {
      cancelled = true;
      ro.disconnect();
      dataSub.dispose();
      resizeSub.dispose();
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
      // Channel has no explicit close; dropping references is enough. The
      // backend detects the dead channel on its next send and removes the
      // subscriber.
    };
  }, [sessionId]);

  return <div className="session-terminal" ref={containerRef} />;
}
