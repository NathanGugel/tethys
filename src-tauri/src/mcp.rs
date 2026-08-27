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
use crate::github::{parse_pr_reference, GithubPoller, GithubSlug, PrReference};
use crate::paths::Paths;
use crate::registry::RegistryLoad;
use crate::state::ManualPr;
use crate::store::Store;

pub use tethys_mcp::{LinkPr, Request, Response};

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
        assert_eq!(args[1], "--allowed-tools=mcp__tethys__link_pr");
    }
}
