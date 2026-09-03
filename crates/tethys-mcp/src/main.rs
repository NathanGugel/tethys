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
    read_frame, write_frame, DescribeWorkspace, GivePrOwnWorkspace, LinkPr, PrView, Request,
    Response, WorkspaceView, ENV_REPO_KEYS, ENV_SESSION_ID, ENV_SOCKET, ENV_WORKSPACE_ID,
    SetWorktreeRef, TOOL_DESCRIBE_WORKSPACE, TOOL_GIVE_PR_OWN_WORKSPACE, TOOL_LINK_PR,
    TOOL_SET_WORKTREE_REF,
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

    /// `describe_workspace` takes no arguments. Which workspace is being asked
    /// about is not the agent's to choose, and there is nothing else to vary.
    fn describe_workspace_tool(&self) -> Tool {
        Tool::new(
            Cow::Borrowed(TOOL_DESCRIBE_WORKSPACE),
            Cow::Borrowed(
                "Report what the Tethys workspace this session belongs to is made \
                 of: each repo, where its worktree is on disk, which branch that \
                 worktree is currently on, and every pull request Tethys is \
                 tracking for it — both the ones it detected from the branches and \
                 the ones attached with link_pr.\n\n\
                 The branch reported per repo is read from git, not from Tethys's \
                 own record, so it tells you what is really checked out. Those can \
                 differ: a worktree may have been moved to another branch by hand, \
                 which is worth noticing before you build on top of it.\n\n\
                 Reach for it when you need to work across the workspace's other \
                 repos, when you need a sibling worktree's path, or to check \
                 whether a PR is already linked before calling link_pr. It reads \
                 only; nothing is changed.",
            ),
            empty_schema(),
        )
    }

    fn give_pr_own_workspace_tool(&self) -> Tool {
        let schema = json!({
            "type": "object",
            "properties": {
                "reference": {
                    "type": "string",
                    "description": "The pull request: a full GitHub URL, \
                        `owner/repo#123`, or just the number.",
                },
                "take_branch": {
                    "type": "boolean",
                    "description": "Set true to detach another workspace's \
                        worktree if it is holding the branch. Leave it out on \
                        the first call: it will refuse and name the workspace \
                        that would be disturbed, which is usually worth \
                        mentioning before you go ahead.",
                },
            },
            "required": ["reference"],
            "additionalProperties": false,
        });
        Tool::new(
            Cow::Borrowed(TOOL_GIVE_PR_OWN_WORKSPACE),
            Cow::Borrowed(
                "Provision a Tethys workspace of its own for a pull request, so \
                 it gets its own worktree per repo and its own dev servers — the \
                 way to actually run a PR that was opened from a branch other \
                 than the one this worktree is on.\n\n\
                 The PR has to be one Tethys already tracks for this workspace, \
                 because that is where its branch comes from; call link_pr first \
                 if it isn't, and describe_workspace to see what is. Only one \
                 worktree may hold a branch at a time, so if another workspace \
                 has it this refuses and names it; pass take_branch to detach \
                 that worktree, which keeps its files at the same commit and \
                 simply stops owning the branch name.\n\n\
                 Provisioning (worktrees, dependency install, setup scripts) \
                 takes minutes and runs in the background: this returns once the \
                 workspace is accepted, not when it is ready.",
            ),
            schema.as_object().cloned().expect("schema literal is an object"),
        )
    }

    /// `set_worktree_ref`'s schema. `repo_key` enumerates the registry for the
    /// same reason the other tools' do — a repo that doesn't exist should not
    /// be expressible.
    fn set_worktree_ref_tool(&self) -> Tool {
        let mut repo_key = json!({
            "type": "string",
            "description": "Which of this workspace's repos to move.",
        });
        if !self.repo_keys.is_empty() {
            repo_key["enum"] = json!(self.repo_keys);
        }
        let schema = json!({
            "type": "object",
            "properties": {
                "repo_key": repo_key,
                "git_ref": {
                    "type": "string",
                    "description": "Branch, remote ref, or commit to check out. \
                        Prefer `origin/<branch>` for a PR branch — that is what \
                        is actually pushed, and it sidesteps the branch being \
                        held by another worktree.",
                },
            },
            "required": ["repo_key", "git_ref"],
            "additionalProperties": false,
        });
        Tool::new(
            Cow::Borrowed(TOOL_SET_WORKTREE_REF),
            Cow::Borrowed(
                "Point one of this workspace's worktrees at a different ref, so \
                 a local build can combine work that lives on separate \
                 branches — a frontend branch against the tip of a stack of \
                 backend PRs, say.\n\n\
                 This is the piece that makes a stacked build expressible. \
                 Building in Tethys uses whatever is on disk in each worktree, \
                 so moving one repo's worktree and leaving the others is enough; \
                 nothing else has to know. Fetches from origin first, then \
                 checks out — attached if it can, detached at the same commit \
                 if another worktree holds that branch. Either way the build is \
                 the same.\n\n\
                 The reply says where the worktree landed and whether it is on \
                 a branch, because a detached worktree is not somewhere to \
                 commit. Use describe_workspace to see where every worktree \
                 currently sits. It does not start a build.",
            ),
            schema.as_object().cloned().expect("schema literal is an object"),
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
        ListToolsResult::with_all_items(vec![
            self.link_pr_tool(),
            self.describe_workspace_tool(),
            self.give_pr_own_workspace_tool(),
            self.set_worktree_ref_tool(),
        ])
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
            TOOL_DESCRIBE_WORKSPACE => self.describe_workspace().await,
            TOOL_GIVE_PR_OWN_WORKSPACE => self.give_pr_own_workspace(request.arguments).await,
            TOOL_SET_WORKTREE_REF => self.set_worktree_ref(request.arguments).await,
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
            other => failed(format!("Tethys answered a link with {other:?}")),
        })
    }


    async fn describe_workspace(&self) -> Result<CallToolResponse, ErrorData> {
        let request = Request::DescribeWorkspace(DescribeWorkspace {
            from_workspace: self.from_workspace.clone(),
            from_session: self.from_session.clone(),
        });

        let response = match self.send(&request).await {
            Ok(response) => response,
            Err(e) => return Ok(failed(format!("could not read the workspace: {e}"))),
        };

        Ok(match response {
            Response::Described(view) => {
                CallToolResult::success(vec![ContentBlock::text(render_workspace(&view))]).into()
            }
            Response::Rejected { message } => failed(message),
            other => failed(format!("Tethys answered a describe with {other:?}")),
        })
    }

    async fn give_pr_own_workspace(
        &self,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResponse, ErrorData> {
        #[derive(Deserialize)]
        struct Args {
            reference: String,
            #[serde(default)]
            take_branch: bool,
        }
        let args: Args = parse_args(arguments)?;
        let request = Request::GivePrOwnWorkspace(GivePrOwnWorkspace {
            from_workspace: self.from_workspace.clone(),
            from_session: self.from_session.clone(),
            reference: args.reference,
            take_branch: args.take_branch,
        });

        let response = match self.send(&request).await {
            Ok(response) => response,
            Err(e) => {
                return Ok(failed(format!(
                    "no workspace was created, and nothing was detached: {e}"
                )))
            }
        };

        Ok(match response {
            Response::WorkspaceCreated {
                workspace_id,
                branch,
                repos,
                freed_from,
            } => {
                let freed = match freed_from {
                    Some(ws) => format!(
                        " Workspace {ws} was holding `{branch}` and has been detached — \
                         it keeps its files, but is no longer on that branch."
                    ),
                    None => String::new(),
                };
                CallToolResult::success(vec![ContentBlock::text(format!(
                    "Workspace {workspace_id} is provisioning on `{branch}` across {}. \
                     It takes a few minutes and nothing further is reported back here.{freed}",
                    repos.join(", ")
                ))])
                .into()
            }
            Response::Rejected { message } => {
                failed(format!("no workspace was created: {message}"))
            }
            other => failed(format!("Tethys answered with {other:?}")),
        })
    }

    async fn set_worktree_ref(
        &self,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResponse, ErrorData> {
        #[derive(Deserialize)]
        struct Args {
            repo_key: String,
            git_ref: String,
        }
        let args: Args = parse_args(arguments)?;
        let request = Request::SetWorktreeRef(SetWorktreeRef {
            from_workspace: self.from_workspace.clone(),
            from_session: self.from_session.clone(),
            repo_key: args.repo_key,
            git_ref: args.git_ref,
        });

        let response = match self.send(&request).await {
            Ok(response) => response,
            Err(e) => return Ok(failed(format!("nothing was moved: {e}"))),
        };

        Ok(match response {
            Response::WorktreeMoved {
                repo_key,
                worktree_path,
                branch,
                commit,
                detached_because_held_by,
            } => {
                let short = commit.chars().take(10).collect::<String>();
                let where_ = match &branch {
                    Some(b) => format!("on `{b}` at {short}"),
                    None => format!("detached at {short}"),
                };
                let why = match detached_because_held_by {
                    Some(holder) => format!(
                        " It is detached rather than on the branch because {holder} \
                         already holds it — the tree is the same, so the build is too."
                    ),
                    None => String::new(),
                };
                CallToolResult::success(vec![ContentBlock::text(format!(
                    "{repo_key} worktree ({worktree_path}) is now {where_}.{why} \
                     Building this workspace now builds that combination; nothing \
                     has been started."
                ))])
                .into()
            }
            Response::Rejected { message } => failed(format!("nothing was moved: {message}")),
            other => failed(format!("Tethys answered with {other:?}")),
        })
    }
}

/// Render the workspace as prose rather than JSON. An agent reads this to
/// decide what to do next, and a table of paths and branches is easier to act
/// on than a nested object.
fn render_workspace(view: &WorkspaceView) -> String {
    let mut out = format!("Workspace {} on branch `{}`", view.workspace_id, view.branch);
    if let Some(root) = &view.root {
        out.push_str(&format!("\nRoot: {root}"));
    }

    out.push_str("\n\nRepos:");
    if view.repos.is_empty() {
        out.push_str("\n  (none)");
    }
    for repo in &view.repos {
        out.push_str(&format!("\n  {} — {}", repo.repo_key, repo.worktree_path));
        match repo.current_branch.as_deref() {
            Some(b) if b == view.branch => out.push_str(&format!("\n    on `{b}`")),
            // Worth calling out: the worktree is not where Tethys thinks it is,
            // which is exactly the case an agent must not paper over.
            Some(b) => out.push_str(&format!(
                "\n    on `{b}` — NOT the workspace branch `{}`",
                view.branch
            )),
            None => out.push_str("\n    branch unknown (worktree missing?)"),
        }
        match &repo.branch_pr {
            Some(pr) => out.push_str(&format!("\n    branch PR: {}", render_pr(pr))),
            None => out.push_str("\n    branch PR: none"),
        }
    }

    out.push_str("\n\nAttached PRs:");
    if view.attached_prs.is_empty() {
        out.push_str("\n  (none)");
    }
    let chains = order_into_stacks(&view.attached_prs);
    for chain in &chains {
        let stacked = chain.len() > 1;
        for (position, &i) in chain.iter().enumerate() {
            let pr = &view.attached_prs[i];
            let where_ = pr.repo_key.as_deref().unwrap_or("repo not recorded");
            if stacked {
                // Position is the actionable part: "the first three" means
                // checking out the head of number three, which contains them.
                out.push_str(&format!(
                    "\n  [{where_}] {}. {}",
                    position + 1,
                    render_pr(pr)
                ));
            } else {
                out.push_str(&format!("\n  [{where_}] {}", render_pr(pr)));
            }
        }
        if stacked {
            out.push_str(
                "\n    ^ a stack, bottom first. Checking out one of these includes \
                 everything below it.",
            );
        }
    }
    out
}

/// Order PRs into stacks: a PR whose base is another PR's head sits on top of
/// it. Returns each chain bottom-first, plus whatever didn't chain to anything.
///
/// This reads GitHub's base branch rather than a `gh stack` object, which
/// means a hand-made chain and a formal stack look the same. For deciding what
/// to build that is the right answer — the question is which PR contains which,
/// and the base chain answers it either way.
fn order_into_stacks(prs: &[PrView]) -> Vec<Vec<usize>> {
    let mut chains: Vec<Vec<usize>> = Vec::new();
    // A PR is a base for another when its head is that other's base.
    let stacked_on = |i: usize| -> Option<usize> {
        let base = prs[i].base_branch.as_deref()?;
        prs.iter()
            .position(|p| p.head_branch.as_deref() == Some(base) && p.number != prs[i].number)
    };
    // Bottoms are the PRs nothing beneath them explains.
    let bottoms: Vec<usize> = (0..prs.len()).filter(|i| stacked_on(*i).is_none()).collect();

    for bottom in bottoms {
        let mut chain = vec![bottom];
        // Walk upward: whoever is stacked directly on the current tip.
        loop {
            let tip = *chain.last().expect("chain is never empty");
            let next = (0..prs.len()).find(|i| {
                !chain.contains(i)
                    && stacked_on(*i) == Some(tip)
            });
            match next {
                Some(i) => chain.push(i),
                None => break,
            }
        }
        chains.push(chain);
    }
    chains
}

fn render_pr(pr: &PrView) -> String {
    let mut bits = vec![format!("{}/{}#{}", pr.owner, pr.name, pr.number)];
    match (&pr.head_branch, &pr.base_branch) {
        (Some(head), Some(base)) => bits.push(format!("`{head}` onto `{base}`")),
        (Some(head), None) => bits.push(format!("from `{head}`")),
        _ => {}
    }
    if let Some(state) = &pr.state {
        let draft = if pr.is_draft.unwrap_or(false) {
            " (draft)"
        } else {
            ""
        };
        bits.push(format!("{state}{draft}"));
    }
    if let Some(checks) = &pr.checks {
        bits.push(format!("checks {checks}"));
    }
    if let Some(review) = &pr.review_decision {
        bits.push(format!("review {review}"));
    }
    if pr.unresolved_threads.unwrap_or(0) > 0 {
        bits.push(format!("{} unresolved", pr.unresolved_threads.unwrap_or(0)));
    }
    if pr.has_merge_conflicts.unwrap_or(false) {
        bits.push("CONFLICTS".into());
    }
    bits.push(pr.url.clone());
    bits.join(" · ")
}

/// A no-argument tool still needs an object schema; some clients reject a bare
/// `{}` with no `type`.
fn empty_schema() -> JsonObject {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
        .as_object()
        .cloned()
        .expect("input schema literal is an object")
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

    /// Every tool has to be listed here *and* in `ALLOWED_TOOLS`. One that
    /// reaches the agent without the permission entry stalls on a dialog
    /// nobody is watching, so the two lists are checked against each other.
    #[test]
    fn the_tools_reply_carries_every_tool_and_each_one_is_allowed() {
        let raw = serde_json::to_value(server().tools_result()).expect("serialize");
        let names: Vec<&str> = raw["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|t| t["name"].as_str().expect("name"))
            .collect();
        assert_eq!(
            names,
            vec![
                TOOL_LINK_PR,
                TOOL_DESCRIBE_WORKSPACE,
                TOOL_GIVE_PR_OWN_WORKSPACE,
                TOOL_SET_WORKTREE_REF
            ]
        );

        for name in names {
            let qualified = format!("mcp__tethys__{name}");
            assert!(
                tethys_mcp::ALLOWED_TOOLS.contains(&qualified.as_str()),
                "{qualified} is offered but missing from ALLOWED_TOOLS"
            );
        }
    }

    /// A no-argument tool still needs a well-formed object schema.
    #[test]
    fn describe_workspace_takes_no_arguments() {
        let raw = serde_json::to_value(server().describe_workspace_tool()).expect("serialize");
        assert_eq!(raw["inputSchema"]["type"], "object");
        assert!(raw["inputSchema"]["required"].is_null());
    }

    /// Taking a branch off another workspace has to be opted into, so the
    /// schema must offer it — and must not require it, or every call becomes a
    /// decision to disturb someone.
    #[test]
    fn give_pr_own_workspace_offers_take_branch_without_requiring_it() {
        let raw = serde_json::to_value(server().give_pr_own_workspace_tool()).expect("serialize");
        let schema = &raw["inputSchema"];
        assert_eq!(schema["required"], serde_json::json!(["reference"]));
        assert_eq!(schema["properties"]["take_branch"]["type"], "boolean");
    }

    /// A frame from a client predating the flag must read as "don't take it".
    #[test]
    fn a_give_frame_without_take_branch_defaults_to_refusing() {
        let raw = r##"{
            "op": "give_pr_own_workspace",
            "from_workspace": "ws-1",
            "reference": "#4300"
        }"##;
        let tethys_mcp::Request::GivePrOwnWorkspace(req) =
            serde_json::from_str(raw).expect("must deserialize")
        else {
            panic!("must parse as give_pr_own_workspace")
        };
        assert!(!req.take_branch);
    }

    fn pr(number: u32, head: &str, base: Option<&str>) -> PrView {
        PrView {
            owner: "acme".into(),
            name: "be".into(),
            number,
            url: format!("https://github.com/acme/be/pull/{number}"),
            repo_key: Some("backend".into()),
            head_branch: Some(head.into()),
            base_branch: base.map(str::to_string),
            state: None,
            is_draft: None,
            checks: None,
            review_decision: None,
            unresolved_threads: None,
            has_merge_conflicts: None,
        }
    }

    /// The case this exists for: six PRs where the third contains the first
    /// three, and knowing that is what makes "build against the first three"
    /// answerable.
    #[test]
    fn a_stack_is_ordered_bottom_first() {
        let prs = vec![
            pr(103, "be-3", Some("be-2")),
            pr(101, "be-1", Some("master")),
            pr(102, "be-2", Some("be-1")),
        ];
        let chains = order_into_stacks(&prs);
        assert_eq!(chains.len(), 1, "one chain, got {chains:?}");
        let numbers: Vec<u32> = chains[0].iter().map(|&i| prs[i].number).collect();
        assert_eq!(numbers, vec![101, 102, 103]);
    }

    /// Two independent PRs are two chains, not one arbitrary ordering.
    #[test]
    fn unrelated_prs_do_not_chain() {
        let prs = vec![
            pr(1, "feat-a", Some("master")),
            pr(2, "feat-b", Some("master")),
        ];
        let chains = order_into_stacks(&prs);
        assert_eq!(chains.len(), 2);
        assert!(chains.iter().all(|c| c.len() == 1));
    }

    /// Separate stacks stay separate — a backend stack and a frontend stack
    /// must not be spliced into one because both start from master.
    #[test]
    fn two_stacks_stay_apart() {
        let prs = vec![
            pr(1, "be-1", Some("master")),
            pr(2, "be-2", Some("be-1")),
            pr(10, "fe-1", Some("master")),
            pr(11, "fe-2", Some("fe-1")),
        ];
        let mut chains = order_into_stacks(&prs);
        chains.sort_by_key(|c| prs[c[0]].number);
        assert_eq!(chains.len(), 2);
        assert_eq!(
            chains[0].iter().map(|&i| prs[i].number).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            chains[1].iter().map(|&i| prs[i].number).collect::<Vec<_>>(),
            vec![10, 11]
        );
    }

    /// A PR polled before base_branch existed can't be placed, and must still
    /// appear rather than being dropped from the listing.
    #[test]
    fn a_pr_without_a_base_still_appears() {
        let mut orphan = pr(7, "mystery", None);
        orphan.base_branch = None;
        let prs = vec![orphan];
        let chains = order_into_stacks(&prs);
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0], vec![0]);
    }

    /// Every PR has to land in exactly one chain, or the listing silently
    /// loses one.
    #[test]
    fn every_pr_appears_exactly_once() {
        let prs = vec![
            pr(1, "be-1", Some("master")),
            pr(2, "be-2", Some("be-1")),
            pr(3, "solo", Some("master")),
        ];
        let mut seen: Vec<usize> = order_into_stacks(&prs).into_iter().flatten().collect();
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2]);
    }
}
