import { useEffect, useMemo, useRef, useState } from "react";
import {
  DndContext,
  KeyboardSensor,
  PointerSensor,
  closestCenter,
  useSensor,
  useSensors,
  type DragEndEvent,
  type DraggableAttributes,
} from "@dnd-kit/core";
import type { SyntheticListenerMap } from "@dnd-kit/core/dist/hooks/utilities";
import {
  SortableContext,
  arrayMove,
  sortableKeyboardCoordinates,
  useSortable,
  verticalListSortingStrategy,
} from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";

import type { Workspace, WorkspaceId } from "./types";
import { GithubChip } from "./GithubChip";

type PendingCreate = { tempId: string; branch: string };

type Props = {
  /** Workspaces that should appear in the sidebar (soft-deleted already filtered out). */
  workspaces: Workspace[];
  selectedId: WorkspaceId | null;
  pendingCreate: PendingCreate | null;
  onSelect: (id: WorkspaceId) => void;
  onSelectPending: () => void;
  onReorder: (ids: WorkspaceId[]) => void;
  onPauseToggle: (ws: Workspace) => void;
  onArchiveToggle: (ws: Workspace) => void;
  onDelete: (ws: Workspace) => void;
  workspaceNeedsTurn: (ws: Workspace) => boolean;
};

export function Sidebar({
  workspaces,
  selectedId,
  pendingCreate,
  onSelect,
  onSelectPending,
  onReorder,
  onPauseToggle,
  onArchiveToggle,
  onDelete,
  workspaceNeedsTurn,
}: Props) {
  const { active, archived } = useMemo(() => {
    const active: Workspace[] = [];
    const archived: Workspace[] = [];
    for (const w of workspaces) {
      if (w.archived_at) archived.push(w);
      else active.push(w);
    }
    archived.sort((a, b) =>
      (b.archived_at ?? "").localeCompare(a.archived_at ?? ""),
    );
    return { active, archived };
  }, [workspaces]);

  const [archivedExpanded, setArchivedExpanded] = useState(false);
  const [menu, setMenu] = useState<{
    ws: Workspace;
    x: number;
    y: number;
  } | null>(null);

  const sensors = useSensors(
    // 5px activation distance prevents a single click from being interpreted
    // as a drag start, which would swallow row selection.
    useSensor(PointerSensor, { activationConstraint: { distance: 5 } }),
    useSensor(KeyboardSensor, {
      coordinateGetter: sortableKeyboardCoordinates,
    }),
  );

  const handleDragEnd = (event: DragEndEvent) => {
    const { active: dragged, over } = event;
    if (!over || dragged.id === over.id) return;
    const from = active.findIndex((w) => w.id === dragged.id);
    const to = active.findIndex((w) => w.id === over.id);
    if (from < 0 || to < 0) return;
    const next = arrayMove(active, from, to);
    onReorder(next.map((w) => w.id));
  };

  return (
    <>
      <ul className="workspace-list">
        {active.length === 0 && archived.length === 0 && !pendingCreate && (
          <li className="empty">No workspaces yet.</li>
        )}
        {pendingCreate && (
          <li
            key={pendingCreate.tempId}
            className={`pending${
              pendingCreate.tempId === selectedId ? " selected" : ""
            }`}
            onClick={onSelectPending}
          >
            <div className="workspace-name">
              <Spinner />
              {pendingCreate.branch}
            </div>
            <div className="pending-label">creating…</div>
          </li>
        )}
        <DndContext
          sensors={sensors}
          collisionDetection={closestCenter}
          onDragEnd={handleDragEnd}
        >
          <SortableContext
            items={active.map((w) => w.id)}
            strategy={verticalListSortingStrategy}
          >
            {active.map((w) => (
              <SortableWorkspaceRow
                key={w.id}
                workspace={w}
                selected={w.id === selectedId}
                needsTurn={workspaceNeedsTurn(w)}
                onSelect={() => onSelect(w.id)}
                onContextMenu={(x, y) => setMenu({ ws: w, x, y })}
              />
            ))}
          </SortableContext>
        </DndContext>

        {archived.length > 0 && (
          <li
            className="archive-header"
            onClick={() => setArchivedExpanded((v) => !v)}
          >
            <span className={`disclosure${archivedExpanded ? " open" : ""}`}>
              ▸
            </span>
            Archived
            <span className="archive-count">{archived.length}</span>
          </li>
        )}
        {archivedExpanded &&
          archived.map((w) => (
            <WorkspaceRow
              key={w.id}
              workspace={w}
              selected={w.id === selectedId}
              needsTurn={workspaceNeedsTurn(w)}
              isArchived
              onSelect={() => onSelect(w.id)}
              onContextMenu={(x, y) => setMenu({ ws: w, x, y })}
            />
          ))}
      </ul>
      {menu && (
        <ContextMenu
          x={menu.x}
          y={menu.y}
          workspace={menu.ws}
          onClose={() => setMenu(null)}
          onPauseToggle={onPauseToggle}
          onArchiveToggle={onArchiveToggle}
          onDelete={onDelete}
        />
      )}
    </>
  );
}

function SortableWorkspaceRow({
  workspace,
  selected,
  needsTurn,
  onSelect,
  onContextMenu,
}: {
  workspace: Workspace;
  selected: boolean;
  needsTurn: boolean;
  onSelect: () => void;
  onContextMenu: (x: number, y: number) => void;
}) {
  const { attributes, listeners, setNodeRef, transform, transition, isDragging } =
    useSortable({ id: workspace.id });

  const style = {
    transform: CSS.Transform.toString(transform),
    transition,
    zIndex: isDragging ? 10 : undefined,
  };

  return (
    <WorkspaceRow
      workspace={workspace}
      selected={selected}
      needsTurn={needsTurn}
      isDragging={isDragging}
      onSelect={onSelect}
      onContextMenu={onContextMenu}
      dndProps={{
        ref: setNodeRef,
        style,
        attributes,
        listeners,
      }}
    />
  );
}

type DndProps = {
  ref: (node: HTMLElement | null) => void;
  style: React.CSSProperties;
  attributes: DraggableAttributes;
  listeners: SyntheticListenerMap | undefined;
};

function WorkspaceRow({
  workspace,
  selected,
  needsTurn,
  isArchived = false,
  isDragging = false,
  onSelect,
  onContextMenu,
  dndProps,
}: {
  workspace: Workspace;
  selected: boolean;
  needsTurn: boolean;
  isArchived?: boolean;
  isDragging?: boolean;
  onSelect: () => void;
  onContextMenu: (x: number, y: number) => void;
  dndProps?: DndProps;
}) {
  const classes = [
    selected ? "selected" : "",
    workspace.paused ? "is-paused" : "",
    isArchived ? "is-archived" : "",
    isDragging ? "is-dragging" : "",
  ]
    .filter(Boolean)
    .join(" ");

  return (
    <li
      ref={dndProps?.ref}
      style={dndProps?.style}
      {...(dndProps?.attributes ?? {})}
      {...(dndProps?.listeners ?? {})}
      className={classes}
      onClick={onSelect}
      onContextMenu={(e) => {
        e.preventDefault();
        onContextMenu(e.clientX, e.clientY);
      }}
    >
      <div className="workspace-name">
        {needsTurn && (
          <span
            className="turn-dot"
            title="Your turn"
            aria-label="your turn"
          />
        )}
        {workspace.branch}
      </div>
      {workspace.repo_links.length > 0 && (
        <ul className="workspace-repo-list">
          {workspace.repo_links.map((r) => (
            <li key={r.repo_key}>
              <span className="repo-key">{r.repo_key}</span>
              {r.github && (
                <div className="repo-gh-footer">
                  <GithubChip status={r.github} linkable={false} />
                </div>
              )}
            </li>
          ))}
        </ul>
      )}
    </li>
  );
}

function ContextMenu({
  x,
  y,
  workspace,
  onClose,
  onPauseToggle,
  onArchiveToggle,
  onDelete,
}: {
  x: number;
  y: number;
  workspace: Workspace;
  onClose: () => void;
  onPauseToggle: (ws: Workspace) => void;
  onArchiveToggle: (ws: Workspace) => void;
  onDelete: (ws: Workspace) => void;
}) {
  const ref = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    const handle = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("mousedown", handle);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", handle);
      document.removeEventListener("keydown", onKey);
    };
  }, [onClose]);

  // Keep the menu inside the viewport.
  const viewportW = window.innerWidth;
  const viewportH = window.innerHeight;
  const ESTIMATED_W = 180;
  const ESTIMATED_H = 110;
  const left = Math.min(x, viewportW - ESTIMATED_W - 4);
  const top = Math.min(y, viewportH - ESTIMATED_H - 4);

  const wrap = (fn: () => void) => () => {
    fn();
    onClose();
  };

  return (
    <div
      ref={ref}
      className="context-menu"
      style={{ left, top }}
      role="menu"
    >
      <button
        type="button"
        role="menuitem"
        onClick={wrap(() => onPauseToggle(workspace))}
      >
        {workspace.paused ? "Resume" : "Pause"}
      </button>
      <button
        type="button"
        role="menuitem"
        onClick={wrap(() => onArchiveToggle(workspace))}
      >
        {workspace.archived_at ? "Unarchive" : "Archive"}
      </button>
      <div className="context-menu-sep" />
      <button
        type="button"
        role="menuitem"
        className="danger"
        onClick={wrap(() => onDelete(workspace))}
      >
        Delete
      </button>
    </div>
  );
}

function Spinner() {
  return <span className="spinner" aria-hidden="true" />;
}
