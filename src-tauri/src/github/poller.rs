use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tokio::sync::{Mutex, Notify};
use tracing::{info, info_span, trace, warn, Instrument};

use crate::github::client::{gh_login_status, run_graphql, GhError};
use crate::github::status::{ChecksRollup, GithubPrStatus, PrState, ReviewDecision};
use crate::github::GithubSlug;
use crate::registry::RegistryLoad;
use crate::state::{AppState, WorkspaceId};
use crate::store::Store;

const BASE_INTERVAL: Duration = Duration::from_secs(45);
const MAX_BACKOFF: Duration = Duration::from_secs(600);
const AUTH_RETRY_INTERVAL: Duration = Duration::from_secs(600);
const FORCE_TICK_MIN_SPACING: Duration = Duration::from_secs(10);

/// Runtime state mutated only from within `run()`.
#[derive(Debug, Default)]
struct PollerInner {
    consecutive_failures: u32,
    auth: AuthState,
    login: Option<String>,
    last_forced_tick: Option<Instant>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthState {
    #[default]
    Unknown,
    Authenticated,
    /// `gh` is installed but not logged in. Slow-poll and re-probe.
    NotAuthenticated,
    /// `gh` is missing. Stop polling for the lifetime of the process.
    Disabled,
}

/// Snapshot of auth state returned by the `github_auth_status` Tauri command.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuthSnapshot {
    pub state: AuthState,
    pub login: Option<String>,
}

pub struct GithubPoller {
    store: Arc<Store>,
    registry: Arc<RegistryLoad>,
    app: AppHandle,
    inner: Mutex<PollerInner>,
    force: Arc<Notify>,
}

impl GithubPoller {
    pub fn new(store: Arc<Store>, registry: Arc<RegistryLoad>, app: AppHandle) -> Self {
        Self {
            store,
            registry,
            app,
            inner: Mutex::new(PollerInner::default()),
            force: Arc::new(Notify::new()),
        }
    }

    /// Long-running loop. Spawn with `tokio::spawn(poller.run())`.
    pub async fn run(self: Arc<Self>) {
        loop {
            let delay = self.next_delay().await;
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = self.force.notified() => {}
            }
            if self.inner.lock().await.auth == AuthState::Disabled {
                return;
            }
            self.tick().await;
        }
    }

    /// Request an immediate poll tick, bypassing the normal sleep. Rate-limited
    /// so rapid window focus/blur doesn't cause a storm.
    pub async fn request_tick(&self) {
        let mut inner = self.inner.lock().await;
        let now = Instant::now();
        if let Some(prev) = inner.last_forced_tick {
            if now.duration_since(prev) < FORCE_TICK_MIN_SPACING {
                return;
            }
        }
        inner.last_forced_tick = Some(now);
        drop(inner);
        self.force.notify_one();
    }

    async fn next_delay(&self) -> Duration {
        let inner = self.inner.lock().await;
        match inner.auth {
            AuthState::Disabled => Duration::from_secs(u64::MAX),
            AuthState::NotAuthenticated => AUTH_RETRY_INTERVAL,
            _ => {
                let backoff = backoff_for(inner.consecutive_failures);
                let jitter = Duration::from_millis(fastrand::u64(0..5_000));
                backoff + jitter
            }
        }
    }

    async fn tick(self: &Arc<Self>) {
        let targets = self.collect_targets().await;
        if targets.is_empty() {
            return;
        }

        let span = info_span!("github.poll_tick", n_repos = targets.len());
        async {
            let start = std::time::Instant::now();
            match self.do_poll(&targets).await {
                Ok(applied) => {
                    let prev_auth = {
                        let mut inner = self.inner.lock().await;
                        inner.consecutive_failures = 0;
                        let prev = inner.auth.clone();
                        inner.auth = AuthState::Authenticated;
                        prev
                    };
                    if prev_auth != AuthState::Authenticated {
                        self.emit_auth_changed().await;
                    }
                    info!(
                        applied = applied,
                        ms = start.elapsed().as_millis() as u64,
                        "poll tick ok",
                    );
                }
                Err(e) => {
                    let ms = start.elapsed().as_millis() as u64;
                    warn!(ms = ms, error = %e, "poll tick failed");
                    self.handle_error(e).await;
                }
            }
        }
        .instrument(span)
        .await;
    }

    async fn emit_auth_changed(&self) {
        let snap = self.auth_snapshot().await;
        let _ = self.app.emit("github:auth_changed", &snap);
    }

    pub async fn auth_snapshot(&self) -> AuthSnapshot {
        let inner = self.inner.lock().await;
        AuthSnapshot {
            state: inner.auth.clone(),
            login: inner.login.clone(),
        }
    }

    async fn collect_targets(&self) -> Vec<Target> {
        let registry = match self.registry.as_ref() {
            RegistryLoad::Ok { registry, .. } => registry,
            _ => return Vec::new(),
        };

        let snapshot: AppState = self.store.read(|s| s.clone()).await;
        let mut out = Vec::new();
        for ws in snapshot.workspaces {
            for link in &ws.repo_links {
                let Some(repo) = registry.find_repo(&link.repo_key) else {
                    continue;
                };
                let Some(slug) = &repo.github_slug else {
                    continue;
                };
                out.push(Target {
                    workspace_id: ws.id.clone(),
                    repo_key: link.repo_key.clone(),
                    slug: slug.clone(),
                    branch: ws.branch.clone(),
                });
            }
        }
        out
    }

    async fn do_poll(&self, targets: &[Target]) -> Result<usize, GhError> {
        let (query, variables) = build_query(targets);
        trace!(query = %query, variables = ?variables, "github graphql request");
        let data = run_graphql(&query, &variables).await?;
        trace!(data = %data, "github graphql response");
        let parsed = parse_response(targets, &data);

        let changed = self
            .store
            .mutate(|s| Ok(apply_results(s, &parsed)))
            .await
            .unwrap_or_default();

        for (ws_id, repo_key, status) in &changed {
            let _ = self.app.emit(
                "github:status_changed",
                json!({
                    "workspace_id": ws_id,
                    "repo_key": repo_key,
                    "status": status,
                }),
            );
        }
        Ok(changed.len())
    }

    async fn handle_error(&self, err: GhError) {
        let mut auth_changed = false;
        {
            let mut inner = self.inner.lock().await;
            match &err {
                GhError::NotInstalled => {
                    warn!("gh CLI not installed — disabling GitHub polling");
                    if inner.auth != AuthState::Disabled {
                        auth_changed = true;
                    }
                    inner.auth = AuthState::Disabled;
                    inner.login = None;
                }
                GhError::NotAuthenticated => {
                    if inner.auth != AuthState::NotAuthenticated {
                        warn!("gh not authenticated — slowing poll; run `gh auth login`");
                        auth_changed = true;
                    }
                    inner.auth = AuthState::NotAuthenticated;
                    inner.login = None;
                }
                GhError::RateLimited | GhError::Network(_) => {
                    inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
                    warn!(
                        error = %err,
                        failures = inner.consecutive_failures,
                        "poll tick failed, backing off",
                    );
                }
                GhError::Graphql(msgs) => {
                    inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
                    warn!(errors = ?msgs, "poll tick returned GraphQL errors");
                }
                GhError::Other(msg) => {
                    inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
                    warn!(error = %msg, "poll tick errored");
                }
            }
        }
        if auth_changed {
            self.emit_auth_changed().await;
        }
    }

    /// Run a one-shot login probe (used at startup and UI "retry auth").
    pub async fn probe_login(self: &Arc<Self>) -> Option<String> {
        let result = gh_login_status().await;
        let (new_auth, new_login, returned) = match &result {
            Ok(login) => (
                AuthState::Authenticated,
                Some(login.clone()),
                Some(login.clone()),
            ),
            Err(GhError::NotInstalled) => (AuthState::Disabled, None, None),
            Err(_) => (AuthState::NotAuthenticated, None, None),
        };
        let changed = {
            let mut inner = self.inner.lock().await;
            let prev_auth = inner.auth.clone();
            let prev_login = inner.login.clone();
            inner.auth = new_auth.clone();
            inner.login = new_login.clone();
            prev_auth != new_auth || prev_login != new_login
        };
        if changed {
            self.emit_auth_changed().await;
        }
        returned
    }
}

#[derive(Debug, Clone)]
struct Target {
    workspace_id: WorkspaceId,
    repo_key: String,
    slug: GithubSlug,
    branch: String,
}

fn backoff_for(failures: u32) -> Duration {
    if failures == 0 {
        return BASE_INTERVAL;
    }
    let secs = BASE_INTERVAL.as_secs() * (1u64 << failures.min(5));
    Duration::from_secs(secs).min(MAX_BACKOFF)
}

fn build_query(targets: &[Target]) -> (String, BTreeMap<String, String>) {
    let mut vars = BTreeMap::new();
    let mut var_decls = Vec::new();
    let mut body = String::new();

    // Shared selection set for a PR node — same shape whether we found it via
    // the branch ref or via the merged-PRs fallback.
    const PR_FIELDS: &str = r#"number
          url
          state
          isDraft
          reviewDecision
          reviewThreads(first: 50) { nodes { isResolved } }
          commits(last: 1) {
            nodes {
              commit {
                oid
                statusCheckRollup {
                  state
                  contexts(first: 100) {
                    nodes {
                      __typename
                      ... on CheckRun {
                        name
                        status
                        conclusion
                      }
                      ... on StatusContext {
                        context
                        state
                      }
                    }
                  }
                }
              }
            }
          }"#;

    for (i, t) in targets.iter().enumerate() {
        let ow = format!("q{i}_owner");
        let nm = format!("q{i}_name");
        let br = format!("q{i}_branch");
        let bn = format!("q{i}_branch_name");
        vars.insert(ow.clone(), t.slug.owner.clone());
        vars.insert(nm.clone(), t.slug.name.clone());
        vars.insert(br.clone(), format!("refs/heads/{}", t.branch));
        vars.insert(bn.clone(), t.branch.clone());
        var_decls.push(format!(
            "${ow}: String!, ${nm}: String!, ${br}: String!, ${bn}: String!"
        ));
        // `mergedPrs` is the fallback for when the branch has been deleted
        // post-merge: GitHub nulls the `ref`, but the PR record persists and
        // is queryable by headRefName.
        body.push_str(&format!(
            r#"q{i}: repository(owner: ${ow}, name: ${nm}) {{
    ref(qualifiedName: ${br}) {{
      associatedPullRequests(first: 1, orderBy: {{field: UPDATED_AT, direction: DESC}}) {{
        nodes {{
          {PR_FIELDS}
        }}
      }}
    }}
    mergedPrs: pullRequests(headRefName: ${bn}, states: [MERGED, CLOSED], first: 1, orderBy: {{field: UPDATED_AT, direction: DESC}}) {{
      nodes {{
        {PR_FIELDS}
      }}
    }}
  }}
"#
        ));
    }

    let decls = var_decls.join(", ");
    let query = format!("query({decls}) {{\n  {body}}}\n");
    (query, vars)
}

fn parse_response(targets: &[Target], data: &Value) -> Vec<(WorkspaceId, String, Option<GithubPrStatus>)> {
    let mut out = Vec::with_capacity(targets.len());
    for (i, t) in targets.iter().enumerate() {
        let alias = format!("q{i}");
        let node = data.get(&alias);
        let status = node.and_then(parse_repo_node);
        out.push((t.workspace_id.clone(), t.repo_key.clone(), status));
    }
    out
}

fn parse_repo_node(repo: &Value) -> Option<GithubPrStatus> {
    // Prefer the PR associated with the live branch ref. If the branch was
    // deleted on merge, `ref` will be null — fall back to the most recent
    // merged/closed PR for that branch name.
    let assoc = repo
        .get("ref")
        .and_then(|r| r.get("associatedPullRequests"))
        .and_then(|a| a.get("nodes"))
        .and_then(|n| n.as_array())
        .and_then(|arr| arr.first());
    let merged_fallback = repo
        .get("mergedPrs")
        .and_then(|m| m.get("nodes"))
        .and_then(|n| n.as_array())
        .and_then(|arr| arr.first());
    let pr = assoc.or(merged_fallback)?;

    let number = pr.get("number")?.as_u64()? as u32;
    let url = pr.get("url")?.as_str()?.to_string();
    let state = match pr.get("state")?.as_str()? {
        "OPEN" => PrState::Open,
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ => return None,
    };
    let is_draft = pr.get("isDraft").and_then(|v| v.as_bool()).unwrap_or(false);

    let review_decision = if state == PrState::Open {
        match pr.get("reviewDecision").and_then(|v| v.as_str()) {
            Some("APPROVED") => ReviewDecision::Approved,
            Some("CHANGES_REQUESTED") => ReviewDecision::ChangesRequested,
            Some("REVIEW_REQUIRED") => ReviewDecision::ReviewRequired,
            _ => ReviewDecision::None,
        }
    } else {
        ReviewDecision::None
    };

    let unresolved_threads = if state == PrState::Open {
        pr.get("reviewThreads")
            .and_then(|r| r.get("nodes"))
            .and_then(|n| n.as_array())
            .map(|arr| {
                arr.iter()
                    .filter(|t| {
                        !t.get("isResolved")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                    })
                    .count() as u32
            })
            .unwrap_or(0)
    } else {
        0
    };

    let commit = pr
        .get("commits")
        .and_then(|c| c.get("nodes"))
        .and_then(|n| n.as_array())
        .and_then(|arr| arr.first())
        .and_then(|node| node.get("commit"));
    let head_sha = commit
        .and_then(|c| c.get("oid"))
        .and_then(|o| o.as_str())
        .unwrap_or("")
        .to_string();
    let rollup = commit.and_then(|c| c.get("statusCheckRollup"));
    let rollup_state = rollup
        .and_then(|r| if r.is_null() { None } else { r.get("state") })
        .and_then(|s| s.as_str())
        .map(rollup_state_from_str);
    let context_nodes = rollup
        .and_then(|r| r.get("contexts"))
        .and_then(|c| c.get("nodes"))
        .and_then(|n| n.as_array());

    // Split bugbot out of the rollup. If we have per-context data, recompute
    // the non-bugbot aggregate so the CI indicator isn't poisoned by bugbot's
    // result. Otherwise fall back to the top-level rollup state.
    let (checks, bugbot) = match context_nodes {
        Some(nodes) => {
            let mut non_bugbot = Vec::new();
            let mut bugbot_states = Vec::new();
            for node in nodes {
                let Some(state) = context_state(node) else {
                    continue;
                };
                if context_is_bugbot(node) {
                    bugbot_states.push(state);
                } else {
                    non_bugbot.push(state);
                }
            }
            let checks = aggregate_rollup(non_bugbot.into_iter());
            let bugbot = aggregate_rollup(bugbot_states.into_iter());
            (checks, bugbot)
        }
        None => (rollup_state.unwrap_or(ChecksRollup::None), ChecksRollup::None),
    };

    Some(GithubPrStatus {
        pr_number: number,
        url,
        state,
        is_draft,
        checks,
        bugbot,
        review_decision,
        unresolved_threads,
        head_sha,
        fetched_at: Utc::now(),
        last_error: None,
    })
}

fn rollup_state_from_str(s: &str) -> ChecksRollup {
    match s {
        "SUCCESS" => ChecksRollup::Success,
        "FAILURE" | "ERROR" => ChecksRollup::Failure,
        "PENDING" | "EXPECTED" => ChecksRollup::Pending,
        _ => ChecksRollup::Neutral,
    }
}

fn context_is_bugbot(node: &Value) -> bool {
    let name = node
        .get("name")
        .and_then(|v| v.as_str())
        .or_else(|| node.get("context").and_then(|v| v.as_str()))
        .unwrap_or("");
    name.to_lowercase().contains("bugbot")
}

/// Map a single check-run / status-context node to a rollup-style state.
/// Mirrors GitHub's own aggregation: any incomplete check is `Pending`;
/// completed checks use their `conclusion`.
fn context_state(node: &Value) -> Option<ChecksRollup> {
    let typename = node.get("__typename").and_then(|v| v.as_str())?;
    match typename {
        "CheckRun" => {
            let status = node.get("status").and_then(|s| s.as_str()).unwrap_or("");
            if status != "COMPLETED" {
                return Some(ChecksRollup::Pending);
            }
            match node.get("conclusion").and_then(|c| c.as_str())? {
                "SUCCESS" => Some(ChecksRollup::Success),
                "FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED"
                | "CANCELLED" | "STALE" => Some(ChecksRollup::Failure),
                "NEUTRAL" | "SKIPPED" => Some(ChecksRollup::Neutral),
                _ => None,
            }
        }
        "StatusContext" => match node.get("state").and_then(|s| s.as_str())? {
            "SUCCESS" => Some(ChecksRollup::Success),
            "FAILURE" | "ERROR" => Some(ChecksRollup::Failure),
            "PENDING" | "EXPECTED" => Some(ChecksRollup::Pending),
            _ => None,
        },
        _ => None,
    }
}

fn aggregate_rollup(states: impl Iterator<Item = ChecksRollup>) -> ChecksRollup {
    let mut has_failure = false;
    let mut has_pending = false;
    let mut has_success = false;
    let mut has_neutral = false;
    for s in states {
        match s {
            ChecksRollup::Failure => has_failure = true,
            ChecksRollup::Pending => has_pending = true,
            ChecksRollup::Success => has_success = true,
            ChecksRollup::Neutral => has_neutral = true,
            ChecksRollup::None => {}
        }
    }
    if has_failure {
        ChecksRollup::Failure
    } else if has_pending {
        ChecksRollup::Pending
    } else if has_success {
        ChecksRollup::Success
    } else if has_neutral {
        ChecksRollup::Neutral
    } else {
        ChecksRollup::None
    }
}

/// Apply parsed results to `AppState`, returning the set of changes to emit.
fn apply_results(
    state: &mut AppState,
    results: &[(WorkspaceId, String, Option<GithubPrStatus>)],
) -> Vec<(WorkspaceId, String, Option<GithubPrStatus>)> {
    let mut changed = Vec::new();
    for (ws_id, repo_key, new_status) in results {
        let Some(ws) = state.find_workspace_mut(ws_id) else {
            continue;
        };
        let Some(link) = ws.repo_links.iter_mut().find(|r| &r.repo_key == repo_key) else {
            continue;
        };
        if is_meaningful_change(link.github.as_ref(), new_status.as_ref()) {
            link.github = new_status.clone();
            changed.push((ws_id.clone(), repo_key.clone(), new_status.clone()));
        }
    }
    changed
}

/// Compare two statuses ignoring `fetched_at`. Emit only on real changes so
/// we don't spam the UI every 45s with "nothing changed, but the timestamp
/// did".
fn is_meaningful_change(old: Option<&GithubPrStatus>, new: Option<&GithubPrStatus>) -> bool {
    match (old, new) {
        (None, None) => false,
        (None, Some(_)) | (Some(_), None) => true,
        (Some(a), Some(b)) => {
            a.pr_number != b.pr_number
                || a.url != b.url
                || a.state != b.state
                || a.is_draft != b.is_draft
                || a.checks != b.checks
                || a.bugbot != b.bugbot
                || a.review_decision != b.review_decision
                || a.unresolved_threads != b.unresolved_threads
                || a.head_sha != b.head_sha
                || a.last_error != b.last_error
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_target(i: usize) -> Target {
        Target {
            workspace_id: format!("ws-{i}"),
            repo_key: "frontend".to_string(),
            slug: GithubSlug {
                owner: "rynobax".to_string(),
                name: "tethys".to_string(),
            },
            branch: format!("feat/foo-{i}"),
        }
    }

    #[test]
    fn query_builds_aliases_and_variables() {
        let targets = vec![mk_target(0), mk_target(1)];
        let (q, vars) = build_query(&targets);
        assert!(q.contains("q0: repository(owner: $q0_owner"));
        assert!(q.contains("q1: repository(owner: $q1_owner"));
        assert!(q.contains("mergedPrs: pullRequests(headRefName: $q0_branch_name"));
        assert_eq!(vars.get("q0_owner").unwrap(), "rynobax");
        assert_eq!(vars.get("q0_branch").unwrap(), "refs/heads/feat/foo-0");
        assert_eq!(vars.get("q0_branch_name").unwrap(), "feat/foo-0");
        assert_eq!(vars.get("q1_branch").unwrap(), "refs/heads/feat/foo-1");
        assert_eq!(vars.get("q1_branch_name").unwrap(), "feat/foo-1");
    }

    #[test]
    fn parse_no_branch_returns_none() {
        let data = json!({ "q0": { "ref": null } });
        let parsed = parse_response(&[mk_target(0)], &data);
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].2.is_none());
    }

    #[test]
    fn parse_no_pr_returns_none() {
        let data = json!({
            "q0": {
                "ref": { "associatedPullRequests": { "nodes": [] } }
            }
        });
        let parsed = parse_response(&[mk_target(0)], &data);
        assert!(parsed[0].2.is_none());
    }

    #[test]
    fn parse_open_pr_with_checks() {
        let data = json!({
            "q0": {
                "ref": {
                    "associatedPullRequests": {
                        "nodes": [{
                            "number": 42,
                            "url": "https://github.com/rynobax/tethys/pull/42",
                            "state": "OPEN",
                            "isDraft": false,
                            "reviewThreads": {
                                "nodes": [
                                    {"isResolved": false},
                                    {"isResolved": true},
                                    {"isResolved": false}
                                ]
                            },
                            "commits": {
                                "nodes": [{"commit": {
                                    "oid": "abc123",
                                    "statusCheckRollup": {"state": "FAILURE"}
                                }}]
                            }
                        }]
                    }
                }
            }
        });
        let parsed = parse_response(&[mk_target(0)], &data);
        let status = parsed[0].2.as_ref().expect("should parse");
        assert_eq!(status.pr_number, 42);
        assert_eq!(status.state, PrState::Open);
        assert_eq!(status.checks, ChecksRollup::Failure);
        assert_eq!(status.unresolved_threads, 2);
        assert_eq!(status.head_sha, "abc123");
    }

    #[test]
    fn parse_falls_back_to_merged_prs_when_ref_null() {
        // Branch deleted post-merge: GitHub returns `ref: null`, but the PR
        // record is still reachable via pullRequests(headRefName:).
        let data = json!({
            "q0": {
                "ref": null,
                "mergedPrs": {
                    "nodes": [{
                        "number": 99,
                        "url": "https://github.com/x/y/pull/99",
                        "state": "MERGED",
                        "isDraft": false,
                        "reviewThreads": { "nodes": [] },
                        "commits": {
                            "nodes": [{"commit": {"oid": "deadbeef", "statusCheckRollup": null}}]
                        }
                    }]
                }
            }
        });
        let status = parse_response(&[mk_target(0)], &data)[0]
            .2
            .clone()
            .expect("should fall back to mergedPrs");
        assert_eq!(status.pr_number, 99);
        assert_eq!(status.state, PrState::Merged);
    }

    #[test]
    fn parse_prefers_ref_over_merged_prs_fallback() {
        // Branch still live with an open PR — ignore any older merged PRs
        // that happen to share the branch name.
        let data = json!({
            "q0": {
                "ref": {
                    "associatedPullRequests": {
                        "nodes": [{
                            "number": 5,
                            "url": "u",
                            "state": "OPEN",
                            "isDraft": false,
                            "reviewThreads": {"nodes": []},
                            "commits": {
                                "nodes": [{"commit": {"oid": "o", "statusCheckRollup": null}}]
                            }
                        }]
                    }
                },
                "mergedPrs": {
                    "nodes": [{
                        "number": 3,
                        "url": "u",
                        "state": "MERGED",
                        "isDraft": false,
                        "reviewThreads": {"nodes": []},
                        "commits": {
                            "nodes": [{"commit": {"oid": "o", "statusCheckRollup": null}}]
                        }
                    }]
                }
            }
        });
        let status = parse_response(&[mk_target(0)], &data)[0].2.clone().unwrap();
        assert_eq!(status.pr_number, 5);
        assert_eq!(status.state, PrState::Open);
    }

    #[test]
    fn parse_merged_pr_zeroes_unresolved() {
        let data = json!({
            "q0": {
                "ref": {
                    "associatedPullRequests": {
                        "nodes": [{
                            "number": 7,
                            "url": "https://github.com/x/y/pull/7",
                            "state": "MERGED",
                            "isDraft": false,
                            "reviewThreads": {
                                "nodes": [{"isResolved": false}, {"isResolved": false}]
                            },
                            "commits": {
                                "nodes": [{"commit": {"oid": "z", "statusCheckRollup": null}}]
                            }
                        }]
                    }
                }
            }
        });
        let status = parse_response(&[mk_target(0)], &data)[0]
            .2
            .clone()
            .expect("parse");
        assert_eq!(status.state, PrState::Merged);
        assert_eq!(status.unresolved_threads, 0);
        assert_eq!(status.checks, ChecksRollup::None);
    }

    #[test]
    fn parse_null_rollup_maps_to_none() {
        let data = json!({
            "q0": {
                "ref": {
                    "associatedPullRequests": {
                        "nodes": [{
                            "number": 1,
                            "url": "u",
                            "state": "OPEN",
                            "isDraft": true,
                            "reviewThreads": {"nodes": []},
                            "commits": {
                                "nodes": [{"commit": {"oid": "o", "statusCheckRollup": null}}]
                            }
                        }]
                    }
                }
            }
        });
        let status = parse_response(&[mk_target(0)], &data)[0]
            .2
            .clone()
            .expect("parse");
        assert_eq!(status.checks, ChecksRollup::None);
        assert!(status.is_draft);
    }

    #[test]
    fn is_meaningful_change_ignores_fetched_at() {
        let base = GithubPrStatus {
            pr_number: 1,
            url: "u".into(),
            state: PrState::Open,
            is_draft: false,
            checks: ChecksRollup::Success,
            bugbot: ChecksRollup::None,
            review_decision: ReviewDecision::None,
            unresolved_threads: 0,
            head_sha: "sha".into(),
            fetched_at: Utc::now(),
            last_error: None,
        };
        let mut later = base.clone();
        later.fetched_at = Utc::now() + chrono::Duration::seconds(60);
        assert!(!is_meaningful_change(Some(&base), Some(&later)));

        let mut changed = base.clone();
        changed.unresolved_threads = 1;
        assert!(is_meaningful_change(Some(&base), Some(&changed)));

        let mut approved = base.clone();
        approved.review_decision = ReviewDecision::Approved;
        assert!(is_meaningful_change(Some(&base), Some(&approved)));
    }

    #[test]
    fn parse_splits_bugbot_from_checks_rollup() {
        // Bugbot says FAILURE, the rest of CI is SUCCESS. The top-level rollup
        // would be FAILURE, but `checks` should reflect non-bugbot only.
        let data = json!({
            "q0": {
                "ref": {
                    "associatedPullRequests": {
                        "nodes": [{
                            "number": 1,
                            "url": "u",
                            "state": "OPEN",
                            "isDraft": false,
                            "reviewThreads": {"nodes": []},
                            "commits": {
                                "nodes": [{"commit": {
                                    "oid": "o",
                                    "statusCheckRollup": {
                                        "state": "FAILURE",
                                        "contexts": {"nodes": [
                                            {"__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS"},
                                            {"__typename": "CheckRun", "name": "test", "status": "COMPLETED", "conclusion": "SUCCESS"},
                                            {"__typename": "CheckRun", "name": "Cursor Bugbot", "status": "COMPLETED", "conclusion": "FAILURE"}
                                        ]}
                                    }
                                }}]
                            }
                        }]
                    }
                }
            }
        });
        let s = parse_response(&[mk_target(0)], &data)[0].2.clone().unwrap();
        assert_eq!(s.checks, ChecksRollup::Success);
        assert_eq!(s.bugbot, ChecksRollup::Failure);
    }

    #[test]
    fn parse_bugbot_pending_when_in_progress() {
        let data = json!({
            "q0": {
                "ref": {
                    "associatedPullRequests": {
                        "nodes": [{
                            "number": 1,
                            "url": "u",
                            "state": "OPEN",
                            "isDraft": false,
                            "reviewThreads": {"nodes": []},
                            "commits": {
                                "nodes": [{"commit": {
                                    "oid": "o",
                                    "statusCheckRollup": {
                                        "state": "PENDING",
                                        "contexts": {"nodes": [
                                            {"__typename": "CheckRun", "name": "Cursor Bugbot", "status": "IN_PROGRESS", "conclusion": null}
                                        ]}
                                    }
                                }}]
                            }
                        }]
                    }
                }
            }
        });
        let s = parse_response(&[mk_target(0)], &data)[0].2.clone().unwrap();
        assert_eq!(s.bugbot, ChecksRollup::Pending);
        assert_eq!(s.checks, ChecksRollup::None);
    }

    #[test]
    fn parse_falls_back_to_top_level_rollup_when_no_contexts() {
        // Older fixture shape — no contexts list. We still get the legacy rollup
        // for `checks`, and `bugbot` falls through to None.
        let data = json!({
            "q0": {
                "ref": {
                    "associatedPullRequests": {
                        "nodes": [{
                            "number": 1,
                            "url": "u",
                            "state": "OPEN",
                            "isDraft": false,
                            "reviewThreads": {"nodes": []},
                            "commits": {
                                "nodes": [{"commit": {"oid": "o", "statusCheckRollup": {"state": "SUCCESS"}}}]
                            }
                        }]
                    }
                }
            }
        });
        let s = parse_response(&[mk_target(0)], &data)[0].2.clone().unwrap();
        assert_eq!(s.checks, ChecksRollup::Success);
        assert_eq!(s.bugbot, ChecksRollup::None);
    }

    #[test]
    fn parse_review_decision_open() {
        let data = json!({
            "q0": {
                "ref": {
                    "associatedPullRequests": {
                        "nodes": [{
                            "number": 1,
                            "url": "u",
                            "state": "OPEN",
                            "isDraft": false,
                            "reviewDecision": "APPROVED",
                            "reviewThreads": {"nodes": []},
                            "commits": {
                                "nodes": [{"commit": {"oid": "o", "statusCheckRollup": {"state": "SUCCESS"}}}]
                            }
                        }]
                    }
                }
            }
        });
        let s = parse_response(&[mk_target(0)], &data)[0].2.clone().unwrap();
        assert_eq!(s.review_decision, ReviewDecision::Approved);
    }

    #[test]
    fn parse_review_decision_null_maps_to_none() {
        let data = json!({
            "q0": {
                "ref": {
                    "associatedPullRequests": {
                        "nodes": [{
                            "number": 1,
                            "url": "u",
                            "state": "OPEN",
                            "isDraft": false,
                            "reviewDecision": null,
                            "reviewThreads": {"nodes": []},
                            "commits": {
                                "nodes": [{"commit": {"oid": "o", "statusCheckRollup": null}}]
                            }
                        }]
                    }
                }
            }
        });
        let s = parse_response(&[mk_target(0)], &data)[0].2.clone().unwrap();
        assert_eq!(s.review_decision, ReviewDecision::None);
    }

    #[test]
    fn parse_review_decision_zero_on_merged() {
        let data = json!({
            "q0": {
                "ref": {
                    "associatedPullRequests": {
                        "nodes": [{
                            "number": 1,
                            "url": "u",
                            "state": "MERGED",
                            "isDraft": false,
                            "reviewDecision": "APPROVED",
                            "reviewThreads": {"nodes": []},
                            "commits": {
                                "nodes": [{"commit": {"oid": "o", "statusCheckRollup": null}}]
                            }
                        }]
                    }
                }
            }
        });
        let s = parse_response(&[mk_target(0)], &data)[0].2.clone().unwrap();
        assert_eq!(s.review_decision, ReviewDecision::None);
    }

    #[test]
    fn backoff_progression() {
        assert_eq!(backoff_for(0), BASE_INTERVAL);
        assert_eq!(backoff_for(1), Duration::from_secs(90));
        assert_eq!(backoff_for(2), Duration::from_secs(180));
        assert!(backoff_for(10) <= MAX_BACKOFF);
    }
}
