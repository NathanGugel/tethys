# GitHub Integration Plan

## Goal

For every workspace, surface live-ish GitHub state (±60s) regardless of whether it is open:

- Is there a PR for the workspace branch? (per repo in the workspace)
- PR state: open / merged / closed
- CI rollup: pending / success / failure
- Unresolved review threads count
- **Merged → ready-to-delete** flag on the workspace

No realtime webhooks, no GitHub App. This is a personal macOS tool; a background poller is plenty.

## Non-goals

- Writing to GitHub (comments, merges, approvals). Read-only.
- Supporting non-GitHub remotes (GitLab, Bitbucket). Skip gracefully if `remote_url` isn't GitHub.
- Multi-account GitHub auth. Use whatever `gh` is logged into.

---

## Strategy at a glance

1. **Auth**: shell out to `gh api graphql`. Piggybacks on existing `gh auth login` state, handles SSO, no secret storage in tethys.
2. **Transport**: one GraphQL query per `(repo, branch)` pair, batched into a single request per tick using aliases.
3. **Cadence**: one background tokio task, ticks every 45s. Per-workspace jitter to avoid thundering herd after wake-from-sleep.
4. **State**: add `github: Option<GithubPrStatus>` to `RepoLink`. Persist via the existing `Store::mutate` path so writes stay atomic + debounced.
5. **UI**: emit `"github:status_changed"`; frontend merges into the same per-workspace derived view that already shows turn state. Workspace row grows a PR chip (number + check color + unresolved count). Merged workspaces get a "Ready to archive" affordance.

---

## Data model

### `RepoLink` (src-tauri/src/state.rs:34)

Add:

```rust
pub struct RepoLink {
    pub repo_key: String,
    pub worktree_path: PathBuf,
    pub setup_script_ran_at: Option<DateTime<Utc>>,
    pub github: Option<GithubPrStatus>,  // NEW
}

pub struct GithubPrStatus {
    pub pr_number: u32,
    pub url: String,
    pub state: PrState,             // Open | Merged | Closed
    pub is_draft: bool,
    pub checks: ChecksRollup,       // Pending | Success | Failure | Neutral | None
    pub unresolved_threads: u32,
    pub head_sha: String,           // so we can detect "checks are for old commit"
    pub fetched_at: DateTime<Utc>,
    pub last_error: Option<String>, // surfaces the last poll failure in UI
}
```

No new TS types library — mirror by hand in `src/types.ts` to match the Rust serde output.

### Workspace-level derived state

Not stored. Computed by the frontend:

- `allMerged = repo_links.every(r => r.github?.state === "Merged")` → show archive button.
- `anyFailing = repo_links.some(r => r.github?.checks === "Failure")` → red dot on workspace.
- `anyUnresolved = repo_links.some(r => (r.github?.unresolved_threads ?? 0) > 0)` → comment icon.

### Per-repo GitHub mapping

Parse `Repo.remote_url` (registry.rs:11) into `(owner, name)`:

- `git@github.com:owner/name.git` → `owner/name`
- `https://github.com/owner/name(.git)?` → `owner/name`
- anything else → mark repo "non-GitHub", skip forever.

Cache the parse result on the `Repo` at registry load time; no runtime re-parsing.

---

## Polling architecture

### Task lifecycle

Spawn once in `lib.rs` setup, alongside the existing `Store` flusher:

```rust
let poller = GithubPoller::new(store.clone(), registry.clone(), app.handle().clone());
tokio::spawn(poller.run());
```

`GithubPoller::run`:

```
loop {
    let ws_snapshot = store.read(|s| s.workspaces.clone());
    poll_all(&ws_snapshot).await;  // exactly one gh api graphql call
    tokio::time::sleep(45s + jitter(0..5s)).await;
}
```

### One request per tick — why this is safe

All `(repo, branch)` pairs across every workspace go into a single aliased GraphQL document, sent as one HTTP round-trip. GitHub's relevant limits:

- **5000 points/hour** (authenticated GraphQL). Point cost is driven by `first`/`last` on connections. Our per-alias cost is ~1 point (`first:1` PR, `last:1` commit, `first:50` review threads all fall in the cheap bucket).
- **500,000-node query complexity cap** per single query. We're orders of magnitude under.
- **10-second query timeout**.

Worst-case personal-use math: 10 repos × 1 point × 80 ticks/hr = **~800 points/hr** — under 20% of the budget, with the cadence I picked. Even 50 repos fits.

**Failure isolation**: GraphQL returns partial data + per-alias errors, so one broken/archived repo doesn't poison the whole tick. The poller applies successful aliases and records `last_error` on the failing ones.

**Only reason to ever chunk**: if we blow past the 500K-node cap or the 10s timeout, both of which require many hundreds of repos. Not a realistic personal-use scenario; leave it out until it's a real problem.

### Query shape

All workspaces share the same `branch` across their repos. Single aliased document:

```graphql
query($q0_owner:String!, $q0_name:String!, $q0_branch:String!, ...) {
  q0: repository(owner:$q0_owner, name:$q0_name) {
    ref(qualifiedName:$q0_branch) {
      associatedPullRequests(first:1, states:[OPEN,MERGED,CLOSED],
                             orderBy:{field:UPDATED_AT, direction:DESC}) {
        nodes {
          number url state isDraft
          reviewThreads(first:50) { nodes { isResolved } }
          commits(last:1) { nodes { commit {
            oid
            statusCheckRollup { state }
          } } }
        }
      }
    }
  }
  q1: repository(...) { ... }
}
```

**Why `ref.associatedPullRequests`** instead of `search("head:branch is:pr")`: cheaper, deterministic, works for closed PRs, avoids search index lag.

### GraphQL client

Shell out: `gh api graphql -f query=@- --jsonfield data`, pipe the query over stdin. Reasons:

- No new HTTP dependency in `Cargo.toml`.
- `gh` already handles token refresh, SSO, enterprise hosts.
- Error surfaces (`gh` exits non-zero with stderr) are trivial to propagate.

If `gh` is missing or unauthenticated, the poller logs once, sets `last_error` on every repo, and backs off to 10-minute retries until `gh auth status` succeeds.

### Error handling

Per-repo:

- Network error / 5xx / rate limit → keep last-known status, set `last_error`, exponential backoff on that repo only (45s → 90s → 3m → 10m cap).
- Branch has no ref on remote → `github = None`, clear prior state.
- Branch exists but no PR → `github = None` (we do NOT invent an empty PR record).
- Repo archived / access denied → mark non-GitHub and stop polling until restart.

---

## Persistence & event flow

### Writes

Every successful chunk fetch does a single `store.mutate`:

```rust
store.mutate(|s| {
    for (ws_id, repo_key, new_status) in results {
        if let Some(ws) = s.workspaces.iter_mut().find(|w| w.id == ws_id) {
            if let Some(rl) = ws.repo_links.iter_mut().find(|r| r.repo_key == repo_key) {
                if rl.github.as_ref() != Some(&new_status) {
                    rl.github = Some(new_status);
                    changed = true;
                }
            }
        }
    }
});
```

Debounced flusher writes state.json at most every 250ms — unchanged.

### Events

Only emit when something changed:

```rust
app.emit("github:status_changed", json!({
    "workspace_id": ws_id,
    "repo_key": repo_key,
    "status": new_status
}));
```

Frontend adds one `useTauriEvent("github:status_changed", ...)` handler in App.tsx next to `"session:turn_changed"` and merges into workspace state.

### Startup hydration

`state.json` already round-trips `RepoLink` via serde — add `#[serde(default)]` on `github` so older state files load cleanly. First poll tick refreshes everything; UI shows the last persisted status in the meantime (so relaunching the app doesn't blank out badges for 45s).

---

## UI surfaces

### Workspace row

- PR chip: `#1234` linking to `pr.url` via `tauri-plugin-opener`.
- Check dot: green / red / yellow / gray; tooltip with rollup state.
- Unresolved comment icon with count, only when `> 0`.
- Draft badge when `is_draft`.

### Merged → archive

When every repo in a workspace has `state = Merged`:

- Workspace header gets a subtle "Ready to archive" banner with a button.
- Button opens the existing delete-workspace confirm dialog, pre-checked "also delete branches".
- No auto-delete. Deletion is user-initiated.

### Stale poll indication

If `fetched_at` is >5 minutes old for any repo, dim the PR chip and tooltip the `last_error`. Don't show a scary toast — the UI should degrade quietly.

### Global

A gear-menu item "GitHub: connected as @ryanbaxley" / "GitHub: not authenticated — run `gh auth login`". Populated by running `gh api user --jq .login` once at startup and on poller-auth-failures.

---

## Observability

- `tracing` spans around each poll tick: `github.poll_tick { n_repos, ms, errors }`.
- Log the GraphQL document + variables at `trace` only (may contain branch names).
- Surface the most recent error per repo in `GithubPrStatus.last_error` so it's visible in state.json when debugging.

---

## Task list

Check off as work progresses. Order is roughly dependency-driven: remote-URL parsing has to exist before the poller can build queries, poller has to emit events before UI can consume them.

### 1. Remote URL parsing & registry plumbing

- [x] Add `src-tauri/src/github/remote_url.rs` with `parse_github_remote(&str) -> Option<GithubSlug>` handling both `git@github.com:owner/name.git` and `https://github.com/owner/name(.git)?` forms (trailing `.git` optional, trailing slash tolerated).
- [x] Unit tests for the parser: SSH, HTTPS, trailing `.git`, trailing `/`, non-GitHub host (GitLab, Bitbucket), malformed input, empty string.
- [x] Cache the parsed slug on `Repo` in `src-tauri/src/registry.rs` at load time (new field `github_slug: Option<GithubSlug>`).
- [x] Skip all GitHub work for repos where `github_slug` is `None` — log once at `info` per such repo at startup.

### 2. Data model

- [x] Add `GithubPrStatus`, `PrState`, `ChecksRollup` enums/structs to `src-tauri/src/github/status.rs` (derive `Serialize`, `Deserialize`, `Clone`, `PartialEq`, `Debug`).
- [x] Add `#[serde(default)] pub github: Option<GithubPrStatus>` to `RepoLink` in `src-tauri/src/state.rs`.
- [x] Verify old state.json files (pre-GitHub field) deserialize cleanly — regression test `pre_github_state_json_round_trips` under `src-tauri/src/state.rs`.
- [x] Mirror the types by hand in `src/types.ts` (snake_case variants on enums).

### 3. `gh` CLI integration layer

- [x] Add `src-tauri/src/github/client.rs` with `async fn run_graphql(query: &str, variables: &BTreeMap<String, String>) -> Result<Value, GhError>`.
- [x] Implementation: `tokio::process::Command::new("gh").args(["api", "graphql", "-f", "query=...", "-f", "var=val", ...])`. `-f` handles string variables cleanly; we never need nested JSON.
- [x] Error taxonomy: `GhError::NotInstalled` (exec failed ENOENT), `GhError::NotAuthenticated` (stderr contains auth patterns), `GhError::RateLimited`, `GhError::Network`, `GhError::Graphql(Vec<String>)` for per-alias errors, `GhError::Other`.
- [x] Probe function `async fn gh_login_status() -> Result<String, GhError>` that runs `gh api user --jq .login` once and returns the username.

### 4. Poller

- [x] Create `src-tauri/src/github/poller.rs` with `GithubPoller` struct holding store, registry, app handle, and a `Mutex<PollerInner>` tracking consecutive failures and auth state.
- [x] `run()` loop: 45s base interval + up to 5s jitter.
- [x] Build the aliased GraphQL document dynamically. Alias format `q{index}`. Variables as a `BTreeMap<String, String>`.
- [x] Parse response: walk aliases, per alias decide `Option<GithubPrStatus>`. Count `reviewThreads.nodes` with `!isResolved`. Zero out unresolved for non-Open.
- [x] Apply via a single `store.mutate` that only touches entries whose status actually changed (ignore `fetched_at` in the diff).
- [x] Emit `"github:status_changed"` per changed `(workspace_id, repo_key)`.
- [x] Exponential backoff on `RateLimited` / `Network` / `Graphql` / `Other` (45s → 90s → 3m → 10m cap) — global, since one request covers every repo.
- [x] On `NotAuthenticated`: switch to 10-minute cadence, warn-log once, stay there until a successful call.
- [x] On `NotInstalled`: warn once and disable polling for the lifetime of the process.
- [x] Unit tests: query building, response parsing (all four cases), backoff curve, change-detection-ignores-fetched_at.

### 5. Task wiring

- [x] Spawn the poller in `src-tauri/src/lib.rs` setup, alongside the existing store flusher. Managed via `Arc<GithubPoller>` for later command access.
- [x] Kick off an eager `probe_login` at startup so the auth state is known before the first tick.
- [x] Tauri window-focus hook: listen for `WindowEvent::Focused(true)` and call `request_tick`, which notifies the `Notify` to short-circuit the sleep. Rate-limited to at most one forced tick per 10s.

### 6. Frontend — event plumbing

- [x] Add `useTauriEvent<GithubStatusChangedEvent>("github:status_changed", ...)` in `src/App.tsx` next to the existing `session:turn_changed` handler.
- [x] Merge payloads into workspace state by `(workspace_id, repo_key)` without refetching the whole workspace list.
- [x] Export derived helpers in `src/workspaceDerived.ts`: `isReadyToArchive`, `checksSummary`, `unresolvedTotal`, `primaryRepoLink`, `isStale`.

### 7. Frontend — workspace row UI

- [x] `GithubChip` component in `src/GithubChip.tsx`: renders `#1234`, opens `pr.url` via `@tauri-apps/plugin-opener`. Hidden when `github` is `null`.
- [x] Check dot: green (Success) / red (Failure) / yellow (Pending) / gray (Neutral/None). Tooltip shows rollup state.
- [x] Unresolved count badge — hidden when count is 0 or PR state is not Open.
- [x] Draft / merged / closed badges on the chip.
- [x] Stale indication: `isStale(fetched_at)` flag triggers `.gh-stale` opacity reduction and tooltip mentions the stale timestamp.

### 8. Frontend — archive affordance

- [x] "Ready to archive" banner above the session pane when `isReadyToArchive(ws)`. Styled positive (violet/merged tone) instead of danger.
- [x] Banner button triggers the existing delete flow (`setConfirmingDelete(true)` → inline confirm → JobLogPane). No separate command needed; `delete_workspace` already deletes branches via `branch_delete_best_effort`.
- [x] Non-GitHub repos don't block the ready-to-archive state: `isReadyToArchive` filters to `github !== null` before the `every(merged)` check.

### 9. Global auth status

- [x] Sidebar footer `GithubAuthFooter` shows "● @login" (green) when authenticated, "● sign in" (amber) when not, or "● gh missing" (gray) when disabled. Title attribute holds the full message including the `gh auth login` hint.
- [x] Tauri commands `github_auth_status` / `github_reprobe_auth` exposed to the frontend.
- [x] Poller emits `"github:auth_changed"` when the auth state transitions (up on successful poll/probe; down on failure).
- [x] Clicking the footer re-probes. No auto-shell-out to `gh auth login` — user runs it themselves.

### 10. Observability

- [x] `tracing::info_span!("github.poll_tick", n_repos)` around each tick body. Success logs `applied` + `ms`; failure logs `ms` + error.
- [x] Log the GraphQL document and variables at `trace` (branch names may be private).
- [x] `last_error` field persists in `GithubPrStatus` in state.json. (Per-alias error attribution on partial GraphQL failures is deferred — currently whole-call failures are recorded in the poller log and auth state; per-repo `last_error` would only be written from partial-response paths, which are rare.)

### 11. Manual QA pass

**Automated verification (done):**

- [x] `pnpm tauri dev` boots cleanly, no panics.
- [x] Startup probe logs `gh authenticated login="rynobax"`.
- [x] First tick: `applied=2 ms=762` — both existing workspaces populated with real PR data.
- [x] Subsequent ticks: `applied=0` — no spurious re-emits (change-detection working).
- [x] Tick cadence: ~45-50s (matches base + jitter).
- [x] `state.json` persists the full `github` sub-object (PR number, url, state, checks, unresolved count, head SHA, fetched_at) — verified via `jq`.
- [x] Real data includes a case with `unresolved_threads: 1` — good test for the amber badge.
- [x] 34 Rust unit tests + TypeScript type-check all green.

**Interactive cases for the user to verify in the running app:**

- [ ] Sidebar shows PR chips on both `pending-segments-mismatch` (#2855) and `enter-in-curie-box-stale` (#2854).
- [ ] `enter-in-curie-box-stale` shows an amber "1" unresolved count badge.
- [ ] Both show green check dots.
- [ ] Clicking a PR chip opens the PR in the default browser.
- [ ] Sidebar footer shows "● @rynobax" in green.
- [ ] Merge a PR → "Ready to archive" banner appears on that workspace within ~45s.
- [ ] `gh auth logout` → footer turns amber "● sign in", ticks slow to 10-min cadence (visible in logs).
- [ ] `gh auth login` → click footer to re-probe, chips refresh.
- [ ] Window-focus kick: switch focus away, push a branch/open a PR, focus tethys → status updates faster than the 45s tick.

---

## Open questions

1. **Enterprise GitHub**: `gh` handles `GH_HOST` fine; do we need to expose a `github_host` field in `repos.toml` or trust `gh`'s host resolution? Lean: trust `gh`.
2. **Multiple PRs for one branch**: rare. Pick the most recently updated (already encoded in the query's `orderBy`). Document it, don't try to show both.
3. **Unresolved threads on closed PRs**: skip — only meaningful for open PRs. Zero it out on non-open state.
4. **Check rollup granularity**: do we care about individual failing check names, or just the rollup color? Start with rollup; add details on click later.
