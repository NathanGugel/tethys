import { useCallback, useEffect, useRef, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { ClipboardAddon } from "@xterm/addon-clipboard";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { SearchAddon, type ISearchOptions } from "@xterm/addon-search";
import { openUrl } from "@tauri-apps/plugin-opener";
import "@xterm/xterm/css/xterm.css";
import { themeToXterm, useTheme } from "./theme";

const DEFAULT_XTERM_THEME = {
  background: "#0a0a0a",
  foreground: "#e8e8e8",
};

// Highlight colors for search hits. Yellow match / orange active-match reads
// on both light and dark themes, matching the iTerm2 find convention. Passed
// to the search addon, which paints overlay decorations over the buffer and
// ticks on the scroll overview ruler.
const SEARCH_OPTIONS: ISearchOptions = {
  decorations: {
    matchBackground: "#9e7b00",
    matchBorder: "transparent",
    matchOverviewRuler: "#9e7b00",
    activeMatchBackground: "#ff8c00",
    activeMatchBorder: "transparent",
    activeMatchColorOverviewRuler: "#ff8c00",
  },
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
  /** When true, the terminal is view-only: keystrokes are not forwarded
   *  to the PTY, the paste handler is disabled, and the custom keybind
   *  handler returns control to xterm (which itself doesn't echo input
   *  when `disableStdin` is set). The user can still scroll, copy, and
   *  search. Used for dev-server output panes (yarn dev / docker compose
   *  up) where input would risk killing the running process. */
  readOnly?: boolean;
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
export function SessionTerminal({ sessionId, readOnly = false }: Props) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const searchRef = useRef<SearchAddon | null>(null);
  const searchInputRef = useRef<HTMLInputElement | null>(null);
  // Find-bar state. `query` is mirrored into a ref so the (mount-time, never
  // re-registered) xterm key handler can read the live value for Cmd+G
  // next/prev without being a hook dependency.
  const [searchOpen, setSearchOpen] = useState(false);
  const [query, setQuery] = useState("");
  const queryRef = useRef(query);
  queryRef.current = query;
  const [results, setResults] = useState({ resultIndex: -1, resultCount: 0 });
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
      cursorBlink: !readOnly,
      disableStdin: readOnly,
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
    // Cmd+F search across the full 50k-line scrollback (xterm owns the
    // buffer, so this is entirely client-side — no backend round-trip).
    const search = new SearchAddon();
    term.loadAddon(search);
    searchRef.current = search;
    const resultsSub = search.onDidChangeResults((r) => setResults(r));
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

    // Cmd+V of a file from Finder/screenshot: WKWebView delivers only an
    // opaque `File` (no `text/plain`, no `text/uri-list`) and then quietly
    // auto-inserts the temp path into the helper textarea after the paste
    // event. xterm wraps that text in bracketed-paste markers, which trips
    // Claude Code's path-→-image flow indiscriminately — turning a pasted
    // log path into `[Image #N]`.
    //
    // For image MIME we want that flow (it's the whole point of pasting a
    // screenshot). For everything else we want iTerm2-style behavior: the
    // path appears as plain typed text. Branch on file MIME, intercept the
    // non-image case, read real paths from NSPasteboard via Rust, and inject
    // raw bytes without bracketed-paste markers.
    const helperTextarea = container.querySelector<HTMLTextAreaElement>(
      ".xterm-helper-textarea",
    );
    const onPaste = (ev: ClipboardEvent) => {
      const cd = ev.clipboardData;
      if (!cd || cd.files.length === 0) return;
      const allImages = Array.from(cd.files).every((f) =>
        f.type.startsWith("image/"),
      );
      if (allImages) return;
      ev.preventDefault();
      ev.stopImmediatePropagation();
      invoke<string[]>("read_clipboard_file_paths")
        .then((paths) => {
          if (paths.length === 0) return;
          const text = paths.map(escapeDroppedPath).join(" ") + " ";
          const bytes = Array.from(new TextEncoder().encode(text));
          return invoke("send_input", { sessionId, data: bytes });
        })
        .catch((e) => {
          console.error("file paste failed:", e);
        });
    };
    if (!readOnly) {
      helperTextarea?.addEventListener("paste", onPaste, true);
    }

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

      // Cmd+F → open the find bar (suppress WKWebView's own page find).
      if (ev.key === "f" && ev.metaKey && !ev.altKey && !ev.ctrlKey) {
        ev.preventDefault();
        setSearchOpen(true);
        // Defer focus/select to after the overlay mounts.
        queueMicrotask(() => searchInputRef.current?.select());
        return false;
      }

      // Cmd+G / Cmd+Shift+G → next / previous match (iTerm2 muscle memory).
      // Reads the live query from the ref since this handler is registered
      // once and never sees re-rendered state.
      if (ev.key === "g" && ev.metaKey && !ev.altKey && !ev.ctrlKey) {
        ev.preventDefault();
        const q = queryRef.current;
        if (q) {
          if (ev.shiftKey) search.findPrevious(q, SEARCH_OPTIONS);
          else search.findNext(q, SEARCH_OPTIONS);
        }
        return false;
      }

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

      // macOS line/word editing. Convention: Cmd = whole line, Alt = word.
      // Each row maps a (key + modifier) pair to a readline byte sequence
      // that the underlying shell / Claude Code / TUI app understands.
      const onlyCmd = ev.metaKey && !ev.altKey && !ev.ctrlKey && !ev.shiftKey;
      const onlyAlt = ev.altKey && !ev.metaKey && !ev.ctrlKey && !ev.shiftKey;
      type EditBind = { key: string; mod: "cmd" | "alt"; bytes: number[] };
      const edits: EditBind[] = [
        // Cmd → line operations.
        { key: "ArrowLeft", mod: "cmd", bytes: [0x01] }, // Ctrl-A: beginning of line
        { key: "ArrowRight", mod: "cmd", bytes: [0x05] }, // Ctrl-E: end of line
        { key: "Backspace", mod: "cmd", bytes: [0x15] }, // Ctrl-U: kill to start of line
        { key: "Delete", mod: "cmd", bytes: [0x0b] }, // Ctrl-K: kill to end of line
        // Alt → word operations (the bindings Cmd used to do).
        { key: "ArrowLeft", mod: "alt", bytes: [0x1b, 0x62] }, // Esc-b: previous word
        { key: "ArrowRight", mod: "alt", bytes: [0x1b, 0x66] }, // Esc-f: next word
        { key: "Backspace", mod: "alt", bytes: [0x17] }, // Ctrl-W: backward-kill-word
        { key: "Delete", mod: "alt", bytes: [0x1b, 0x64] }, // Esc-d: kill-word forward
      ];
      for (const { key, mod, bytes } of edits) {
        if (ev.key !== key) continue;
        if (mod === "cmd" && !onlyCmd) continue;
        if (mod === "alt" && !onlyAlt) continue;
        ev.preventDefault();
        sendRaw(bytes);
        return false;
      }

      return true;
    });

    termRef.current = term;
    fitRef.current = fit;

    // Keystrokes → backend. Skipped in read-only mode so dev-server
    // panes don't accidentally forward input to yarn dev / docker compose.
    const dataSub = readOnly
      ? null
      : term.onData((data) => {
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
      dataSub?.dispose();
      resizeSub.dispose();
      resultsSub.dispose();
      searchRef.current = null;
      dragDisposed = true;
      try {
        dragUnlisten?.();
      } catch {}
      helperTextarea?.removeEventListener("paste", onPaste, true);
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
      // Channel has no explicit close; dropping references is enough. The
      // backend detects the dead channel on its next send and removes the
      // subscriber.
    };
    // readOnly is intentionally part of the deps so toggling it
    // (rare — basically only for hot-reload while developing) rebuilds
    // the terminal with the right options.
  }, [sessionId, readOnly]);

  // Incremental find: re-search from the current position as the query
  // changes so highlights and the match counter track each keystroke.
  useEffect(() => {
    if (!searchOpen) return;
    searchRef.current?.findNext(query, {
      ...SEARCH_OPTIONS,
      incremental: true,
    });
  }, [query, searchOpen]);

  const findNext = useCallback(() => {
    searchRef.current?.findNext(queryRef.current, SEARCH_OPTIONS);
  }, []);
  const findPrevious = useCallback(() => {
    searchRef.current?.findPrevious(queryRef.current, SEARCH_OPTIONS);
  }, []);
  const closeSearch = useCallback(() => {
    setSearchOpen(false);
    searchRef.current?.clearDecorations();
    termRef.current?.focus();
  }, []);

  const matchLabel = query
    ? results.resultCount === 0
      ? "No results"
      : `${results.resultIndex >= 0 ? results.resultIndex + 1 : 0}/${results.resultCount}`
    : "";

  return (
    <div className="session-terminal">
      <div className="session-terminal-surface" ref={containerRef} />
      {searchOpen && (
        <div className="terminal-find" role="search">
          <input
            ref={searchInputRef}
            className="terminal-find-input"
            type="text"
            placeholder="Find"
            value={query}
            autoFocus
            spellCheck={false}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                if (e.shiftKey) findPrevious();
                else findNext();
              } else if (e.key === "Escape") {
                e.preventDefault();
                closeSearch();
              } else if (e.key === "f" && e.metaKey) {
                // Cmd+F while the bar is open re-selects rather than
                // letting WKWebView open its own find.
                e.preventDefault();
                searchInputRef.current?.select();
              }
            }}
          />
          <span className="terminal-find-count">{matchLabel}</span>
          <button
            className="terminal-find-btn"
            title="Previous match (Shift+Enter)"
            onClick={findPrevious}
            disabled={!query}
          >
            ↑
          </button>
          <button
            className="terminal-find-btn"
            title="Next match (Enter)"
            onClick={findNext}
            disabled={!query}
          >
            ↓
          </button>
          <button
            className="terminal-find-btn"
            title="Close (Esc)"
            onClick={closeSearch}
          >
            ✕
          </button>
        </div>
      )}
    </div>
  );
}
