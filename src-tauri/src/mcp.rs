//! Both halves of the seam in front of the Tethys MCP server.
//!
//! [`McpLaunch`] is the spawn side: it renders the `--mcp-config` a Claude
//! session is launched with. [`listen`] is the receiving side: the socket that
//! config points at.
//!
//! The two are here together because they are one contract read from opposite
//! ends — the config names a binary, a socket and an identity, and the listener
//! is what answers on the other end of it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use tauri::{AppHandle, Emitter};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, error, info, warn};

use crate::error::{AppError, AppResult};
use crate::github::{parse_pr_reference, GithubPoller, GithubPrStatus, GithubSlug, PrReference};
use crate::paths::Paths;
use crate::inprogress::InProgressWorkspaces;
use crate::registry::RegistryLoad;
use crate::state::ManualPr;
use crate::store::Store;

pub use tethys_mcp::{
    DescribeWorkspace, GivePrOwnWorkspace, LinkPr, PrView, RepoView, Request, Response,
    SetWorktreeRef, WorkspaceView,
};

/// Everything needed to render a session's `--mcp-config`, resolved once at
/// boot: the companion binary, the socket it should dial, and the registry repo
/// keys that become the tool's `repo_key` enum.
#[derive(Debug, Clone)]
pub struct McpLaunch {
    server_bin: PathBuf,
    socket: PathBuf,
    repo_keys: Vec<String>,
}

impl McpLaunch {
    /// `None` when the companion binary isn't sitting next to the app. A
    /// session then spawns without the Tethys tools, which is still a session
    /// that works — so this is a warning, not a startup failure.
    pub fn resolve(paths: &Paths, registry: &RegistryLoad) -> Option<Self> {
        let server_bin = match crate::paths::tethys_mcp_bin() {
            Ok(p) if p.exists() => p,
            Ok(p) => {
                warn!(
                    path = %p.display(),
                    "tethys-mcp binary not found — sessions can't link PRs"
                );
                return None;
            }
            Err(e) => {
                warn!(error = %e, "could not resolve tethys-mcp path");
                return None;
            }
        };
        // An empty registry leaves the enum out of the schema, which is the
        // honest rendering of "there is nothing to pick from".
        let repo_keys = registry
            .require()
            .map(|reg| reg.repos.iter().map(|r| r.key.clone()).collect())
            .unwrap_or_default();
        Some(Self {
            server_bin,
            socket: paths.mcp_socket(),
            repo_keys,
        })
    }

    /// The `claude` flags that put the Tethys tools in a session's hands.
    ///
    /// `--mcp-config` takes JSON inline, so nothing is written to disk. The
    /// calling identity rides in the server's `env` block rather than being
    /// passed as a tool argument, which is what makes it trustworthy: the agent
    /// never gets to say which workspace it is acting on.
    ///
    /// Deliberately no `--strict-mcp-config` — that would cut the session off
    /// from every other MCP server configured on this machine.
    ///
    /// Both flags are spelled `--flag=value` rather than `--flag value`. Both
    /// are variadic in `claude --help` (`<configs...>`, `<tools...>`), and a
    /// variadic flag eats every following argument that isn't itself a flag —
    /// which would silently swallow a trailing positional prompt. The `=` form
    /// takes exactly one value and stops.
    pub fn claude_args(&self, workspace_id: &str, session_id: &str) -> Vec<String> {
        vec![
            format!("--mcp-config={}", self.config_json(workspace_id, session_id)),
            format!("--allowed-tools={}", tethys_mcp::ALLOWED_TOOLS.join(",")),
        ]
    }

    fn config_json(&self, workspace_id: &str, session_id: &str) -> String {
        json!({
            "mcpServers": {
                tethys_mcp::SERVER_NAME: {
                    "command": self.server_bin,
                    "env": {
                        tethys_mcp::ENV_SOCKET: self.socket,
                        tethys_mcp::ENV_WORKSPACE_ID: workspace_id,
                        tethys_mcp::ENV_SESSION_ID: session_id,
                        tethys_mcp::ENV_REPO_KEYS: self.repo_keys.join(","),
                    },
                },
            },
        })
        .to_string()
    }
}

/// What the socket can reach — one field per thing an agent is allowed to ask
/// Tethys for. Bundled rather than passed loose so a second tool costs a field
/// here and nothing at the call site.
#[derive(Clone)]
pub struct McpServices {
    pub app: AppHandle,
    pub store: Arc<Store>,
    pub registry: Arc<RegistryLoad>,
    pub poller: Arc<GithubPoller>,
    pub paths: Paths,
    pub in_progress: InProgressWorkspaces,
}

/// Bind `mcp.sock` and spawn an accept loop. If the socket already exists (a
/// prior run died without cleanup) it's removed first.
pub async fn listen(socket_path: &Path, services: McpServices) -> AppResult<()> {
    if socket_path.exists() {
        tokio::fs::remove_file(socket_path).await.ok();
    }
    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let listener = UnixListener::bind(socket_path)?;
    info!(path = %socket_path.display(), "mcp socket listening");

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let services = services.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_connection(stream, services).await {
                            warn!(error = %e, "mcp connection error");
                        }
                    });
                }
                Err(e) => error!(error = %e, "mcp accept failed"),
            }
        }
    });

    Ok(())
}

/// One request, one reply, then the peer hangs up.
///
/// A rejection is a reply, not a dropped connection: the calling agent has to
/// be told, in words, that nothing happened.
async fn serve_connection(mut stream: UnixStream, services: McpServices) -> AppResult<()> {
    let request: Request = tethys_mcp::read_frame(&mut stream).await?;
    let response = match request {
        Request::LinkPr(req) => link_pr(&services, req).await,
        Request::DescribeWorkspace(req) => describe_workspace(&services, req).await,
        Request::GivePrOwnWorkspace(req) => give_pr_own_workspace(&services, req).await,
        Request::SetWorktreeRef(req) => set_worktree_ref(&services, req).await,
    };
    tethys_mcp::write_frame(&mut stream, &response).await?;
    Ok(())
}

/// The same door the attach dialog goes through — the agent supplies only the
/// reference, and the workspace comes off the connection's baked-in identity.
async fn link_pr(services: &McpServices, req: LinkPr) -> Response {
    debug!(
        from_workspace = %req.from_workspace,
        from_session = ?req.from_session,
        reference = %req.reference,
        "pr link requested"
    );
    match attach(services, &req).await {
        Ok(response) => response,
        Err(e) => {
            warn!(error = %e, "pr link refused");
            Response::Rejected {
                message: e.to_string(),
            }
        }
    }
}

async fn attach(services: &McpServices, req: &LinkPr) -> AppResult<Response> {
    let parsed = parse_pr_reference(&req.reference).ok_or_else(|| {
        AppError::Other(format!(
            "could not read '{}' as a pull request. Use a GitHub URL, \
             owner/repo#123, or just the number.",
            req.reference
        ))
    })?;

    // A bare number is only meaningful against one of the workspace's own
    // repos, so resolving it doubles as the check that the agent is pointing
    // at a repo this workspace actually spans.
    let slug = match &parsed {
        PrReference::Qualified(slug, _) => slug.clone(),
        PrReference::Number(_) => {
            resolve_repo_slug(services, &req.from_workspace, req.repo_key.as_deref()).await?
        }
    };
    let number = match parsed {
        PrReference::Qualified(_, n) => n,
        PrReference::Number(n) => n,
    };

    // Record which repo the PR belongs to. Named by the agent, or worked out by
    // matching the slug against the registry — either way it is only stored
    // once we know the workspace actually spans that repo.
    let repo_key = match req.repo_key.clone() {
        Some(key) => Some(key),
        None => infer_repo_key(services, &req.from_workspace, &slug).await,
    };

    let already_attached = services
        .store
        .mutate(|s| {
            let ws = s
                .find_workspace_mut(&req.from_workspace)
                .ok_or_else(|| AppError::WorkspaceNotFound(req.from_workspace.clone()))?;
            let already = ws
                .manual_prs
                .iter()
                .any(|p| p.owner == slug.owner && p.name == slug.name && p.number == number);
            if !already {
                ws.manual_prs.push(ManualPr {
                    owner: slug.owner.clone(),
                    name: slug.name.clone(),
                    number,
                    repo_key: repo_key.clone(),
                    github: None,
                });
            }
            Ok(already)
        })
        .await?;

    info!(
        id = %req.from_workspace,
        owner = %slug.owner,
        name = %slug.name,
        number = number,
        already_attached,
        "linked PR to workspace via mcp"
    );
    services.poller.request_tick().await;
    let _ = services.app.emit(
        "workspace:changed",
        json!({ "workspace_id": req.from_workspace }),
    );

    Ok(Response::Linked {
        url: format!(
            "https://github.com/{}/{}/pull/{}",
            slug.owner, slug.name, number
        ),
        owner: slug.owner,
        name: slug.name,
        number,
        already_attached,
    })
}

/// Which repo a bare PR number belongs to. Named explicitly when the workspace
/// spans several; inferred when there is only one GitHub-linked repo to pick.
async fn resolve_repo_slug(
    services: &McpServices,
    workspace_id: &str,
    repo_key: Option<&str>,
) -> AppResult<GithubSlug> {
    let repo_keys: Vec<String> = services
        .store
        .read(|s| {
            s.find_workspace(workspace_id)
                .map(|ws| ws.repo_links.iter().map(|l| l.repo_key.clone()).collect())
                .unwrap_or_default()
        })
        .await;
    if repo_keys.is_empty() {
        return Err(AppError::WorkspaceNotFound(workspace_id.to_string()));
    }

    let reg = services.registry.require()?;

    if let Some(key) = repo_key {
        if !repo_keys.iter().any(|k| k == key) {
            return Err(AppError::Other(format!(
                "this workspace doesn't span repo '{key}'. It has: {}.",
                repo_keys.join(", ")
            )));
        }
        return reg
            .find_repo(key)
            .and_then(|r| r.github_slug.clone())
            .ok_or_else(|| AppError::Other(format!("repo '{key}' has no GitHub remote")));
    }

    // No repo named — infer, but only when the choice is unambiguous.
    let candidates: Vec<(String, GithubSlug)> = repo_keys
        .iter()
        .filter_map(|k| {
            reg.find_repo(k)
                .and_then(|r| r.github_slug.clone())
                .map(|slug| (k.clone(), slug))
        })
        .collect();
    match candidates.len() {
        0 => Err(AppError::Other(
            "none of this workspace's repos have a GitHub remote".into(),
        )),
        1 => Ok(candidates.into_iter().next().expect("len checked").1),
        _ => Err(AppError::Other(format!(
            "this workspace spans several GitHub repos ({}), so a bare number \
             is ambiguous. Pass repo_key, or use a full URL.",
            candidates
                .into_iter()
                .map(|(k, _)| k)
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Provision a workspace of its own for a PR the caller's workspace tracks.
///
/// Resolving the PR against tracked state rather than GitHub is deliberate:
/// the head branch has to come from somewhere, and requiring the PR to be
/// tracked means an agent can only ever act on a PR already associated with
/// its own workspace.
async fn give_pr_own_workspace(services: &McpServices, req: GivePrOwnWorkspace) -> Response {
    debug!(
        from_workspace = %req.from_workspace,
        reference = %req.reference,
        "pr workspace requested"
    );
    match provision_for_pr(services, &req).await {
        Ok(response) => response,
        Err(e) => {
            warn!(error = %e, "pr workspace refused");
            Response::Rejected {
                message: e.to_string(),
            }
        }
    }
}

async fn provision_for_pr(
    services: &McpServices,
    req: &GivePrOwnWorkspace,
) -> AppResult<Response> {
    let parsed = parse_pr_reference(&req.reference).ok_or_else(|| {
        AppError::Other(format!(
            "could not read '{}' as a pull request. Use a GitHub URL, \
             owner/repo#123, or just the number.",
            req.reference
        ))
    })?;
    let number = match parsed {
        PrReference::Qualified(_, n) => n,
        PrReference::Number(n) => n,
    };

    let Some(caller) = services
        .store
        .read(|s| s.find_workspace(&req.from_workspace).cloned())
        .await
    else {
        return Err(AppError::WorkspaceNotFound(req.from_workspace.clone()));
    };

    // The head branch is the whole of what a new workspace needs, and it only
    // exists on a polled status — so the PR has to be one Tethys is tracking.
    let branch = caller
        .manual_prs
        .iter()
        .filter(|p| p.number == number)
        .find_map(|p| p.github.as_ref().and_then(|g| g.head_branch.clone()))
        .or_else(|| {
            caller.repo_links.iter().find_map(|l| {
                l.github
                    .as_ref()
                    .filter(|g| g.pr_number == number)
                    .and_then(|g| g.head_branch.clone())
            })
        })
        .ok_or_else(|| {
            AppError::Other(format!(
                "this workspace isn't tracking a PR #{number} with a known branch. \
                 Link it first, or wait for the next poll if you just linked it."
            ))
        })?;

    if branch == caller.branch {
        return Err(AppError::Other(format!(
            "PR #{number} is opened from `{branch}`, which is this workspace's own \
             branch — it already has a worktree here."
        )));
    }

    // Only one worktree may hold a branch. Detach whoever has it; their files
    // stay on the same commit, they just stop owning the name.
    let mut freed_from = None;
    let reg = services.registry.require()?;
    for repo in &reg.repos {
        let clone_path = services.paths.repo_clone_path(&repo.key);
        if !clone_path.exists() {
            continue;
        }
        if let Some(holder) = crate::git::worktree_holding_branch(&clone_path, &branch).await? {
            let name = holder
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| holder.display().to_string());
            // Refuse by default. Detaching is harmless to that worktree's
            // files but takes it off its branch, and an agent should decide to
            // do that deliberately — and be able to mention it first.
            if !req.take_branch {
                return Err(AppError::Other(format!(
                    "`{branch}` is checked out in workspace '{name}', and only one \
                     worktree can hold it. Pass take_branch to detach that \
                     worktree — it keeps its files at the same commit, but stops \
                     being on a branch."
                )));
            }
            crate::git::detach_worktree(&holder).await?;
            freed_from = Some(name);
            info!(branch = %branch, holder = %holder.display(), "detached to free branch for pr workspace");
            break;
        }
    }

    // Mirror the caller's repo set, so the new workspace can run the same stack.
    let repos: Vec<String> = caller.repo_links.iter().map(|l| l.repo_key.clone()).collect();
    if repos.is_empty() {
        return Err(AppError::Other(
            "this workspace has no repos to mirror".into(),
        ));
    }

    let workspace_id = uuid::Uuid::new_v4().to_string();
    let args = crate::commands::CreateWorkspaceArgs {
        workspace_id: workspace_id.clone(),
        branch: branch.clone(),
        repo_selections: repos.clone(),
        claude_binary: caller.claude_binary.clone(),
    };

    // Provisioning takes minutes; the caller is told it started, not that it
    // finished. Failures land on the row as CreationFailed, where they're the
    // user's to see — the agent has already moved on.
    let services = services.clone();
    let id_for_log = workspace_id.clone();
    tauri::async_runtime::spawn(async move {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        if let Err(e) = crate::commands::provision_workspace(
            &services.app,
            &services.store,
            &services.registry,
            &services.paths,
            &services.in_progress,
            args,
            crate::job::JobTx(tx),
        )
        .await
        {
            warn!(id = %id_for_log, error = %e, "mcp-requested workspace failed to provision");
        }
    });

    Ok(Response::WorkspaceCreated {
        workspace_id,
        branch,
        repos,
        freed_from,
    })
}

/// Move one of the caller's worktrees to a ref.
///
/// The build reads whatever is on disk, so this plus `Build local` is how a
/// stacked change gets built: put the backend worktree at the tip of the
/// backend PRs you want included and leave the frontend where it is.
async fn set_worktree_ref(services: &McpServices, req: SetWorktreeRef) -> Response {
    debug!(
        from_workspace = %req.from_workspace,
        repo_key = %req.repo_key,
        git_ref = %req.git_ref,
        "worktree move requested"
    );
    match move_worktree(services, &req).await {
        Ok(response) => response,
        Err(e) => {
            warn!(error = %e, "worktree move refused");
            Response::Rejected {
                message: e.to_string(),
            }
        }
    }
}

async fn move_worktree(services: &McpServices, req: &SetWorktreeRef) -> AppResult<Response> {
    let git_ref = req.git_ref.trim();
    if git_ref.is_empty() {
        return Err(AppError::Other("git_ref is required".into()));
    }

    let worktree = services
        .store
        .read(|s| {
            s.find_workspace(&req.from_workspace).and_then(|w| {
                w.repo_links
                    .iter()
                    .find(|l| l.repo_key == req.repo_key)
                    .map(|l| l.worktree_path.clone())
            })
        })
        .await
        .ok_or_else(|| {
            AppError::Other(format!(
                "this workspace has no '{}' worktree",
                req.repo_key
            ))
        })?;

    if !worktree.exists() {
        return Err(AppError::Other(format!(
            "the '{}' worktree is missing at {}",
            req.repo_key,
            worktree.display()
        )));
    }

    // A PR branch usually only exists on the remote from this worktree's point
    // of view, so fetch before trying to check it out.
    crate::git::fetch_ref_best_effort(&worktree, git_ref).await;
    let landed = crate::git::checkout_ref(&worktree, git_ref).await?;

    info!(
        workspace = %req.from_workspace,
        repo_key = %req.repo_key,
        git_ref = %git_ref,
        branch = ?landed.branch,
        commit = %landed.commit,
        "moved worktree"
    );
    let _ = services.app.emit(
        "workspace:changed",
        json!({ "workspace_id": req.from_workspace }),
    );

    Ok(Response::WorktreeMoved {
        repo_key: req.repo_key.clone(),
        worktree_path: worktree.display().to_string(),
        branch: landed.branch,
        commit: landed.commit,
        detached_because_held_by: landed
            .detached_because_held_by
            .map(|p| {
                p.parent()
                    .and_then(|q| q.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.display().to_string())
            }),
    })
}

/// Which of the workspace's repos carries this slug, if any. Used to record a
/// repo for a PR the agent didn't name one for.
async fn infer_repo_key(
    services: &McpServices,
    workspace_id: &str,
    slug: &GithubSlug,
) -> Option<String> {
    let repo_keys: Vec<String> = services
        .store
        .read(|s| {
            s.find_workspace(workspace_id)
                .map(|ws| ws.repo_links.iter().map(|l| l.repo_key.clone()).collect())
                .unwrap_or_default()
        })
        .await;
    let reg = services.registry.require().ok()?;
    repo_keys.into_iter().find(|key| {
        reg.find_repo(key)
            .and_then(|r| r.github_slug.as_ref())
            .is_some_and(|s| s == slug)
    })
}

/// Everything an agent may know about the workspace it is running in. Scoped to
/// the caller's own workspace — the id comes off the connection's baked-in
/// identity, so this can't be turned into a way to read the others.
async fn describe_workspace(services: &McpServices, req: DescribeWorkspace) -> Response {
    debug!(
        from_workspace = %req.from_workspace,
        from_session = ?req.from_session,
        "workspace description requested"
    );

    let Some(snapshot) = services
        .store
        .read(|s| s.find_workspace(&req.from_workspace).cloned())
        .await
    else {
        return Response::Rejected {
            message: format!("workspace {} is not in state", req.from_workspace),
        };
    };

    // The workspace root is the parent every repo worktree sits under.
    let root = snapshot
        .repo_links
        .first()
        .and_then(|l| l.worktree_path.parent())
        .map(|p| p.display().to_string());

    let mut repos = Vec::with_capacity(snapshot.repo_links.len());
    for link in &snapshot.repo_links {
        let slug = services
            .registry
            .require()
            .ok()
            .and_then(|reg| reg.find_repo(&link.repo_key).and_then(|r| r.github_slug.clone()));
        repos.push(RepoView {
            repo_key: link.repo_key.clone(),
            worktree_path: link.worktree_path.display().to_string(),
            current_branch: current_branch(&link.worktree_path).await,
            branch_pr: link.github.as_ref().and_then(|status| {
                slug.as_ref()
                    .map(|slug| pr_view(slug, status, Some(link.repo_key.clone())))
            }),
        });
    }

    let attached_prs = snapshot
        .manual_prs
        .iter()
        .map(|pr| match &pr.github {
            Some(status) => pr_view(
                &GithubSlug {
                    owner: pr.owner.clone(),
                    name: pr.name.clone(),
                },
                status,
                pr.repo_key.clone(),
            ),
            // Attached but not yet polled — report it rather than hide it, or
            // an agent would re-link a PR that is already there.
            None => PrView {
                owner: pr.owner.clone(),
                name: pr.name.clone(),
                number: pr.number,
                url: format!(
                    "https://github.com/{}/{}/pull/{}",
                    pr.owner, pr.name, pr.number
                ),
                repo_key: pr.repo_key.clone(),
                head_branch: None,
                state: None,
                is_draft: None,
                checks: None,
                review_decision: None,
                unresolved_threads: None,
                has_merge_conflicts: None,
            },
        })
        .collect();

    Response::Described(WorkspaceView {
        workspace_id: snapshot.id.clone(),
        branch: snapshot.branch.clone(),
        root,
        repos,
        attached_prs,
    })
}

/// The branch a worktree is *actually* on. `None` when the directory is gone or
/// git can't answer — which is itself worth reporting, so the caller doesn't
/// assume the worktree is fine.
async fn current_branch(worktree: &Path) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!branch.is_empty()).then_some(branch)
}

/// Flatten a polled status into the wire view. The enums become strings here
/// so the wire crate doesn't have to restate them.
fn pr_view(slug: &GithubSlug, status: &GithubPrStatus, repo_key: Option<String>) -> PrView {
    PrView {
        owner: slug.owner.clone(),
        name: slug.name.clone(),
        number: status.pr_number,
        url: status.url.clone(),
        repo_key,
        head_branch: status.head_branch.clone(),
        state: Some(as_tag(&status.state)),
        is_draft: Some(status.is_draft),
        checks: Some(as_tag(&status.checks)),
        review_decision: Some(as_tag(&status.review_decision)),
        unresolved_threads: Some(status.unresolved_threads),
        has_merge_conflicts: Some(status.has_merge_conflicts),
    }
}

/// Render one of the status enums as its snake_case serde tag, so the wire
/// format tracks the app's own spelling without a second definition.
fn as_tag<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch() -> McpLaunch {
        McpLaunch {
            server_bin: PathBuf::from("/opt/tethys/tethys-mcp"),
            socket: PathBuf::from("/tmp/app/mcp.sock"),
            repo_keys: vec!["frontend".into(), "backend".into()],
        }
    }

    /// The identity has to reach the server through the env block. If it moved
    /// to a tool argument, an agent could claim any workspace it liked.
    #[test]
    fn the_config_carries_the_callers_identity() {
        let raw = launch().config_json("ws-1", "sess-1");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
        let env = &parsed["mcpServers"]["tethys"]["env"];
        assert_eq!(env[tethys_mcp::ENV_WORKSPACE_ID], "ws-1");
        assert_eq!(env[tethys_mcp::ENV_SESSION_ID], "sess-1");
        assert_eq!(env[tethys_mcp::ENV_REPO_KEYS], "frontend,backend");
        assert_eq!(env[tethys_mcp::ENV_SOCKET], "/tmp/app/mcp.sock");
    }

    /// Regression: the space form of these flags is variadic and would swallow
    /// a trailing positional prompt.
    #[test]
    fn the_claude_flags_use_the_equals_form() {
        let args = launch().claude_args("ws-1", "sess-1");
        assert_eq!(args.len(), 2);
        assert!(args[0].starts_with("--mcp-config={"));
        assert_eq!(
            args[1],
            format!("--allowed-tools={}", tethys_mcp::ALLOWED_TOOLS.join(","))
        );
    }
}
