import { useEffect, useRef } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { ClipboardAddon } from "@xterm/addon-clipboard";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { openUrl } from "@tauri-apps/plugin-opener";
import "@xterm/xterm/css/xterm.css";
import { themeToXterm, useTheme } from "./theme";

const DEFAULT_XTERM_THEME = {
  background: "#0a0a0a",
  foreground: "#e8e8e8",
};

/**
 * Backslash-escape spaces in a filesystem path. Matches iTerm2's drop
 * format inside a bracketed paste — Claude Code unescapes `\ ` and resolves
 * the path, which triggers the `[Image #N]` attachment flow for images.
 */
function escapeDroppedPath(p: string): string {
  return p.replace(/([\\ ])/g, "\\$1");
}

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
      // xterm.js owns scrollback end-to-end. Tmux is only a process
      // keeper (mouse off, no copy-mode) — wheel events fall through to
      // xterm.js and scroll its buffer natively. Claude writes to the
      // main buffer, so the stream naturally populates this scrollback.
      scrollback: 50000,
      allowProposedApi: true,
      // OSC 8 escape-sequence hyperlinks (Claude Code emits these for PR
      // URLs etc.) default to `window.open` which Tauri's WKWebView blocks
      // and routes through dialog.confirm. Route through plugin-opener.
      linkHandler: {
        activate: (_ev, uri) => {
          openUrl(uri).catch((e) => {
            console.error("openUrl failed:", e);
          });
        },
      },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.loadAddon(new ClipboardAddon());
    // Ctrl/Cmd+Click a URL → open in the default browser. We route through
    // plugin-opener so WKWebView doesn't try to intercept navigation and
    // prompt via `dialog.confirm` (which isn't in our capability set).
    term.loadAddon(
      new WebLinksAddon((event, uri) => {
        event.preventDefault();
        openUrl(uri).catch((e) => {
          console.error("openUrl failed:", e);
        });
      }),
    );
    // Using xterm's default DOM renderer — @xterm/addon-canvas reaches into
    // v5 internals that v6 removed (`_linkifier2`), and WebGL + WKWebView
    // has known issues on macOS. DOM is plenty fast for interactive shells.
    term.open(container);
    fit.fit();
    term.focus();

    // macOS-friendly keybindings over and above xterm's defaults.
    // Returning false suppresses xterm's default dispatch for that key;
    // we send our own byte sequence via send_input.
    const sendRaw = (bytes: number[]) => {
      invoke("send_input", { sessionId, data: bytes }).catch((e) => {
        console.error("send_input (keybind) failed:", e);
      });
    };
    term.attachCustomKeyEventHandler((ev) => {
      if (ev.type !== "keydown") return true;

      // Shift+Enter → newline (Option+Enter equivalent in Claude Code).
      if (
        ev.key === "Enter" &&
        ev.shiftKey &&
        !ev.metaKey &&
        !ev.altKey &&
        !ev.ctrlKey
      ) {
        ev.preventDefault();
        sendRaw([0x1b, 0x0d]);
        return false;
      }

      // Cmd+Delete (mac "Delete" = Backspace) → delete previous word.
      if (ev.key === "Backspace" && ev.metaKey && !ev.altKey && !ev.ctrlKey) {
        ev.preventDefault();
        sendRaw([0x17]); // Ctrl-W / readline backward-kill-word
        return false;
      }

      // Cmd+Left → previous word (Alt-b).
      if (ev.key === "ArrowLeft" && ev.metaKey && !ev.altKey && !ev.ctrlKey) {
        ev.preventDefault();
        sendRaw([0x1b, 0x62]);
        return false;
      }

      // Cmd+Right → next word (Alt-f).
      if (ev.key === "ArrowRight" && ev.metaKey && !ev.altKey && !ev.ctrlKey) {
        ev.preventDefault();
        sendRaw([0x1b, 0x66]);
        return false;
      }

      return true;
    });

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

    // Drag files from Finder onto the window → paste escaped paths into the
    // active session, like iTerm2. Wrapped in bracketed-paste markers
    // (`\x1b[200~…\x1b[201~`) so Claude Code recognizes it as a paste and
    // runs its path-→-image attachment flow, producing `[Image #N]` for
    // images. The event is window-wide; only one SessionTerminal mounts at
    // a time, so no per-pane gating is needed.
    let dragUnlisten: (() => void) | null = null;
    let dragDisposed = false;
    getCurrentWebview()
      .onDragDropEvent((event) => {
        if (event.payload.type !== "drop") return;
        if (event.payload.paths.length === 0) return;
        const inner =
          event.payload.paths.map(escapeDroppedPath).join(" ") + " ";
        const text = `\x1b[200~${inner}\x1b[201~`;
        const bytes = Array.from(new TextEncoder().encode(text));
        invoke("send_input", { sessionId, data: bytes }).catch((e) => {
          console.error("send_input (drag-drop) failed:", e);
        });
        term.focus();
      })
      .then((fn) => {
        if (dragDisposed) {
          try {
            fn();
          } catch {}
        } else {
          dragUnlisten = fn;
        }
      })
      .catch((e) => {
        console.error("onDragDropEvent subscribe failed:", e);
      });

    return () => {
      cancelled = true;
      ro.disconnect();
      dataSub.dispose();
      resizeSub.dispose();
      dragDisposed = true;
      try {
        dragUnlisten?.();
      } catch {}
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
