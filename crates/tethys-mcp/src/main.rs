//! MCP server companion binary — the tools a session gets over Tethys itself.
//!
//! Claude Code spawns one of these per session, from the `--mcp-config` Tethys
//! renders at spawn time. It exposes `link_pr` — point the current workspace at
//! a pull request — and forwards each call to the running Tethys app over
//! `mcp.sock`.
//!
//! Two things about this process are worth knowing:
//!
//! 1. **stdout belongs to the protocol.** Nothing may print there. Diagnostics
//!    go to stderr, where Claude collects them.
//! 2. **Failures are loud.** Its sibling `tethys-hook` exits 0 no matter what,
//!    because a broken hook must never disturb a session. Here the opposite
//!    holds: an agent told its PR is on the workspace row when it isn't will
//!    move on believing the work is visible.

use std::borrow::Cow;
use std::env;
use std::path::PathBuf;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    Implementation, JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::io::stdio;
use rmcp::{ErrorData, RoleServer, ServiceExt};
use serde::Deserialize;
use serde_json::json;
use tokio::net::UnixStream;

use tethys_mcp::{
    read_frame, write_frame, LinkPr, Request, Response, ENV_REPO_KEYS, ENV_SESSION_ID, ENV_SOCKET,
    ENV_WORKSPACE_ID, TOOL_LINK_PR,
};

/// What the calling agent supplies to `link_pr`. The workspace it lands on
/// comes from the environment, so an agent can only ever link to its own.
#[derive(Debug, Deserialize)]
struct LinkPrArgs {
    reference: String,
    #[serde(default)]
    repo_key: Option<String>,
}

/// The server: a socket to talk to, an identity to stamp on requests, and the
/// repo keys that make up the tool's `repo_key` enum.
#[derive(Debug, Clone)]
struct TethysServer {
    socket: PathBuf,
    from_workspace: String,
    from_session: Option<String>,
    repo_keys: Vec<String>,
}

impl TethysServer {
    /// Read the config Tethys baked into our environment. A missing socket or
    /// workspace id means we were launched by something other than Tethys, and
    /// there is nothing useful we could do.
    fn from_env() -> anyhow::Result<Self> {
        let socket = env::var(ENV_SOCKET).map_err(|_| anyhow::anyhow!("{ENV_SOCKET} is not set"))?;
        let from_workspace = env::var(ENV_WORKSPACE_ID)
            .map_err(|_| anyhow::anyhow!("{ENV_WORKSPACE_ID} is not set"))?;
        let repo_keys = env::var(ENV_REPO_KEYS)
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        Ok(Self {
            socket: PathBuf::from(socket),
            from_workspace,
            from_session: env::var(ENV_SESSION_ID).ok().filter(|s| !s.is_empty()),
            repo_keys,
        })
    }

    /// `link_pr`'s input schema, built at runtime so `repo_key` can enumerate
    /// the registry — an agent then cannot name a repo Tethys has never heard
    /// of. It stays optional: naming a repo is only ever disambiguation, and a
    /// workspace with one GitHub repo — or a pasted URL that names its own —
    /// needs none.
    fn link_pr_schema(&self) -> JsonObject {
        let mut repo_key = json!({
            "type": "string",
            "description": "Which of this workspace's repos the PR belongs to. \
                Only needed when the workspace spans more than one GitHub repo \
                and you are passing a bare number.",
        });
        if !self.repo_keys.is_empty() {
            repo_key["enum"] = json!(self.repo_keys);
        }
        let schema = json!({
            "type": "object",
            "properties": {
                "reference": {
                    "type": "string",
                    "description": "The pull request: a full GitHub URL, \
                        `owner/repo#123`, or just the number.",
                },
                "repo_key": repo_key,
            },
            "required": ["reference"],
            "additionalProperties": false,
        });
        schema
            .as_object()
            .cloned()
            .expect("input schema literal is an object")
    }

    fn link_pr_tool(&self) -> Tool {
        Tool::new(
            Cow::Borrowed(TOOL_LINK_PR),
            Cow::Borrowed(
                "Show a pull request on the Tethys workspace this session belongs \
                 to, so its state — CI, reviews, conflicts — appears on the row \
                 for this work without anyone going looking for it.\n\n\
                 Call it right after you open a PR. Tethys finds the PR for the \
                 workspace's own branch by itself, so the case this exists for is \
                 a PR you opened from some other branch in the same worktree — a \
                 stacked PR, a follow-up, a fix cut off the base branch. Calling \
                 it for the branch PR anyway is harmless and simply makes it \
                 appear sooner.\n\n\
                 The PR must already exist on GitHub: this records it, it does \
                 not create or modify anything. Fails if the reference doesn't \
                 resolve or the repo isn't one this workspace spans.",
            ),
            self.link_pr_schema(),
        )
    }

    /// The `tools/list` reply.
    ///
    /// `ttl_ms` and `cache_scope` are not optional in practice. Claude Code
    /// negotiates a protocol version that requires `ttlMs` on a paginated
    /// result, and rmcp's `with_all_items` leaves it out — a reply without it
    /// is rejected and retried until the client gives up with "tools fetch
    /// failed". A ttl of 0 is the honest value: the `repo_key` enum is baked in
    /// when Tethys spawns this process, so a cached list must not outlive the
    /// session it was built for.
    fn tools_result(&self) -> ListToolsResult {
        ListToolsResult::with_all_items(vec![self.link_pr_tool()])
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Private)
    }

    /// One request, one connection, one reply. Short-lived like the hook's, but
    /// this one waits for an answer.
    async fn send(&self, request: &Request) -> anyhow::Result<Response> {
        let mut stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|e| anyhow::anyhow!("could not reach Tethys at {}: {e}", self.socket.display()))?;
        write_frame(&mut stream, request).await?;
        let response: Response = read_frame(&mut stream).await?;
        Ok(response)
    }
}

impl ServerHandler for TethysServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("tethys", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Tethys manages parallel Claude sessions across git worktrees. \
                 Use link_pr to put a pull request you opened onto this \
                 workspace's row, where its CI and review state are visible.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(self.tools_result())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        match request.name.as_ref() {
            TOOL_LINK_PR => self.link_pr(request.arguments).await,
            other => Err(ErrorData::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }
}

/// The call: parse the agent's arguments, stamp the identity from the
/// environment onto them, and turn whatever comes back into words.
///
/// Everything past the parse is a *tool-level* error rather than a protocol
/// one. A protocol error can be swallowed by the client; the agent has to read
/// the reason it didn't get what it asked for, or it will assume it did.
impl TethysServer {
    async fn link_pr(&self, arguments: Option<JsonObject>) -> Result<CallToolResponse, ErrorData> {
        let args: LinkPrArgs = parse_args(arguments)?;
        let request = Request::LinkPr(LinkPr {
            from_workspace: self.from_workspace.clone(),
            from_session: self.from_session.clone(),
            repo_key: args.repo_key,
            reference: args.reference,
        });

        let response = match self.send(&request).await {
            Ok(response) => response,
            Err(e) => return Ok(failed(format!("link failed, nothing was linked: {e}"))),
        };

        Ok(match response {
            Response::Linked {
                owner,
                name,
                number,
                url,
                already_attached,
            } => {
                let note = if already_attached {
                    "It was already on this workspace, so nothing changed"
                } else {
                    "Tethys polls it from here on, so its CI and review state show up \
                     on the workspace row"
                };
                CallToolResult::success(vec![ContentBlock::text(format!(
                    "Linked {owner}/{name}#{number} ({url}) to this workspace. {note}."
                ))])
                .into()
            }
            Response::Rejected { message } => {
                failed(format!("link refused, nothing was linked: {message}"))
            }
        })
    }
}

/// Deserialize a tool call's arguments. The one place a bad call is a protocol
/// error: the agent sent something the schema said it couldn't.
fn parse_args<T: serde::de::DeserializeOwned>(
    arguments: Option<JsonObject>,
) -> Result<T, ErrorData> {
    serde_json::from_value(serde_json::Value::Object(arguments.unwrap_or_default()))
        .map_err(|e| ErrorData::invalid_params(format!("bad arguments: {e}"), None))
}

/// A tool-level failure: an error the agent reads, not one the client eats.
fn failed(message: String) -> CallToolResponse {
    CallToolResult::error(vec![ContentBlock::text(message)]).into()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let server = TethysServer::from_env()?;
    let running = server.serve(stdio()).await?;
    running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> TethysServer {
        TethysServer {
            socket: PathBuf::from("/tmp/mcp.sock"),
            from_workspace: "ws-1".into(),
            from_session: Some("sess-1".into()),
            repo_keys: vec!["frontend".into(), "backend".into()],
        }
    }

    /// Regression: without `ttlMs`, Claude Code rejects the reply outright and
    /// the tool never appears — it reports "tools fetch failed" after retrying.
    /// Nothing in the type system asks for this field, so only a test holds it.
    #[test]
    fn the_tools_reply_carries_a_freshness_ttl() {
        let raw = serde_json::to_value(server().tools_result()).expect("serialize");
        assert_eq!(raw["ttlMs"], 0, "reply was {raw}");
        assert_eq!(raw["cacheScope"], "private");
    }

    /// Only the reference is required: a workspace with one GitHub repo, or a
    /// pasted URL that names its own, gives Tethys enough to resolve the rest.
    #[test]
    fn link_pr_requires_only_the_reference() {
        let schema = serde_json::to_value(server().link_pr_schema()).expect("serialize");
        assert_eq!(schema["required"], serde_json::json!(["reference"]));
        assert_eq!(
            schema["properties"]["repo_key"]["enum"],
            serde_json::json!(["frontend", "backend"])
        );
    }

    /// An empty registry must not render an `enum` that nothing can satisfy.
    #[test]
    fn an_empty_registry_leaves_the_enum_out() {
        let mut server = server();
        server.repo_keys.clear();
        let schema = serde_json::to_value(server.link_pr_schema()).expect("serialize");
        assert!(schema["properties"]["repo_key"]["enum"].is_null());
        assert_eq!(schema["properties"]["repo_key"]["type"], "string");
    }

    #[test]
    fn the_tools_reply_carries_the_link_tool() {
        let raw = serde_json::to_value(server().tools_result()).expect("serialize");
        let names: Vec<&str> = raw["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|t| t["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, vec![TOOL_LINK_PR]);
    }
}
