use std::{
    borrow::Cow, env, ffi::OsString, fs::Permissions, os::unix::fs::PermissionsExt, path::Path,
    path::PathBuf, process::Stdio,
};

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{
        CallToolResult, ContentBlock, ErrorCode, ErrorData, Implementation, ServerCapabilities,
        ServerInfo,
    },
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
    ServerHandler, ServiceExt,
};
use serde::{Deserialize, Serialize};
use tokio::fs;

const FLOATING_WIDTH: &str = "85%";
const FLOATING_HEIGHT: &str = "75%";
const MAX_PANE_TITLE_CHARS: usize = 80;
const AGENT_URL_ENV: &str = "EDIT_PROPOSED_TEXT_AGENT_URL";
const AGENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EditRequest {
    #[schemars(description = "The text to open for editing")]
    text: String,

    #[schemars(description = "Short label for what the text is (default: 'proposed text')")]
    purpose: Option<String>,

    #[schemars(description = "File extension for the temp file (default: 'md')")]
    file_extension: Option<String>,

    #[schemars(description = "Open in a zellij floating pane (default: true)")]
    floating: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EditIdRequest {
    #[schemars(description = "The edit_id returned by begin_edit or edit_proposed_text")]
    edit_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
enum EditMode {
    ZellijFloating,
    ZellijPane,
    Zed,
    Agent,
}

#[derive(Debug, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "lowercase")]
#[schemars(extend("type" = "object"))]
enum EditResult {
    Pending {
        edit_id: String,
        mode: EditMode,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Completed {
        edit_id: String,
        text: String,
        changed: bool,
        mode: EditMode,
    },
    Canceled {
        edit_id: String,
    },
    Failed {
        edit_id: String,
        exit_code: i32,
    },
}

#[derive(Debug, Deserialize, Serialize)]
struct StoredEdit {
    original: String,
    extension: String,
    mode: EditMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<AgentEdit>,
}

// where an agent-mode draft lives on the edit agent
#[derive(Debug, Clone, Deserialize, Serialize)]
struct AgentEdit {
    url: String,
    id: String,
}

#[derive(Debug, Serialize)]
struct AgentCreate<'a> {
    text: &'a str,
    extension: &'a str,
    purpose: &'a str,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum AgentStatus {
    Pending,
    Completed { text: String },
    Canceled,
    Failed { exit_code: Option<i32> },
}

#[derive(Debug, Deserialize)]
struct AgentCreated {
    edit_id: String,
}

#[derive(Debug, Clone)]
struct EditServer {
    tool_router: rmcp::handler::server::router::tool::ToolRouter<EditServer>,
    state_dir: PathBuf,
    zellij_session: Option<String>,
    agent_url: Option<String>,
    http: reqwest::Client,
}

fn internal_error(context: &str, err: impl std::fmt::Display) -> ErrorData {
    ErrorData {
        code: ErrorCode::INTERNAL_ERROR,
        message: Cow::from(format!("{context}: {err}")),
        data: None,
    }
}

fn resolve_editor() -> String {
    env::var("PI_EDIT_TEXT_EDITOR")
        .or_else(|_| env::var("VISUAL"))
        .or_else(|_| env::var("EDITOR"))
        .unwrap_or_else(|_| "nvim".into())
}

fn sanitize_extension(ext: Option<&str>) -> &str {
    ext.filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("md")
}

fn normalize_purpose(purpose: Option<&str>) -> String {
    let purpose = purpose
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if purpose.is_empty() {
        "proposed text".into()
    } else {
        purpose
    }
}

fn pane_title(purpose: &str) -> String {
    format!("Edit {purpose}")
        .chars()
        .take(MAX_PANE_TITLE_CHARS)
        .collect()
}

fn neovim_filetype(extension: &str) -> &str {
    match extension {
        "md" => "markdown",
        "txt" => "text",
        extension => extension,
    }
}

fn cursor_placement(purpose: &str, extension: &str) -> &'static str {
    if purpose.to_ascii_lowercase().contains("title") || extension == "gitcommit" {
        "start"
    } else {
        "end"
    }
}

fn edit_result(response: EditResult) -> CallToolResult {
    let value = rmcp::serde_json::to_value(&response).expect("EditResult must serialize to JSON");
    let mut result = CallToolResult::success(vec![ContentBlock::text(value.to_string())]);
    result.is_error = Some(matches!(response, EditResult::Failed { .. }));
    result.structured_content = Some(value);
    result
}

fn resolve_zed() -> PathBuf {
    for path in [
        "/usr/local/bin/zed",
        "/opt/homebrew/bin/zed",
        "/Applications/Zed.app/Contents/MacOS/cli",
        "/Applications/Zed Preview.app/Contents/MacOS/cli",
    ] {
        if Path::new(path).is_file() {
            return path.into();
        }
    }
    if let Some(home) = env::var_os("HOME") {
        for app in ["Zed.app", "Zed Preview.app"] {
            let path = PathBuf::from(&home)
                .join("Applications")
                .join(app)
                .join("Contents/MacOS/cli");
            if path.is_file() {
                return path;
            }
        }
    }
    "zed".into()
}

const ZED_SCRIPT: &str = r#"#!/usr/bin/env bash
"$3" --wait --new "$1"
exit_code=$?
printf "%s\n" "$exit_code" > "$2.tmp"
mv "$2.tmp" "$2"
"#;

async fn launch_zed(script: &Path, text: &Path, done: &Path, zed: &Path) -> std::io::Result<()> {
    let mut child = tokio::process::Command::new("bash")
        .arg(script)
        .arg(text)
        .arg(done)
        .arg(zed)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

const EDITOR_SCRIPT: &str = r#"#!/usr/bin/env bash
set +e
file="$1"
done_file="$2"
editor_cmd="$3"
filetype="$4"
cursor="$5"
[ -z "$editor_cmd" ] && editor_cmd="nvim"
editor_bin="${editor_cmd%% *}"

if [ "${editor_bin##*/}" = "nvim" ]; then
	cursor_cmd="normal! G$"
	[ "$cursor" = "start" ] && cursor_cmd="normal! gg0"
	bash -lc "$editor_cmd -c 'setlocal filetype=$filetype' -c '$cursor_cmd' \"\$1\"" -- "$file"
else
	bash -lc "$editor_cmd \"\$1\"" -- "$file"
fi

exit_code=$?
printf "%s\n" "$exit_code" > "$done_file.tmp"
mv "$done_file.tmp" "$done_file"
"#;

struct ZellijLaunch<'a> {
    script: &'a Path,
    text: &'a Path,
    done: &'a Path,
    editor: &'a str,
    filetype: &'a str,
    cursor: &'a str,
    floating: bool,
    session: &'a str,
    title: &'a str,
}

fn zellij_args(launch: &ZellijLaunch<'_>) -> Vec<OsString> {
    let mut args = vec![
        "--session".into(),
        launch.session.into(),
        "run".into(),
        "--close-on-exit".into(),
        "--near-current-pane".into(),
    ];

    if launch.floating {
        args.extend([
            "--floating".into(),
            "--pinned".into(),
            "true".into(),
            "--width".into(),
            FLOATING_WIDTH.into(),
            "--height".into(),
            FLOATING_HEIGHT.into(),
        ]);
    }

    args.extend([
        "--name".into(),
        launch.title.into(),
        "--".into(),
        launch.script.as_os_str().into(),
        launch.text.as_os_str().into(),
        launch.done.as_os_str().into(),
        launch.editor.into(),
        launch.filetype.into(),
        launch.cursor.into(),
    ]);
    args
}

async fn launch_zellij(launch: &ZellijLaunch<'_>) -> std::io::Result<bool> {
    let mut cmd = tokio::process::Command::new("zellij");
    // pin to the session we verified, so a binary/socket mismatch cannot
    // silently land the pane in some other session
    let status = cmd
        .args(zellij_args(launch))
        .stdout(Stdio::null())
        .status()
        .await?;

    Ok(status.success())
}

// confirm the current zellij binary can actually see the session named in
// ZELLIJ_SESSION_NAME. when claude is launched from a stale session whose
// socket protocol no longer matches the binary on PATH (e.g. 0.43.1 session,
// 0.44.1 binary), `zellij run` would otherwise dispatch into whichever
// session the binary can see, which the user is not looking at.
async fn ensure_session_reachable(session: &str) -> Result<(), String> {
    let output = tokio::process::Command::new("zellij")
        .arg("list-sessions")
        .arg("--no-formatting")
        .arg("--short")
        .output()
        .await
        .map_err(|e| format!("failed to run `zellij list-sessions`: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "`zellij list-sessions` exited with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let visible: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if visible.contains(&session) {
        return Ok(());
    }

    Err(format!(
        "ZELLIJ_SESSION_NAME is '{session}' but the current zellij binary cannot see it. \
         Visible sessions: [{}]. This usually means claude was launched from a zellij \
         session whose socket protocol does not match the zellij binary now on PATH \
         (e.g. an old 0.43.x session and a newer 0.44.x binary). Restart claude inside \
         a session the current binary can see.",
        visible.join(", ")
    ))
}

#[tool_router]
impl EditServer {
    fn new(state_dir: PathBuf) -> Self {
        Self {
            tool_router: Self::tool_router(),
            state_dir,
            zellij_session: None,
            agent_url: None,
            http: reqwest::Client::builder()
                .timeout(AGENT_TIMEOUT)
                .build()
                .expect("http client must build"),
        }
    }

    #[tool(
        description = "Nonblocking alias for begin_edit. Returns pending and an edit_id, NOT edited text. After the user saves and closes the editor tab, call finish_edit with that ID. Never treat pending as approval or poll in a long-running loop.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<EditResult>()
    )]
    async fn edit_proposed_text(
        &self,
        Parameters(req): Parameters<EditRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        self.begin_edit(Parameters(req)).await
    }

    #[tool(
        description = "Open a persistent draft in a Zellij pane, or outside Zellij in Zed, locally or on the user's laptop through the edit agent. Returns immediately with pending and an edit_id. Ask the user to save and close the editor tab, then reply when ready. Call finish_edit on that reply; do not wait or repeatedly poll in one tool call. Pending is not approval.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<EditResult>()
    )]
    async fn begin_edit(
        &self,
        Parameters(req): Parameters<EditRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let session = self.zellij_session.as_deref();
        if let Some(session) = session {
            if let Err(msg) = ensure_session_reachable(session).await {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "Error: {msg}"
                ))]));
            }
        }

        let editor = resolve_editor();
        let ext = sanitize_extension(req.file_extension.as_deref());
        let floating = req.floating.unwrap_or(true);
        let purpose = normalize_purpose(req.purpose.as_deref());
        let title = pane_title(&purpose);
        let filetype = neovim_filetype(ext);
        let cursor = cursor_placement(&purpose, ext);

        fs::create_dir_all(&self.state_dir)
            .await
            .map_err(|e| internal_error("failed to create edit storage", e))?;
        fs::set_permissions(&self.state_dir, Permissions::from_mode(0o700))
            .await
            .map_err(|e| internal_error("failed to secure edit storage", e))?;
        let tmp_dir = tempfile::Builder::new()
            .prefix("edit-")
            .permissions(Permissions::from_mode(0o700))
            .tempdir_in(&self.state_dir)
            .map_err(|e| internal_error("failed to create draft directory", e))?;
        let text_path = tmp_dir.path().join(format!("draft.{ext}"));
        let done_path = tmp_dir.path().join("editor.done");
        let script_path = tmp_dir.path().join("run-editor.sh");

        fs::write(&text_path, &req.text)
            .await
            .map_err(|e| internal_error("failed to write draft", e))?;

        // zellij keeps the edit next to the terminal, the agent is for runners
        // without one
        if let (None, Some(url)) = (session, &self.agent_url) {
            let agent = match self.agent_create(url, &req.text, ext, &purpose).await {
                Ok(agent) => agent,
                Err(msg) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                        "Error: failed to open the draft through the edit agent at {url}: {msg}"
                    ))]))
                }
            };
            return self
                .store_pending(
                    tmp_dir,
                    &req.text,
                    ext,
                    EditMode::Agent,
                    Some(agent),
                    &purpose,
                )
                .await;
        }

        fs::write(
            &script_path,
            if session.is_some() {
                EDITOR_SCRIPT
            } else {
                ZED_SCRIPT
            },
        )
        .await
        .map_err(|e| internal_error("failed to write script", e))?;
        fs::set_permissions(&script_path, Permissions::from_mode(0o755))
            .await
            .map_err(|e| internal_error("failed to chmod script", e))?;

        let launch = |floating| ZellijLaunch {
            script: &script_path,
            text: &text_path,
            done: &done_path,
            editor: &editor,
            filetype,
            cursor,
            floating,
            session: session.unwrap_or_default(),
            title: &title,
        };

        // try floating first, fall back to embedded pane
        let mode = if session.is_none() {
            launch_zed(&script_path, &text_path, &done_path, &resolve_zed())
                .await
                .map_err(|e| internal_error("failed to launch Zed", e))?;
            Some(EditMode::Zed)
        } else if floating {
            match launch_zellij(&launch(true)).await {
                Ok(true) => Some(EditMode::ZellijFloating),
                _ => {
                    tracing::warn!("floating pane failed, retrying as embedded pane");
                    launch_zellij(&launch(false))
                        .await
                        .unwrap_or(false)
                        .then_some(EditMode::ZellijPane)
                }
            }
        } else {
            launch_zellij(&launch(false))
                .await
                .unwrap_or(false)
                .then_some(EditMode::ZellijPane)
        };

        let Some(mode) = mode else {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "Error: failed to launch editor via zellij.",
            )]));
        };

        self.store_pending(tmp_dir, &req.text, ext, mode, None, &purpose)
            .await
    }

    async fn store_pending(
        &self,
        tmp_dir: tempfile::TempDir,
        original: &str,
        ext: &str,
        mode: EditMode,
        agent: Option<AgentEdit>,
        purpose: &str,
    ) -> Result<CallToolResult, ErrorData> {
        let edit_id = tmp_dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let stored = StoredEdit {
            original: original.into(),
            extension: ext.into(),
            mode,
            agent,
        };
        fs::write(
            tmp_dir.path().join("edit.json"),
            rmcp::serde_json::to_vec(&stored).unwrap(),
        )
        .await
        .map_err(|e| internal_error("failed to save edit metadata", e))?;
        let _ = tmp_dir.keep();
        tracing::info!("opened {purpose} as {edit_id}");
        Ok(edit_result(EditResult::Pending {
            edit_id,
            mode,
            note: None,
        }))
    }

    fn edit_path(&self, edit_id: &str) -> Result<PathBuf, ErrorData> {
        if !edit_id.starts_with("edit-")
            || edit_id.len() > 64
            || !edit_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(ErrorData {
                code: ErrorCode::INVALID_PARAMS,
                message: "invalid edit_id".into(),
                data: None,
            });
        }
        Ok(self.state_dir.join(edit_id))
    }

    async fn load_edit(&self, path: &Path) -> Result<StoredEdit, ErrorData> {
        let bytes = fs::read(path.join("edit.json"))
            .await
            .map_err(|e| internal_error("edit not found or unreadable", e))?;
        rmcp::serde_json::from_slice(&bytes).map_err(|e| internal_error("invalid edit metadata", e))
    }

    #[tool(
        description = "Check an edit without waiting. Returns pending until the editor closes, completed with the exact saved text, canceled, or failed. If pending, ask the user to save and close the tab and reply when ready. Never treat pending, canceled, or failed as approval. Results survive MCP restarts and can be read again.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<EditResult>()
    )]
    async fn finish_edit(
        &self,
        Parameters(req): Parameters<EditIdRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let path = self.edit_path(&req.edit_id)?;
        let stored = self.load_edit(&path).await?;
        if fs::try_exists(path.join("canceled"))
            .await
            .map_err(|e| internal_error("failed to check cancellation", e))?
        {
            return Ok(edit_result(EditResult::Canceled {
                edit_id: req.edit_id,
            }));
        }
        if let Some(agent) = &stored.agent {
            if let Some(result) = self.sync_agent(&path, &stored, agent, &req.edit_id).await? {
                return Ok(result);
            }
        }
        let exit_code_raw = match fs::read_to_string(path.join("editor.done")).await {
            Ok(value) => value,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(edit_result(EditResult::Pending {
                    edit_id: req.edit_id,
                    mode: stored.mode,
                    note: None,
                }));
            }
            Err(e) => return Err(internal_error("failed to read editor status", e)),
        };
        let exit_code: i32 = exit_code_raw
            .trim()
            .parse()
            .map_err(|e| internal_error("invalid editor exit code", e))?;
        if exit_code != 0 {
            return Ok(edit_result(EditResult::Failed {
                edit_id: req.edit_id,
                exit_code,
            }));
        }
        let text = fs::read_to_string(path.join(format!("draft.{}", stored.extension)))
            .await
            .map_err(|e| internal_error("failed to read edited text", e))?;
        Ok(edit_result(EditResult::Completed {
            edit_id: req.edit_id,
            changed: text != stored.original,
            text,
            mode: stored.mode,
        }))
    }

    #[tool(
        description = "Cancel an edit so finish_edit cannot return it as completed. Does not close the editor or delete the draft. Cancellation is persistent and repeatable.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<EditResult>()
    )]
    async fn cancel_edit(
        &self,
        Parameters(req): Parameters<EditIdRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let path = self.edit_path(&req.edit_id)?;
        let stored = self.load_edit(&path).await?;
        fs::write(path.join("canceled"), b"")
            .await
            .map_err(|e| internal_error("failed to cancel edit", e))?;
        // the local marker already decides the result, so an unreachable agent
        // only keeps its copy pending until it is pruned
        if let Some(agent) = &stored.agent {
            if let Err(e) = self.http.delete(agent_edit_url(agent)).send().await {
                tracing::warn!("failed to cancel {} on the edit agent: {e}", agent.id);
            }
        }
        Ok(edit_result(EditResult::Canceled {
            edit_id: req.edit_id,
        }))
    }
}

impl EditServer {
    async fn agent_create(
        &self,
        url: &str,
        text: &str,
        extension: &str,
        purpose: &str,
    ) -> Result<AgentEdit, String> {
        let response = self
            .http
            .post(format!("{}/v1/edits", url.trim_end_matches('/')))
            .json(&AgentCreate {
                text,
                extension,
                purpose,
            })
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!("{status}: {}", body.trim()));
        }
        let created: AgentCreated = response.json().await.map_err(|e| e.to_string())?;
        if !valid_agent_id(&created.edit_id) {
            return Err(format!("invalid edit id {:?}", created.edit_id));
        }
        Ok(AgentEdit {
            url: url.trim_end_matches('/').into(),
            id: created.edit_id,
        })
    }

    // mirrors the agent's state into the local draft and markers, so a
    // finished edit is read like any other and stays readable offline.
    // returns a result only when the local files cannot answer yet.
    async fn sync_agent(
        &self,
        path: &Path,
        stored: &StoredEdit,
        agent: &AgentEdit,
        edit_id: &str,
    ) -> Result<Option<CallToolResult>, ErrorData> {
        let done_path = path.join("editor.done");
        if fs::try_exists(&done_path)
            .await
            .map_err(|e| internal_error("failed to check editor status", e))?
        {
            return Ok(None);
        }
        let pending = |note: Option<String>| -> Result<Option<CallToolResult>, ErrorData> {
            Ok(Some(edit_result(EditResult::Pending {
                edit_id: edit_id.into(),
                mode: stored.mode,
                note,
            })))
        };
        let status = match self.agent_status(agent).await {
            Ok(status) => status,
            Err(msg) => return pending(Some(format!("edit agent unreachable: {msg}"))),
        };
        let exit_code = match status {
            AgentStatus::Pending => return pending(None),
            AgentStatus::Canceled => {
                fs::write(path.join("canceled"), b"")
                    .await
                    .map_err(|e| internal_error("failed to record cancellation", e))?;
                return Ok(Some(edit_result(EditResult::Canceled {
                    edit_id: edit_id.into(),
                })));
            }
            AgentStatus::Completed { text } => {
                // the draft goes first, so a crash before the marker only
                // repeats the sync
                fs::write(path.join(format!("draft.{}", stored.extension)), text)
                    .await
                    .map_err(|e| internal_error("failed to save edited text", e))?;
                0
            }
            AgentStatus::Failed { exit_code } => exit_code.unwrap_or(-1),
        };
        let tmp = path.join("editor.done.tmp");
        fs::write(&tmp, format!("{exit_code}\n"))
            .await
            .map_err(|e| internal_error("failed to record editor status", e))?;
        fs::rename(&tmp, &done_path)
            .await
            .map_err(|e| internal_error("failed to record editor status", e))?;
        Ok(None)
    }

    async fn agent_status(&self, agent: &AgentEdit) -> Result<AgentStatus, String> {
        let response = self
            .http
            .get(agent_edit_url(agent))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!("{status}: {}", body.trim()));
        }
        response.json().await.map_err(|e| e.to_string())
    }
}

fn valid_agent_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn agent_edit_url(agent: &AgentEdit) -> String {
    format!("{}/v1/edits/{}", agent.url, agent.id)
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for EditServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "edit-proposed-text",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Use begin_edit (or its edit_proposed_text alias) to open proposed text \
                 in Zellij or Zed, locally or on the user's laptop. It returns immediately with an edit_id, not edited text. \
                 Ask the user to save, close the editor tab, and reply when ready. On that \
                 reply, call finish_edit. Do not poll in a long-running loop. Only completed \
                 contains final text; pending, canceled, and failed are not approval. \
                 Editing does not replace any required explicit approval to post content.",
            )
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let home = env::var_os("HOME").ok_or("HOME is required for persistent edit storage")?;
    let state_dir = PathBuf::from(home).join(".local/state/edit-proposed-text");
    let mut server = EditServer::new(state_dir);
    server.zellij_session = env::var("ZELLIJ_SESSION_NAME")
        .ok()
        .filter(|s| !s.is_empty());
    server.agent_url = env::var(AGENT_URL_ENV).ok().filter(|s| !s.is_empty());
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    async fn stored_draft(root: &Path, edit_id: &str) -> PathBuf {
        let path = root.join(edit_id);
        fs::create_dir_all(&path).await.unwrap();
        fs::write(path.join("draft.md"), "before\n").await.unwrap();
        fs::write(
            path.join("edit.json"),
            rmcp::serde_json::to_vec(&StoredEdit {
                original: "before\n".into(),
                extension: "md".into(),
                mode: EditMode::Zed,
                agent: None,
            })
            .unwrap(),
        )
        .await
        .unwrap();
        path
    }

    async fn finish(server: &EditServer, edit_id: &str) -> CallToolResult {
        server
            .finish_edit(Parameters(EditIdRequest {
                edit_id: edit_id.into(),
            }))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn checks_pending_without_waiting_and_recovers_exact_text_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = stored_draft(root.path(), "edit-restart").await;
        let server = EditServer::new(root.path().into());
        let pending = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            finish(&server, "edit-restart"),
        )
        .await
        .unwrap();
        assert_eq!(
            pending.structured_content.unwrap(),
            rmcp::serde_json::json!({
                "status": "pending", "edit_id": "edit-restart", "mode": "zed"
            })
        );
        fs::write(path.join("draft.md"), " after\n\nżółć\n")
            .await
            .unwrap();
        // saving alone must not complete an open editor
        assert_eq!(
            finish(&server, "edit-restart")
                .await
                .structured_content
                .unwrap()["status"],
            "pending"
        );
        fs::write(path.join("editor.done"), "0\n").await.unwrap();
        drop(server);
        let restarted = EditServer::new(root.path().into());
        let expected = rmcp::serde_json::json!({
            "status": "completed", "edit_id": "edit-restart", "mode": "zed",
            "text": " after\n\nżółć\n", "changed": true
        });
        for _ in 0..2 {
            let result = finish(&restarted, "edit-restart").await;
            assert_eq!(result.is_error, Some(false));
            assert_eq!(result.structured_content.unwrap(), expected);
        }
        fs::write(path.join("draft.md"), "before\n").await.unwrap();
        assert_eq!(
            finish(&restarted, "edit-restart")
                .await
                .structured_content
                .unwrap()["changed"],
            false
        );
    }

    #[tokio::test]
    async fn cancellation_survives_restart_and_overrides_later_editor_completion() {
        let root = tempfile::tempdir().unwrap();
        let path = stored_draft(root.path(), "edit-cancel").await;
        let server = EditServer::new(root.path().into());
        for _ in 0..2 {
            let result = server
                .cancel_edit(Parameters(EditIdRequest {
                    edit_id: "edit-cancel".into(),
                }))
                .await
                .unwrap();
            assert_eq!(
                result.structured_content.unwrap(),
                rmcp::serde_json::json!({
                    "status": "canceled", "edit_id": "edit-cancel"
                })
            );
        }
        fs::write(path.join("draft.md"), "must not be accepted")
            .await
            .unwrap();
        fs::write(path.join("editor.done"), "0\n").await.unwrap();
        drop(server);
        let restarted = EditServer::new(root.path().into());
        assert_eq!(
            finish(&restarted, "edit-cancel")
                .await
                .structured_content
                .unwrap(),
            rmcp::serde_json::json!({
                "status": "canceled", "edit_id": "edit-cancel"
            })
        );
        assert_eq!(
            fs::read_to_string(path.join("draft.md")).await.unwrap(),
            "must not be accepted"
        );
    }

    #[tokio::test]
    async fn rejects_invalid_ids_missing_edits_and_failed_editors() {
        let root = tempfile::tempdir().unwrap();
        let server = EditServer::new(root.path().into());
        for edit_id in [
            "../other",
            "edit-../other",
            "/tmp/edit-test",
            "edit-test/child",
        ] {
            assert_eq!(
                server.edit_path(edit_id).unwrap_err().code,
                ErrorCode::INVALID_PARAMS
            );
        }
        assert!(server
            .finish_edit(Parameters(EditIdRequest {
                edit_id: "edit-missing".into()
            }))
            .await
            .is_err());
        assert!(server
            .cancel_edit(Parameters(EditIdRequest {
                edit_id: "edit-missing".into()
            }))
            .await
            .is_err());
        let path = stored_draft(root.path(), "edit-failed").await;
        fs::write(path.join("editor.done"), "19\n").await.unwrap();
        let result = finish(&server, "edit-failed").await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap(),
            rmcp::serde_json::json!({
                "status": "failed", "edit_id": "edit-failed", "exit_code": 19
            })
        );
        fs::write(path.join("editor.done"), "invalid")
            .await
            .unwrap();
        assert!(server
            .finish_edit(Parameters(EditIdRequest {
                edit_id: "edit-failed".into()
            }))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn zed_launch_is_nonblocking_quotes_paths_and_waits_for_close() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("draft with spaces.md");
        let script = root.path().join("run-editor.sh");
        let zed = root.path().join("fake zed");
        let done = root.path().join("editor.done");
        let capture = root.path().join("args");
        let gate = root.path().join("closed");
        fs::write(&path, "before").await.unwrap();
        fs::write(&script, ZED_SCRIPT).await.unwrap();
        fs::write(
            &zed,
            r#"#!/usr/bin/env bash
root="${0%/*}"
printf '%s\n' "$@" > "$root/args"
while [ ! -f "$root/closed" ]; do sleep 0.02; done
printf 'saved\n\n' > "$3"
exit 0
"#,
        )
        .await
        .unwrap();
        fs::set_permissions(&zed, Permissions::from_mode(0o700))
            .await
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            launch_zed(&script, &path, &done, &zed),
        )
        .await
        .unwrap()
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !capture.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!done.exists());
        assert_eq!(
            fs::read_to_string(&capture).await.unwrap(),
            format!("--wait\n--new\n{}\n", path.display())
        );
        fs::write(gate, b"").await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !done.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(fs::read_to_string(&done).await.unwrap(), "0\n");
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "saved\n\n");
    }

    #[test]
    fn normalizes_purpose_for_the_pane_title() {
        assert_eq!(normalize_purpose(None), "proposed text");
        assert_eq!(
            normalize_purpose(Some("  PR   description\n")),
            "PR description"
        );
        assert_eq!(pane_title("PR description"), "Edit PR description");
        assert_eq!(
            pane_title(&"x".repeat(100)).chars().count(),
            MAX_PANE_TITLE_CHARS
        );
    }

    #[test]
    fn configures_neovim_from_the_draft_type() {
        assert_eq!(neovim_filetype("md"), "markdown");
        assert_eq!(neovim_filetype("txt"), "text");
        assert_eq!(neovim_filetype("gitcommit"), "gitcommit");
        assert_eq!(cursor_placement("PR title", "txt"), "start");
        assert_eq!(cursor_placement("commit message", "gitcommit"), "start");
        assert_eq!(cursor_placement("PR description", "md"), "end");
    }

    #[test]
    fn passes_filetype_and_cursor_to_neovim() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let text = tmp_dir.path().join("draft.md");
        let done = tmp_dir.path().join("done");
        let capture = tmp_dir.path().join("args");
        let editor = tmp_dir.path().join("nvim");
        // written and closed before it runs, linux refuses to exec a file
        // that is still open for writing
        std::fs::write(
            &editor,
            b"#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > \"$CAPTURE\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&editor, Permissions::from_mode(0o755)).unwrap();
        std::fs::write(&text, "draft").unwrap();

        let status = Command::new("bash")
            .arg("-c")
            .arg(EDITOR_SCRIPT)
            .arg("editor-script")
            .arg(&text)
            .arg(&done)
            .arg(&editor)
            .arg("markdown")
            .arg("end")
            .env("CAPTURE", &capture)
            .status()
            .unwrap();

        assert!(status.success());
        assert_eq!(std::fs::read_to_string(done).unwrap(), "0\n");
        assert_eq!(
            std::fs::read_to_string(capture).unwrap(),
            format!(
                "-c\nsetlocal filetype=markdown\n-c\nnormal! G$\n{}\n",
                text.display()
            )
        );
    }

    #[test]
    fn sizes_floating_panes_near_the_current_pane() {
        let launch = ZellijLaunch {
            script: Path::new("script"),
            text: Path::new("draft.md"),
            done: Path::new("done"),
            editor: "nvim",
            filetype: "markdown",
            cursor: "end",
            floating: true,
            session: "session",
            title: "Edit PR description",
        };
        let args: Vec<_> = zellij_args(&launch)
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect();

        assert!(args
            .windows(2)
            .any(|args| args == ["--width", FLOATING_WIDTH]));
        assert!(args
            .windows(2)
            .any(|args| args == ["--height", FLOATING_HEIGHT]));
        assert!(args
            .windows(2)
            .any(|args| args == ["--name", "Edit PR description"]));
        assert!(args.contains(&"--near-current-pane".into()));
    }

    #[test]
    fn returns_text_with_structured_edit_metadata() {
        let schema = rmcp::handler::server::tool::schema_for_type::<EditResult>();
        assert_eq!(schema.get("type").unwrap(), "object");
        let result = edit_result(EditResult::Completed {
            edit_id: "edit-test".into(),
            text: "after".into(),
            changed: true,
            mode: EditMode::ZellijFloating,
        });

        assert_eq!(result.is_error, Some(false));
        assert_eq!(
            result.structured_content.unwrap(),
            rmcp::serde_json::json!({
                "status": "completed",
                "edit_id": "edit-test",
                "text": "after",
                "changed": true,
                "mode": "zellij-floating"
            })
        );
    }

    #[derive(Default)]
    struct FakeAgent {
        created: Vec<rmcp::serde_json::Value>,
        status: rmcp::serde_json::Value,
        deleted: Vec<String>,
    }

    type SharedAgent = std::sync::Arc<std::sync::Mutex<FakeAgent>>;

    async fn fake_agent() -> (String, SharedAgent, tokio::task::JoinHandle<()>) {
        use axum::{
            extract::{Path as UrlPath, State},
            http::StatusCode,
            routing::{get, post},
            Json, Router,
        };
        use rmcp::serde_json::{json, Value};

        let agent = SharedAgent::default();
        agent.lock().unwrap().status = json!({"edit_id": "edit-agent-1", "status": "pending"});
        let app = Router::new()
            .route(
                "/v1/edits",
                post(
                    |State(agent): State<SharedAgent>, Json(body): Json<Value>| async move {
                        agent.lock().unwrap().created.push(body);
                        (
                            StatusCode::CREATED,
                            Json(json!({"edit_id": "edit-agent-1", "status": "pending"})),
                        )
                    },
                ),
            )
            .route(
                "/v1/edits/{id}",
                get(
                    |State(agent): State<SharedAgent>, UrlPath(id): UrlPath<String>| async move {
                        assert_eq!(id, "edit-agent-1");
                        Json(agent.lock().unwrap().status.clone())
                    },
                )
                .delete(
                    |State(agent): State<SharedAgent>, UrlPath(id): UrlPath<String>| async move {
                        agent.lock().unwrap().deleted.push(id);
                        Json(json!({"edit_id": "edit-agent-1", "status": "canceled"}))
                    },
                ),
            )
            .with_state(agent.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, agent, handle)
    }

    fn agent_server(root: &Path, url: &str) -> EditServer {
        let mut server = EditServer::new(root.into());
        server.agent_url = Some(url.into());
        server
    }

    async fn begin(server: &EditServer, text: &str) -> CallToolResult {
        server
            .begin_edit(Parameters(EditRequest {
                text: text.into(),
                purpose: Some("Linear comment".into()),
                file_extension: None,
                floating: None,
            }))
            .await
            .unwrap()
    }

    async fn begin_id(server: &EditServer, text: &str) -> String {
        begin(server, text).await.structured_content.unwrap()["edit_id"]
            .as_str()
            .unwrap()
            .into()
    }

    #[tokio::test]
    async fn agent_edits_complete_through_the_agent_and_stay_readable_offline() {
        use rmcp::serde_json::json;
        let root = tempfile::tempdir().unwrap();
        let (url, agent, handle) = fake_agent().await;
        let server = agent_server(root.path(), &url);

        let pending = begin(&server, "before\n").await.structured_content.unwrap();
        let edit_id = pending["edit_id"].as_str().unwrap().to_string();
        assert_eq!(
            pending,
            json!({"status": "pending", "edit_id": edit_id, "mode": "agent"})
        );
        assert_eq!(
            agent.lock().unwrap().created,
            vec![json!({"text": "before\n", "extension": "md", "purpose": "Linear comment"})]
        );
        assert_eq!(
            finish(&server, &edit_id).await.structured_content.unwrap()["status"],
            "pending"
        );

        agent.lock().unwrap().status = json!({
            "edit_id": "edit-agent-1", "status": "completed", "text": " after\n\nżółć\n", "changed": true
        });
        let expected = json!({
            "status": "completed", "edit_id": edit_id, "mode": "agent",
            "text": " after\n\nżółć\n", "changed": true
        });
        assert_eq!(
            finish(&server, &edit_id).await.structured_content.unwrap(),
            expected
        );
        assert_eq!(
            fs::read_to_string(root.path().join(&edit_id).join("draft.md"))
                .await
                .unwrap(),
            " after\n\nżółć\n"
        );

        // the result was mirrored locally, so it no longer needs the agent
        handle.abort();
        let _ = handle.await;
        let restarted = agent_server(root.path(), &url);
        assert_eq!(
            finish(&restarted, &edit_id)
                .await
                .structured_content
                .unwrap(),
            expected
        );
    }

    #[tokio::test]
    async fn unreachable_agent_keeps_edits_pending_and_refuses_new_ones() {
        let root = tempfile::tempdir().unwrap();
        let (url, _agent, handle) = fake_agent().await;
        let edit_id = begin_id(&agent_server(root.path(), &url), "before\n").await;
        handle.abort();
        let _ = handle.await;

        // a fresh client, so no pooled connection outlives the agent
        let server = agent_server(root.path(), &url);
        let pending = finish(&server, &edit_id).await.structured_content.unwrap();
        assert_eq!(pending["status"], "pending");
        assert!(pending["note"]
            .as_str()
            .unwrap()
            .starts_with("edit agent unreachable"));

        let refused = begin(&server, "x").await;
        assert_eq!(refused.is_error, Some(true));
        // a refused edit leaves no draft behind
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn agent_cancellation_and_failures_carry_over() {
        use rmcp::serde_json::json;
        let root = tempfile::tempdir().unwrap();
        let (url, agent, _handle) = fake_agent().await;
        let server = agent_server(root.path(), &url);

        let canceled = begin_id(&server, "before\n").await;
        let result = server
            .cancel_edit(Parameters(EditIdRequest {
                edit_id: canceled.clone(),
            }))
            .await
            .unwrap();
        assert_eq!(result.structured_content.unwrap()["status"], "canceled");
        assert_eq!(agent.lock().unwrap().deleted, vec!["edit-agent-1"]);
        agent.lock().unwrap().status = json!({"edit_id": "edit-agent-1", "status": "completed", "text": "must not be accepted"});
        assert_eq!(
            finish(&server, &canceled).await.structured_content.unwrap()["status"],
            "canceled"
        );

        let failed = begin_id(&server, "before\n").await;
        agent.lock().unwrap().status =
            json!({"edit_id": "edit-agent-1", "status": "failed", "exit_code": 3});
        let result = finish(&server, &failed).await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap(),
            json!({"status": "failed", "edit_id": failed, "exit_code": 3})
        );

        let agent_canceled = begin_id(&server, "before\n").await;
        agent.lock().unwrap().status = json!({"edit_id": "edit-agent-1", "status": "canceled"});
        assert_eq!(
            finish(&server, &agent_canceled)
                .await
                .structured_content
                .unwrap()["status"],
            "canceled"
        );
    }
}
