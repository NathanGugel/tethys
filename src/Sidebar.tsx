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

type ActiveItem =
  | { kind: "workspace"; id: string; workspace: Workspace }
  | { kind: "pending"; id: string; pending: PendingCreate };

type Props = {
  /** Workspaces that should appear in the sidebar (soft-deleted already filtered out). */
  workspaces: Workspace[];
  selectedId: WorkspaceId | null;
  pendingCreates: PendingCreate[];
  /** Unified ordering of active items — workspace ids and pending tempIds mixed. */
  displayOrder: string[];
  onSelect: (id: WorkspaceId) => void;
  onSelectPending: (tempId: string) => void;
  onReorder: (orderedIds: string[]) => void;
  onArchiveToggle: (ws: Workspace) => void;
  onDelete: (ws: Workspace) => void;
  onClearTurn: (ws: Workspace) => void;
  workspaceNeedsTurn: (ws: Workspace) => boolean;
};

export function Sidebar({
  workspaces,
  selectedId,
  pendingCreates,
  displayOrder,
  onSelect,
  onSelectPending,
  onReorder,
  onArchiveToggle,
  onDelete,
  onClearTurn,
  workspaceNeedsTurn,
}: Props) {
  const { activeItems, archived } = useMemo(() => {
    const wsById = new Map(workspaces.map((w) => [w.id, w]));
    const pendingById = new Map(pendingCreates.map((p) => [p.tempId, p]));
    const archived: Workspace[] = [];
    for (const w of workspaces) {
      if (w.archived_at) archived.push(w);
    }
    archived.sort((a, b) =>
      (b.archived_at ?? "").localeCompare(a.archived_at ?? ""),
    );

    const seen = new Set<string>();
    const activeItems: ActiveItem[] = [];
    const pushWs = (w: Workspace) => {
      if (w.archived_at) return;
      activeItems.push({ kind: "workspace", id: w.id, workspace: w });
    };
    for (const id of displayOrder) {
      if (seen.has(id)) continue;
      seen.add(id);
      const ws = wsById.get(id);
      if (ws) {
        pushWs(ws);
        continue;
      }
      const p = pendingById.get(id);
      if (p) activeItems.push({ kind: "pending", id: p.tempId, pending: p });
    }
    // Anything not yet in displayOrder (transient — reconcile in App.tsx
    // will catch up next render). Pendings prepend, workspaces append.
    for (const p of pendingCreates) {
      if (seen.has(p.tempId)) continue;
      seen.add(p.tempId);
      activeItems.unshift({ kind: "pending", id: p.tempId, pending: p });
    }
    for (const w of workspaces) {
      if (seen.has(w.id) || w.archived_at) continue;
      seen.add(w.id);
      pushWs(w);
    }
    return { activeItems, archived };
  }, [workspaces, pendingCreates, displayOrder]);

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
    const ids = activeItems.map((i) => i.id);
    const from = ids.indexOf(String(dragged.id));
    const to = ids.indexOf(String(over.id));
    if (from < 0 || to < 0) return;
    onReorder(arrayMove(ids, from, to));
  };

  return (
    <>
      <ul className="workspace-list">
        {activeItems.length === 0 && archived.length === 0 && (
          <li className="empty">No workspaces yet.</li>
        )}
        <DndContext
          sensors={sensors}
          collisionDetection={closestCenter}
          onDragEnd={handleDragEnd}
        >
          <SortableContext
            items={activeItems.map((i) => i.id)}
            strategy={verticalListSortingStrategy}
          >
            {activeItems.map((item) =>
              item.kind === "workspace" ? (
                <SortableWorkspaceRow
                  key={item.id}
                  workspace={item.workspace}
                  selected={item.id === selectedId}
                  needsTurn={workspaceNeedsTurn(item.workspace)}
                  onSelect={() => onSelect(item.workspace.id)}
                  onContextMenu={(x, y) =>
                    setMenu({ ws: item.workspace, x, y })
                  }
                />
              ) : (
                <SortablePendingRow
                  key={item.id}
                  pending={item.pending}
                  selected={item.id === selectedId}
                  onSelect={() => onSelectPending(item.pending.tempId)}
                />
              ),
            )}
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
          hasTurn={workspaceNeedsTurn(menu.ws)}
          onClose={() => setMenu(null)}
          onArchiveToggle={onArchiveToggle}
          onDelete={onDelete}
          onClearTurn={onClearTurn}
        />
      )}
    </>
  );
}

function SortablePendingRow({
  pending,
  selected,
  onSelect,
}: {
  pending: PendingCreate;
  selected: boolean;
  onSelect: () => void;
}) {
  const { attributes, listeners, setNodeRef, transform, transition, isDragging } =
    useSortable({ id: pending.tempId });

  const style = {
    transform: CSS.Transform.toString(transform),
    transition,
    zIndex: isDragging ? 10 : undefined,
  };

  const classes = ["pending", selected ? "selected" : "", isDragging ? "is-dragging" : ""]
    .filter(Boolean)
    .join(" ");

  return (
    <li
      ref={setNodeRef}
      style={style}
      {...attributes}
      {...listeners}
      className={classes}
      onClick={onSelect}
    >
      <div className="workspace-name">
        <Spinner />
        {pending.branch}
      </div>
      <div className="pending-label">creating…</div>
    </li>
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
        {workspace.branch}
        {needsTurn && (
          <span
            className="turn-dot"
            title="Your turn"
            aria-label="your turn"
          />
        )}
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
  hasTurn,
  onClose,
  onArchiveToggle,
  onDelete,
  onClearTurn,
}: {
  x: number;
  y: number;
  workspace: Workspace;
  hasTurn: boolean;
  onClose: () => void;
  onArchiveToggle: (ws: Workspace) => void;
  onDelete: (ws: Workspace) => void;
  onClearTurn: (ws: Workspace) => void;
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
      {hasTurn && (
        <button
          type="button"
          role="menuitem"
          onClick={wrap(() => onClearTurn(workspace))}
        >
          Clear notification
        </button>
      )}
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
