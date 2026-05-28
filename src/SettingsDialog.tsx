import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

import type { AppSettings, IdeChoice } from "./types";

type IdeKind = IdeChoice["kind"];

/**
 * App-wide settings modal. Currently just picks the IDE that the workspace
 * "Open in IDE" button launches. Loads the persisted settings on mount and
 * writes them back via `set_settings` on save.
 */
export function SettingsDialog({ onClose }: { onClose: () => void }) {
  const [kind, setKind] = useState<IdeKind>("cursor");
  const [customApp, setCustomApp] = useState("");
  const [loaded, setLoaded] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    invoke<AppSettings>("get_settings")
      .then((s) => {
        setKind(s.ide.kind);
        if (s.ide.kind === "custom") setCustomApp(s.ide.app);
        setLoaded(true);
      })
      .catch((e) => {
        setError(String(e));
        setLoaded(true);
      });
  }, []);

  const save = async () => {
    let ide: IdeChoice;
    if (kind === "custom") {
      const app = customApp.trim();
      if (!app) {
        setError("Enter an app name or path for the custom IDE.");
        return;
      }
      ide = { kind: "custom", app };
    } else if (kind === "vs_code") {
      ide = { kind: "vs_code" };
    } else {
      ide = { kind: "cursor" };
    }
    setSaving(true);
    setError(null);
    try {
      await invoke("set_settings", { settings: { ide } });
      onClose();
    } catch (e) {
      setError(String(e));
      setSaving(false);
    }
  };

  return (
    <div className="modal-backdrop" onClick={saving ? undefined : onClose}>
      <div
        className="modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
      >
        <h3>Settings</h3>
        <div className="settings-section">
          <div className="settings-label">IDE for "Open in IDE"</div>
          <ul className="ide-choices">
            <li>
              <label className="repo-row">
                <input
                  type="radio"
                  name="ide"
                  checked={kind === "cursor"}
                  onChange={() => setKind("cursor")}
                  disabled={!loaded}
                />
                <span className="repo-display">Cursor</span>
              </label>
            </li>
            <li>
              <label className="repo-row">
                <input
                  type="radio"
                  name="ide"
                  checked={kind === "vs_code"}
                  onChange={() => setKind("vs_code")}
                  disabled={!loaded}
                />
                <span className="repo-display">VS Code</span>
              </label>
            </li>
            <li>
              <label className="repo-row">
                <input
                  type="radio"
                  name="ide"
                  checked={kind === "custom"}
                  onChange={() => setKind("custom")}
                  disabled={!loaded}
                />
                <span className="repo-display">Custom</span>
              </label>
              {kind === "custom" && (
                <input
                  type="text"
                  className="settings-custom-app"
                  placeholder="App name or /path/to/App.app"
                  value={customApp}
                  onChange={(e) => setCustomApp(e.target.value)}
                  autoFocus
                />
              )}
            </li>
          </ul>
          <p className="muted">
            Launched via macOS <code>open -a</code>. For Custom, enter an
            application name (e.g. <code>Zed</code>) or a path to a{" "}
            <code>.app</code> bundle.
          </p>
        </div>
        {error && <div className="error-banner">{error}</div>}
        <div className="modal-actions">
          <button type="button" onClick={onClose} disabled={saving}>
            Cancel
          </button>
          <button
            type="button"
            className="primary"
            onClick={save}
            disabled={!loaded || saving}
          >
            {saving ? "Saving…" : "Save"}
          </button>
        </div>
      </div>
    </div>
  );
}
