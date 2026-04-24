import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type {
  CreateWorkspaceArgs,
  Discrepancies,
  GithubStatusChangedEvent,
  RegistryStatus,
  Repo,
  SessionInfo,
  SessionRuntimeState,
  Theme,
  TurnChangedEvent,
  Workspace,
  WorkspaceId,
} from "./types";
import { GithubAuthFooter } from "./GithubAuthFooter";
import { GithubChip } from "./GithubChip";
import { JobLogPane } from "./JobLogPane";
import { SessionTerminal } from "./SessionTerminal";
import { applyTheme, ThemeContext } from "./theme";
import { useTauriEvent } from "./useTauriEvent";
import { isReadyToArchive } from "./workspaceDerived";
import "./App.css";

type PendingCreate = {
  tempId: string;
  branch: string;
  args: CreateWorkspaceArgs;
};

type PendingDelete = {
  workspaceId: WorkspaceId;
  branch: string;
};

function App() {
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);
  const [registry, setRegistry] = useState<RegistryStatus | null>(null);
  const [discrepancies, setDiscrepancies] = useState<Discrepancies | null>(null);
  const [selectedId, setSelectedId] = useState<WorkspaceId | null>(null);
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [pendingCreate, setPendingCreate] = useState<PendingCreate | null>(null);
  const [pendingDelete, setPendingDelete] = useState<PendingDelete | null>(null);
  /**
   * Per-session turn state tracked by listening to `session:turn_changed`
   * globally. Used for the sidebar attention dot without needing to
   * fetch sessions for every workspace.
   */
  const [turnStates, setTurnStates] = useState<
    Map<string, { workspaceId: string; state: SessionRuntimeState }>
  >(new Map());
  /**
   * Sessions for every workspace, cached so switching into a workspace
   * shows the terminal immediately instead of flashing "Dormant" during
   * the list_sessions round-trip. Populated eagerly on workspace load
   * and kept in sync via session:* events.
   */
  const [sessionsByWorkspace, setSessionsByWorkspace] = useState<
    Map<WorkspaceId, SessionInfo[]>
  >(new Map());
  const [theme, setTheme] = useState<Theme | null>(null);

  useEffect(() => {
    invoke<Theme | null>("get_theme")
      .then((t) => {
        setTheme(t);
        applyTheme(t);
      })
      .catch((e) => console.error("get_theme failed:", e));
  }, []);

  useTauriEvent<Theme | null>("theme:changed", (event) => {
    const t = event.payload ?? null;
    setTheme(t);
    applyTheme(t);
  });

  useTauriEvent<TurnChangedEvent>("session:turn_changed", (event) => {
    const { workspace_id, session_id, runtime_state, notification_type } =
      event.payload;
    setTurnStates((prev) => {
      const next = new Map(prev);
      if (runtime_state === "dormant") {
        next.delete(session_id);
      } else {
        next.set(session_id, {
          workspaceId: workspace_id,
          state: runtime_state,
        });
      }
      return next;
    });
    // Keep the cached SessionInfo[] in sync so WorkspaceDetail sees the
    // new runtime_state without a full re-fetch.
    setSessionsByWorkspace((prev) => {
      const list = prev.get(workspace_id);
      if (!list) return prev;
      const next = new Map(prev);
      next.set(
        workspace_id,
        list.map((s) =>
          s.id === session_id
            ? {
                ...s,
                runtime_state,
                notification_type: notification_type ?? null,
              }
            : s,
        ),
      );
      return next;
    });
  });

  useTauriEvent<GithubStatusChangedEvent>("github:status_changed", (event) => {
    const { workspace_id, repo_key, status } = event.payload;
    setWorkspaces((prev) =>
      prev.map((w) => {
        if (w.id !== workspace_id) return w;
        return {
          ...w,
          repo_links: w.repo_links.map((r) =>
            r.repo_key === repo_key ? { ...r, github: status } : r,
          ),
        };
      }),
    );
  });

  const workspaceNeedsTurn = useCallback(
    (w: Workspace): boolean => {
      if (w.paused) return false;
      for (const info of turnStates.values()) {
        if (info.workspaceId !== w.id) continue;
        if (info.state === "idle" || info.state === "waiting_input") return true;
      }
      return false;
    },
    [turnStates],
  );

  const refreshSessionsFor = useCallback(async (workspaceId: WorkspaceId) => {
    try {
      const list = await invoke<SessionInfo[]>("list_sessions", {
        workspaceId,
      });
      setSessionsByWorkspace((prev) => {
        const next = new Map(prev);
        next.set(workspaceId, list);
        return next;
      });
    } catch (e) {
      console.error("list_sessions:", e);
    }
  }, []);

  const refresh = useCallback(async () => {
    try {
      const [list, reg, disc] = await Promise.all([
        invoke<Workspace[]>("list_workspaces"),
        invoke<RegistryStatus>("registry_status"),
        invoke<Discrepancies>("list_discrepancies"),
      ]);
      setWorkspaces(list);
      setRegistry(reg);
      setDiscrepancies(disc);
      setError(null);
      // Pre-load sessions for every workspace so switching in doesn't
      // render a stale/empty sessions list.
      await Promise.all(list.map((w) => refreshSessionsFor(w.id)));
    } catch (e) {
      setError(String(e));
    }
  }, [refreshSessionsFor]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  useTauriEvent("workspace:changed", () => refresh());
  useTauriEvent<{ workspace_id: string }>("session:changed", (event) => {
    refreshSessionsFor(event.payload.workspace_id);
  });
  useTauriEvent<{ workspace_id: string }>("session:exit", (event) => {
    refreshSessionsFor(event.payload.workspace_id);
  });

  const selected = useMemo(
    () => workspaces.find((w) => w.id === selectedId) ?? null,
    [workspaces, selectedId],
  );

  const registryOk = registry?.kind === "ok";

  return (
    <ThemeContext.Provider value={theme}>
    <div className="app">
      <aside className="sidebar">
        <div className="sidebar-header">
          <button
            className="primary"
            onClick={() => setCreating(true)}
            type="button"
            disabled={!registryOk || pendingCreate !== null}
            title={
              !registryOk
                ? "Configure repos.toml first"
                : pendingCreate
                  ? "Another workspace is being created"
                  : undefined
            }
          >
            New workspace
          </button>
        </div>
        <ul className="workspace-list">
          {workspaces.length === 0 && !pendingCreate && (
            <li className="empty">No workspaces yet.</li>
          )}
          {pendingCreate && (
            <li
              key={pendingCreate.tempId}
              className={`pending${
                pendingCreate.tempId === selectedId ? " selected" : ""
              }`}
              onClick={() => setSelectedId(pendingCreate.tempId)}
            >
              <div className="workspace-name">
                <Spinner />
                {pendingCreate.branch}
              </div>
              <div className="pending-label">creating…</div>
            </li>
          )}
          {workspaces.map((w) => {
            const deleting = pendingDelete?.workspaceId === w.id;
            const classes = [
              w.id === selectedId ? "selected" : "",
              deleting ? "pending" : "",
              w.paused && !deleting ? "is-paused" : "",
            ]
              .filter(Boolean)
              .join(" ");
            return (
              <li
                key={w.id}
                className={classes}
                onClick={() => setSelectedId(w.id)}
              >
                <div className="workspace-name">
                  {deleting ? (
                    <Spinner />
                  ) : (
                    workspaceNeedsTurn(w) && (
                      <span
                        className="turn-dot"
                        title="Your turn"
                        aria-label="your turn"
                      />
                    )
                  )}
                  {w.branch}
                </div>
                {deleting ? (
                  <div className="pending-label">deleting…</div>
                ) : (
                  w.repo_links.length > 0 && (
                    <ul className="workspace-repo-list">
                      {w.repo_links.map((r) => (
                        <li key={r.repo_key}>
                          <span className="repo-key">{r.repo_key}</span>
                          {r.github && (
                            <GithubChip status={r.github} linkable={false} />
                          )}
                        </li>
                      ))}
                    </ul>
                  )
                )}
              </li>
            );
          })}
        </ul>
        <GithubAuthFooter />
      </aside>

      <main className="detail">
        {error && <div className="error-banner">{error}</div>}
        {registry && !registryOk && (
          <RegistryNotice registry={registry} onChanged={refresh} />
        )}
        {discrepancies &&
          (discrepancies.orphaned_dirs.length > 0 ||
            discrepancies.missing_worktrees.length > 0) && (
            <DiscrepancyNotice
              discrepancies={discrepancies}
              onChanged={refresh}
            />
          )}
        {pendingCreate && selectedId === pendingCreate.tempId ? (
          <JobLogPane
            title={`Creating ${pendingCreate.branch}`}
            command="create_workspace"
            args={{ args: pendingCreate.args }}
            onSuccess={async (result) => {
              const ws = result as Workspace;
              // Refresh before swapping selection so WorkspaceDetail has
              // the workspace available when the pending pane unmounts.
              await refresh();
              setSelectedId(ws.id);
              setPendingCreate(null);
            }}
            onDismiss={() => {
              setPendingCreate(null);
              setSelectedId(null);
            }}
          />
        ) : pendingDelete &&
          selected &&
          selected.id === pendingDelete.workspaceId ? (
          <JobLogPane
            title={`Deleting ${pendingDelete.branch}`}
            command="delete_workspace"
            args={{ id: pendingDelete.workspaceId }}
            onSuccess={() => {
              setPendingDelete(null);
              setSelectedId(null);
              refresh();
            }}
            onDismiss={() => {
              setPendingDelete(null);
              refresh();
            }}
          />
        ) : selected ? (
          <WorkspaceDetail
            workspace={selected}
            sessions={sessionsByWorkspace.get(selected.id) ?? []}
            onRequestDelete={() =>
              setPendingDelete({
                workspaceId: selected.id,
                branch: selected.branch,
              })
            }
          />
        ) : (
          registryOk && (
            <div className="placeholder">
              Select a workspace, or create one to get started.
            </div>
          )
        )}
      </main>

      {creating && registry?.kind === "ok" && (
        <CreateWorkspaceDialog
          repos={registry.registry.repos}
          onClose={() => setCreating(false)}
          onSubmit={(args) => {
            setCreating(false);
            const tempId = `pending:${Date.now()}`;
            setPendingCreate({ tempId, branch: args.branch, args });
            setSelectedId(tempId);
          }}
        />
      )}
    </div>
    </ThemeContext.Provider>
  );
}

function RegistryNotice({
  registry,
  onChanged,
}: {
  registry: RegistryStatus;
  onChanged: () => void;
}) {
  const openConfig = async () => {
    try {
      await invoke("open_repos_config");
    } catch (e) {
      alert(String(e));
    }
  };

  if (registry.kind === "ok") return null;

  return (
    <div className="registry-notice">
      <h2>Repos not configured</h2>
      {registry.kind === "missing" ? (
        <p>
          Tethys expects a repo registry at <code>{registry.path}</code>. It
          doesn't exist yet.
        </p>
      ) : (
        <>
          <p>
            Tethys couldn't load <code>{registry.path}</code>:
          </p>
          <pre>{registry.error}</pre>
        </>
      )}
      <p>
        Click the button below to open it in your default editor. Fill in{" "}
        <code>worktree_root</code> and at least one <code>[[repo]]</code>, then{" "}
        <strong>restart Tethys</strong> — registry changes take effect at
        launch.
      </p>
      <div className="actions">
        <button className="primary" type="button" onClick={openConfig}>
          Open repos.toml
        </button>
        <button type="button" onClick={onChanged}>
          Re-check
        </button>
      </div>
    </div>
  );
}

function DiscrepancyNotice({
  discrepancies,
  onChanged,
}: {
  discrepancies: Discrepancies;
  onChanged: () => void;
}) {
  const [busy, setBusy] = useState<Set<string>>(() => new Set());

  const withBusy = async (key: string, fn: () => Promise<void>) => {
    setBusy((prev) => new Set(prev).add(key));
    try {
      await fn();
    } catch (e) {
      alert(String(e));
    } finally {
      setBusy((prev) => {
        const next = new Set(prev);
        next.delete(key);
        return next;
      });
    }
  };

  const removeOrphan = (path: string) =>
    withBusy(`orphan:${path}`, async () => {
      await invoke("remove_orphan_dir", { path });
      onChanged();
    });

  const forgetWorkspace = (id: string) =>
    withBusy(`forget:${id}`, async () => {
      await invoke("forget_workspace", { id });
      onChanged();
    });

  // Collapse missing-worktree rows by workspace_id — multiple repos per
  // workspace would otherwise show redundant Forget buttons.
  const missingByWorkspace = new Map<
    string,
    { branch: string; repos: string[] }
  >();
  for (const m of discrepancies.missing_worktrees) {
    const entry = missingByWorkspace.get(m.workspace_id) ?? {
      branch: m.branch,
      repos: [],
    };
    entry.repos.push(m.repo_key);
    missingByWorkspace.set(m.workspace_id, entry);
  }

  return (
    <div className="discrepancy-notice">
      <h2>State / disk mismatch</h2>
      <p>
        Tethys found things that don't line up between <code>state.json</code>{" "}
        and your <code>worktree_root</code>. Usually the result of a crash or
        manual filesystem surgery.
      </p>

      {discrepancies.orphaned_dirs.length > 0 && (
        <>
          <h3>Orphaned worktrees</h3>
          <p className="muted">
            Directories with no matching workspace in state. Safe to remove.
          </p>
          <ul className="discrepancy-list">
            {discrepancies.orphaned_dirs.map((o) => {
              const working = busy.has(`orphan:${o.path}`);
              return (
                <li key={o.path}>
                  <code className="discrepancy-path">{o.path}</code>
                  <button
                    type="button"
                    className="danger"
                    onClick={() => removeOrphan(o.path)}
                    disabled={working}
                  >
                    {working ? (
                      <>
                        <Spinner /> Removing…
                      </>
                    ) : (
                      "Remove"
                    )}
                  </button>
                </li>
              );
            })}
          </ul>
        </>
      )}

      {missingByWorkspace.size > 0 && (
        <>
          <h3>Missing worktrees</h3>
          <p className="muted">
            Workspaces in state whose worktree directories have vanished. Forget
            drops the workspace from state; it does not touch disk.
          </p>
          <ul className="discrepancy-list">
            {Array.from(missingByWorkspace.entries()).map(
              ([id, { branch, repos }]) => {
                const working = busy.has(`forget:${id}`);
                return (
                  <li key={id}>
                    <div className="discrepancy-meta">
                      <code>{branch}</code>{" "}
                      <span className="muted">
                        · missing: {repos.join(", ")}
                      </span>
                    </div>
                    <button
                      type="button"
                      className="danger"
                      onClick={() => forgetWorkspace(id)}
                      disabled={working}
                    >
                      {working ? (
                        <>
                          <Spinner /> Forgetting…
                        </>
                      ) : (
                        "Forget"
                      )}
                    </button>
                  </li>
                );
              },
            )}
          </ul>
        </>
      )}
    </div>
  );
}

function WorkspaceDetail({
  workspace,
  sessions,
  onRequestDelete,
}: {
  workspace: Workspace;
  sessions: SessionInfo[];
  onRequestDelete: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const [showInfo, setShowInfo] = useState(false);
  const [selectedSessionId, setSelectedSessionId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Meta ids we've already auto-resumed this app-run — guards against
  // retry loops if spawn fails, while still allowing a manual Resume
  // click to try again.
  const autoResumedRef = useRef<Set<string>>(new Set());

  // Reset selection when the selected workspace changes.
  useEffect(() => {
    setSelectedSessionId(null);
  }, [workspace.id]);

  const togglePause = async () => {
    setBusy(true);
    try {
      await invoke(workspace.paused ? "resume_workspace" : "pause_workspace", {
        id: workspace.id,
      });
    } finally {
      setBusy(false);
    }
  };

  const performDelete = () => {
    setConfirmingDelete(false);
    onRequestDelete();
  };

  const liveById = new Map(sessions.map((s) => [s.id, s]));
  // Newest first — the most recently started session is usually what you
  // want to see. `workspace.sessions` is append-ordered on the backend.
  const ordered = [...workspace.sessions].reverse();

  // Effective selection: user's explicit pick if still valid; otherwise the
  // first live session, else the newest entry.
  const effectiveSelected = (() => {
    if (selectedSessionId && ordered.some((m) => m.id === selectedSessionId))
      return selectedSessionId;
    const firstLive = ordered.find((m) => liveById.has(m.id));
    return firstLive?.id ?? ordered[0]?.id ?? null;
  })();

  const selected = effectiveSelected
    ? ordered.find((m) => m.id === effectiveSelected) ?? null
    : null;
  const selectedLive = selected ? liveById.get(selected.id) ?? null : null;

  const startInRepo = async (repoKey: string) => {
    setBusy(true);
    setError(null);
    try {
      const res = await invoke<SessionInfo>("start_claude_session", {
        args: { workspace_id: workspace.id, repo_key: repoKey },
      });
      setSelectedSessionId(res.id);
      // App-level listener on `session:changed` refreshes the cache.
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const resumeMeta = async (metaId: string, repoKey: string) => {
    setBusy(true);
    setError(null);
    try {
      const res = await invoke<SessionInfo>("resume_claude_session", {
        args: {
          workspace_id: workspace.id,
          repo_key: repoKey,
          session_meta_id: metaId,
        },
      });
      setSelectedSessionId(res.id);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  // Auto-resume the selected session when it's dormant but has a
  // claude_session_id. `autoResumedRef` prevents a retry loop if the
  // spawn fails — the user can still click Resume manually below.
  useEffect(() => {
    if (!selected || selectedLive) return;
    if (!selected.claude_session_id) return;
    if (autoResumedRef.current.has(selected.id)) return;
    autoResumedRef.current.add(selected.id);
    void resumeMeta(selected.id, selected.repo_key);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selected?.id, selectedLive?.id, selected?.claude_session_id]);

  return (
    <div className="workspace-detail">
      <header>
        <h2>
          <code>{workspace.branch}</code>
          {workspace.repo_links.map(
            (r) => r.github && <GithubChip key={r.repo_key} status={r.github} />,
          )}
        </h2>
        <div className="actions">
          <button type="button" onClick={() => setShowInfo(true)}>
            Info
          </button>
          <button type="button" onClick={togglePause} disabled={busy}>
            {workspace.paused ? "Resume" : "Pause"}
          </button>
          <button
            type="button"
            className="danger"
            onClick={() => setConfirmingDelete(true)}
            disabled={busy}
          >
            Delete
          </button>
        </div>
      </header>
      {confirmingDelete && (
        <div className="inline-confirm" role="alert">
          <div className="inline-confirm-message">
            Delete workspace <code>{workspace.branch}</code>? This removes
            every worktree <strong>including any uncommitted changes</strong>{" "}
            and clears the workspace from Tethys state.
          </div>
          <div className="inline-confirm-actions">
            <button
              type="button"
              onClick={() => setConfirmingDelete(false)}
              autoFocus
            >
              Cancel
            </button>
            <button
              type="button"
              className="danger primary"
              onClick={performDelete}
            >
              Delete
            </button>
          </div>
        </div>
      )}
      {!confirmingDelete && isReadyToArchive(workspace) && (
        <div className="archive-banner">
          <div>
            <strong>Ready to archive.</strong>{" "}
            <span className="muted">
              Every linked PR for <code>{workspace.branch}</code> is merged.
            </span>
          </div>
          <button
            type="button"
            className="primary"
            onClick={() => setConfirmingDelete(true)}
          >
            Delete workspace
          </button>
        </div>
      )}
      {showInfo && (
        <WorkspaceInfoDialog
          workspace={workspace}
          onClose={() => setShowInfo(false)}
        />
      )}

      <div className="session-pane">
        <SessionBar
          sessions={ordered}
          liveById={liveById}
          repos={workspace.repo_links}
          selectedId={effectiveSelected}
          onSelect={setSelectedSessionId}
          onStartInRepo={startInRepo}
          busy={busy}
        />
        {error && <div className="error-banner">{error}</div>}
        {selected ? (
          selectedLive ? (
            <>
              {!selectedLive.running && (
                <div className="session-exit-banner">
                  Claude exited. Scrollback preserved below.
                </div>
              )}
              <SessionTerminal sessionId={selectedLive.id} />
            </>
          ) : (
            <div className="session-dormant">
              <p>
                This Claude session is dormant. Resume re-opens the
                conversation with <code>claude --resume</code>.
              </p>
              {selected.claude_session_id ? (
                <button
                  type="button"
                  className="primary"
                  onClick={() => resumeMeta(selected.id, selected.repo_key)}
                  disabled={busy}
                >
                  {busy ? (
                    <>
                      <Spinner /> Resuming…
                    </>
                  ) : (
                    "Resume"
                  )}
                </button>
              ) : (
                <p className="muted">
                  No <code>claude_session_id</code> was captured for this
                  session — can't resume. (If you just started it, wait a
                  second for the SessionStart hook.)
                </p>
              )}
            </div>
          )
        ) : (
          <div className="session-pane empty">
            <p className="muted">No Claude sessions in this workspace yet.</p>
            <p className="muted">
              Click <strong>+ New</strong> above to start one.
            </p>
          </div>
        )}
      </div>
    </div>
  );
}

function SessionBar({
  sessions,
  liveById,
  repos,
  selectedId,
  onSelect,
  onStartInRepo,
  busy,
}: {
  sessions: { id: string; repo_key: string; claude_session_id: string | null }[];
  liveById: Map<string, SessionInfo>;
  repos: { repo_key: string }[];
  selectedId: string | null;
  onSelect: (id: string) => void;
  onStartInRepo: (repoKey: string) => void;
  busy: boolean;
}) {
  const [menuOpen, setMenuOpen] = useState(false);
  const wrapRef = useRef<HTMLDivElement | null>(null);

  // Close the "+ New" repo menu on outside click.
  useEffect(() => {
    if (!menuOpen) return;
    const handler = (e: MouseEvent) => {
      if (wrapRef.current && !wrapRef.current.contains(e.target as Node)) {
        setMenuOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [menuOpen]);

  const onNewClick = () => {
    if (repos.length === 0) return;
    if (repos.length === 1) {
      onStartInRepo(repos[0].repo_key);
      return;
    }
    setMenuOpen((v) => !v);
  };

  return (
    <div className="session-chip-bar">
      {sessions.map((m) => {
        const live = liveById.get(m.id);
        const label = m.id.slice(0, 8);
        const needsTurn =
          live?.running &&
          (live.runtime_state === "idle" ||
            live.runtime_state === "waiting_input");
        return (
          <button
            key={m.id}
            type="button"
            className={`session-chip${selectedId === m.id ? " active" : ""}`}
            onClick={() => onSelect(m.id)}
          >
            <span className="chip-repo">{m.repo_key}</span>
            <code>{label}</code>
            {needsTurn && <span className="turn-dot" />}
            {live && !live.running && (
              <span className="chip-state">exited</span>
            )}
            {!live && <span className="chip-state">dormant</span>}
          </button>
        );
      })}
      <div className="new-session-wrap" ref={wrapRef}>
        <button
          type="button"
          className="session-chip new"
          onClick={onNewClick}
          disabled={busy || repos.length === 0}
          title={
            repos.length === 0
              ? "No repos in this workspace"
              : repos.length === 1
                ? `Start a new Claude session in ${repos[0].repo_key}`
                : "Start a new Claude session"
          }
        >
          {busy ? <Spinner /> : "+"} New
          {repos.length > 1 && <span className="caret">▾</span>}
        </button>
        {menuOpen && repos.length > 1 && (
          <div className="new-session-menu" role="menu">
            {repos.map((r) => (
              <button
                key={r.repo_key}
                type="button"
                role="menuitem"
                onClick={() => {
                  setMenuOpen(false);
                  onStartInRepo(r.repo_key);
                }}
              >
                New in <code>{r.repo_key}</code>
              </button>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

function WorkspaceInfoDialog({
  workspace,
  onClose,
}: {
  workspace: Workspace;
  onClose: () => void;
}) {
  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div
        className="modal info-modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
      >
        <h3>
          Workspace <code>{workspace.branch}</code>
        </h3>
        <dl className="workspace-fields">
          <dt>Created</dt>
          <dd>{new Date(workspace.created_at).toLocaleString()}</dd>
          <dt>Repos</dt>
          <dd>
            {workspace.repo_links.length === 0 ? (
              "(none)"
            ) : (
              <ul className="repo-link-list">
                {workspace.repo_links.map((r) => (
                  <li key={r.repo_key}>
                    <code>{r.repo_key}</code>
                    <span className="repo-link-path">{r.worktree_path}</span>
                    {r.setup_script_ran_at !== null && (
                      <span className="ok-badge">setup ok</span>
                    )}
                  </li>
                ))}
              </ul>
            )}
          </dd>
          <dt>ID</dt>
          <dd>
            <code>{workspace.id}</code>
          </dd>
        </dl>
        <div className="modal-actions">
          <button type="button" onClick={onClose} autoFocus>
            Close
          </button>
        </div>
      </div>
    </div>
  );
}

const LAST_REPO_SELECTION_KEY = "tethys.createWorkspace.lastRepoSelection";

function loadLastRepoSelection(repos: Repo[]): Set<string> {
  const available = new Set(repos.map((r) => r.key));
  try {
    const raw = localStorage.getItem(LAST_REPO_SELECTION_KEY);
    if (raw) {
      const parsed = JSON.parse(raw);
      if (Array.isArray(parsed)) {
        const restored = parsed.filter(
          (k): k is string => typeof k === "string" && available.has(k),
        );
        if (restored.length > 0) return new Set(restored);
      }
    }
  } catch {
    // fall through to default
  }
  return available;
}

function CreateWorkspaceDialog({
  repos,
  onClose,
  onSubmit,
}: {
  repos: Repo[];
  onClose: () => void;
  onSubmit: (args: CreateWorkspaceArgs) => void;
}) {
  const [branch, setBranch] = useState("");
  const [selected, setSelected] = useState<Set<string>>(() =>
    loadLastRepoSelection(repos),
  );

  const toggle = (key: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  };

  const canSubmit = branch.trim().length > 0 && selected.size > 0;

  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    const repoSelections = Array.from(selected);
    try {
      localStorage.setItem(
        LAST_REPO_SELECTION_KEY,
        JSON.stringify(repoSelections),
      );
    } catch {
      // non-fatal: preference just won't persist
    }
    onSubmit({
      branch: branch.trim(),
      repo_selections: repoSelections,
    });
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <form
        className="modal"
        onSubmit={submit}
        onClick={(e) => e.stopPropagation()}
      >
        <h3>New workspace</h3>
        <label>
          Branch
          <input
            autoFocus
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            value={branch}
            onChange={(e) => setBranch(e.target.value)}
            placeholder="e.g. ryan/session-resume"
          />
        </label>
        <div className="repo-select">
          <div className="repo-select-label">Repos</div>
          {repos.length === 0 ? (
            <p className="muted">
              No repos in registry. Add some to <code>repos.toml</code>.
            </p>
          ) : (
            <ul>
              {repos.map((r) => (
                <li key={r.key}>
                  <label className="repo-row">
                    <input
                      type="checkbox"
                      checked={selected.has(r.key)}
                      onChange={() => toggle(r.key)}
                    />
                    <span className="repo-display">{r.key}</span>
                  </label>
                </li>
              ))}
            </ul>
          )}
        </div>
        <div className="modal-actions">
          <button type="button" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="primary" disabled={!canSubmit}>
            Create
          </button>
        </div>
      </form>
    </div>
  );
}

function Spinner() {
  return <span className="spinner" aria-hidden="true" />;
}

export default App;
