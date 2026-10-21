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
}

#[derive(Debug, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "lowercase")]
#[schemars(extend("type" = "object"))]
enum EditResult {
    Pending {
        edit_id: String,
        mode: EditMode,
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
}

#[derive(Debug, Clone)]
struct EditServer {
    tool_router: rmcp::handler::server::router::tool::ToolRouter<EditServer>,
    state_dir: PathBuf,
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

const ZED_SCRIPT: &str = r#"#!/bin/bash
"$3" --wait --new "$1"
exit_code=$?
printf "%s\n" "$exit_code" > "$2.tmp"
/bin/mv "$2.tmp" "$2"
"#;

async fn launch_zed(script: &Path, text: &Path, done: &Path, zed: &Path) -> std::io::Result<()> {
    let mut child = tokio::process::Command::new("/bin/bash")
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
/bin/mv "$done_file.tmp" "$done_file"
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
        description = "Open a persistent draft in a Zellij pane, or Zed when outside Zellij. Returns immediately with pending and an edit_id. Ask the user to save and close the editor tab, then reply when ready. Call finish_edit on that reply; do not wait or repeatedly poll in one tool call. Pending is not approval.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<EditResult>()
    )]
    async fn begin_edit(
        &self,
        Parameters(req): Parameters<EditRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let session = env::var("ZELLIJ_SESSION_NAME")
            .ok()
            .filter(|s| !s.is_empty());
        if let Some(session) = &session {
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
        let edit_id = tmp_dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let text_path = tmp_dir.path().join(format!("draft.{ext}"));
        let done_path = tmp_dir.path().join("editor.done");
        let script_path = tmp_dir.path().join("run-editor.sh");

        fs::write(&text_path, &req.text)
            .await
            .map_err(|e| internal_error("failed to write draft", e))?;
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
            session: session.as_deref().unwrap_or_default(),
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

        let stored = StoredEdit {
            original: req.text,
            extension: ext.into(),
            mode,
        };
        fs::write(
            tmp_dir.path().join("edit.json"),
            rmcp::serde_json::to_vec(&stored).unwrap(),
        )
        .await
        .map_err(|e| internal_error("failed to save edit metadata", e))?;
        let _ = tmp_dir.keep();
        tracing::info!("opened {purpose} as {edit_id}");
        Ok(edit_result(EditResult::Pending { edit_id, mode }))
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
        let exit_code_raw = match fs::read_to_string(path.join("editor.done")).await {
            Ok(value) => value,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(edit_result(EditResult::Pending {
                    edit_id: req.edit_id,
                    mode: stored.mode,
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
        self.load_edit(&path).await?;
        fs::write(path.join("canceled"), b"")
            .await
            .map_err(|e| internal_error("failed to cancel edit", e))?;
        Ok(edit_result(EditResult::Canceled {
            edit_id: req.edit_id,
        }))
    }
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
                 in Zellij or Zed. It returns immediately with an edit_id, not edited text. \
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
    let service = EditServer::new(state_dir).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, process::Command};

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
            r#"#!/bin/bash
root="${0%/*}"
printf '%s\n' "$@" > "$root/args"
while [ ! -f "$root/closed" ]; do /bin/sleep 0.02; done
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
        let mut editor_file = std::fs::File::create(&editor).unwrap();
        editor_file
            .write_all(b"#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > \"$CAPTURE\"\n")
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
}
