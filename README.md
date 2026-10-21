# edit-proposed-text

A stdio MCP server that opens agent-proposed text (PR descriptions, commit messages, comments) in your editor so you can revise it before it is used. Works with Claude Code, Amp and other MCP clients.

## Build

```bash
cargo build --release
```

Configure the resulting `target/release/edit-proposed-text-mcp` executable as a stdio MCP server, e.g. for Claude Code:

```bash
claude mcp add edit-proposed-text -- /path/to/target/release/edit-proposed-text-mcp
```

## Tools

- `begin_edit({text, purpose?, file_extension?, floating?})` opens a draft and immediately returns `{status: "pending", edit_id, mode}`.
- In Zellij, it uses the current session and the existing editor selection (`PI_EDIT_TEXT_EDITOR`, `VISUAL`, `EDITOR`, then `nvim`). Outside Zellij, including desktop-launched runners, it opens Zed with `--wait`. On macOS it checks standard CLI and app installation paths, so a desktop runner does not need Zed on its `PATH`.
- Save and close the draft tab (or exit the terminal editor), then tell the agent you're ready. `finish_edit({edit_id})` returns `completed` with the exact saved `text`, `changed`, and `mode`. Saving without closing is still `pending`.
- `finish_edit` never waits. If it returns `pending`, the agent should stop and wait for your reply, not poll in a long-running MCP call. There is no editing deadline, avoiding client-side tool call timeouts.
- `cancel_edit({edit_id})` permanently marks an edit `canceled`, even if the editor subsequently exits successfully. It does not close Zed, terminate the terminal editor, or delete the draft. An unsuccessful editor exit returns `failed` with its exit code.
- `edit_proposed_text` is a nonblocking alias for `begin_edit`: it returns an edit ID, not final text. Clients must call `finish_edit` before using the draft. Editing does not replace any required approval to post content.

## Storage

Drafts and metadata live in `~/.local/state/edit-proposed-text/` with private directory permissions. They survive MCP restarts, and results can be retrieved repeatedly. They are retained until you manually remove the corresponding `edit-*` directory; close its editor first. Restart or reload the MCP connection after rebuilding to discover the new tools.
