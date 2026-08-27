//! The wire format shared by the `tethys-mcp` companion binary and the Tethys
//! app that answers its frames.
//!
//! Same discipline as [`tethys_hook`]: one type, defined once, so a field
//! can't be added to the sender and silently dropped by the receiver. The
//! failure contract is the opposite one, though. The hook must never disrupt a
//! Claude session, so it swallows everything; an agent that believes it showed
//! Nathan a PR when it didn't will move on as though the work is visible.

use std::io;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Name the server registers under, so its tools are addressed as
/// `mcp__tethys__<tool>` in Claude's permission strings.
pub const SERVER_NAME: &str = "tethys";

/// Point the calling workspace at a pull request. The agent asks for the
/// effect, and doesn't need the word "attach" the UI uses.
pub const TOOL_LINK_PR: &str = "link_pr";

/// Every tool, fully qualified the way Claude's permission system spells them.
/// Each one has to be listed for `--allowed-tools`, or a call to it stalls on a
/// permission dialog nobody is watching.
pub const ALLOWED_TOOLS: &[&str] = &["mcp__tethys__link_pr"];

/// Env keys Tethys bakes into the generated `--mcp-config` at spawn time.
/// The calling session's identity arrives this way rather than as tool
/// arguments, so an agent cannot claim an origin that isn't its own.
pub const ENV_SOCKET: &str = "TETHYS_MCP_SOCKET";
pub const ENV_WORKSPACE_ID: &str = "TETHYS_MCP_WORKSPACE_ID";
pub const ENV_SESSION_ID: &str = "TETHYS_MCP_SESSION_ID";
/// Comma-separated registry repo keys, used to build the tool's `repo_key`
/// enum so a calling agent can't name a repo that doesn't exist. Fixed for the
/// life of the session — Tethys only reloads `repos.toml` at boot, so this is
/// as fresh as the app's own view of the registry.
pub const ENV_REPO_KEYS: &str = "TETHYS_MCP_REPO_KEYS";

/// Cap on a single frame in either direction. The only way past this is a bug
/// or a hostile caller.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// One request from the MCP server to the app. One variant per tool — the tag
/// is what lets them share a socket, and is why a second tool costs a variant
/// rather than a second socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    LinkPr(LinkPr),
}

/// A link request: the reference the agent typed, plus the identity Tethys
/// baked into the server's environment.
///
/// The workspace the PR lands on is *not* an argument — an agent gets to say
/// which PR, never whose workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkPr {
    /// Workspace the calling session belongs to, and the one the PR is linked
    /// to.
    pub from_workspace: String,
    /// Calling session's Tethys id. Unused beyond logging today, but a link is
    /// worth tracing back to the session that asked for it.
    #[serde(default)]
    pub from_session: Option<String>,
    /// Which of the workspace's repos the PR belongs to. `None` leaves it to
    /// be inferred, from the reference's own `owner/repo` or from there being
    /// only one GitHub-linked repo to choose.
    #[serde(default)]
    pub repo_key: Option<String>,
    /// `123`, `#123`, `owner/repo#123`, or a full GitHub PR URL.
    pub reference: String,
}

/// The app's answer, one success variant per request plus a shared refusal.
///
/// A link isn't reported until the PR has been resolved to real GitHub
/// coordinates, so the agent is never told a link happened that didn't.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Linked {
        owner: String,
        name: String,
        number: u32,
        url: String,
        /// True when this was already attached — the call is idempotent, and
        /// saying so keeps an agent from reporting a change it didn't make.
        already_attached: bool,
    },
    Rejected {
        message: String,
    },
}

/// Write a length-prefixed JSON frame.
pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(value).map_err(io::Error::other)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {} bytes exceeds the cap", payload.len()),
        ));
    }
    writer
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    writer.write_all(&payload).await?;
    writer.flush().await
}

/// Read one length-prefixed JSON frame.
pub async fn read_frame<R, T>(reader: &mut R) -> io::Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} out of bounds"),
        ));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    serde_json::from_slice(&buf).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_link_frame_round_trips_under_its_own_tag() {
        let req = Request::LinkPr(LinkPr {
            from_workspace: "ws-1".into(),
            from_session: Some("sess-1".into()),
            repo_key: Some("backend".into()),
            reference: "https://github.com/me/api/pull/12".into(),
        });

        let raw = serde_json::to_value(&req).expect("serialize");
        assert_eq!(raw["op"], "link_pr");

        let mut buf = Vec::new();
        write_frame(&mut buf, &req).await.expect("write");
        let mut cursor = std::io::Cursor::new(buf);
        let Request::LinkPr(back) = read_frame(&mut cursor).await.expect("read");
        assert_eq!(back.reference, "https://github.com/me/api/pull/12");
        assert_eq!(back.repo_key.as_deref(), Some("backend"));
    }

    /// `repo_key` is only ever disambiguation, so the common call omits it.
    #[test]
    fn a_link_frame_without_a_repo_key_parses() {
        let raw = r##"{
            "op": "link_pr",
            "from_workspace": "ws-1",
            "reference": "#12"
        }"##;
        let Request::LinkPr(req) = serde_json::from_str(raw).expect("must deserialize");
        assert_eq!(req.repo_key, None);
        assert_eq!(req.from_session, None);
    }

    /// A truncated prefix must be an error, not a hang or a panic.
    #[tokio::test]
    async fn a_short_frame_is_an_error() {
        let mut cursor = std::io::Cursor::new(vec![0u8, 0, 1]);
        let got: io::Result<Request> = read_frame(&mut cursor).await;
        assert!(got.is_err());
    }

    #[tokio::test]
    async fn a_zero_length_frame_is_rejected() {
        let mut cursor = std::io::Cursor::new(0u32.to_be_bytes().to_vec());
        let got: io::Result<Request> = read_frame(&mut cursor).await;
        assert_eq!(got.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
