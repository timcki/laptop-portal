# laptop-portal

Lets coding agents on headless servers use your laptop's Zed over Tailscale. It has two parts:

- `laptop-portal-mcp`, a stdio MCP server for the agent's machine. It opens proposed text (PR descriptions, commit messages, comments) in an editor so you can revise it before it is used, and opens the server's own files in Zed for reviews. Works with Claude Code, Amp and other MCP clients.
- `laptop-portal-agent` (in `agent/`), a small service on the laptop that the server reaches over the tailnet. See [Edit agent](#edit-agent).

Without the agent, the MCP server still works locally in Zellij or Zed.

Some names predate the rename from `edit-proposed-text` and are kept so existing setups keep working: the `timcki.xyz/cap/edit-proposed-text` capability, the `edit-agent` node name, `EDIT_PROPOSED_TEXT_AGENT_URL`, the `edit_proposed_text` tool alias, and both state directories.

## Build

```bash
cargo build --release
```

Configure the resulting `target/release/laptop-portal-mcp` executable as a stdio MCP server, e.g. for Claude Code:

```bash
claude mcp add laptop-portal -- /path/to/target/release/laptop-portal-mcp
```

## Tools

- `begin_edit({text, purpose?, file_extension?, floating?})` opens a draft and immediately returns `{status: "pending", edit_id, mode}`.
- In Zellij, it uses the current session and the existing editor selection (`PI_EDIT_TEXT_EDITOR`, `VISUAL`, `EDITOR`, then `nvim`). Outside Zellij, it sends the draft to the [edit agent](#edit-agent) when `EDIT_PROPOSED_TEXT_AGENT_URL` is set, and otherwise opens Zed with `--wait`. On macOS it checks standard CLI and app installation paths, so a desktop runner does not need Zed on its `PATH`.
- Save and close the draft tab (or exit the terminal editor), then tell the agent you're ready. `finish_edit({edit_id})` returns `completed` with the exact saved `text`, `changed`, and `mode`. Saving without closing is still `pending`.
- `finish_edit` never waits. If it returns `pending`, the agent should stop and wait for your reply, not poll in a long-running MCP call. There is no editing deadline, avoiding client-side tool call timeouts. A `pending` result may carry a `note`, e.g. when the edit agent is unreachable.
- `laptop-portal-mcp wait <edit_id> [--timeout <secs>]` checks the edit every 2 seconds until it leaves `pending`, then prints the `finish_edit` result and exits 0, or exits 2 while still pending at the timeout (default one hour). Agents that can run a background command and get notified when it exits (Claude Code can) run it right after `begin_edit`, so closing the tab is enough and you don't have to reply. It reads the agent URL from the stored edit, so it needs no environment. The server instructions name the exact binary path.
- `open_in_zed({path, ssh_user?})` opens a directory or file on the server in the laptop's Zed over SSH, for reviews. The edit agent builds `ssh://<user>@<caller>/<path>` from the calling server's own tailnet name, so a server can only open itself. Without an agent, or if it fails, nothing opens and the result is a `zed://ssh/...` link to click instead.
- `cancel_edit({edit_id})` permanently marks an edit `canceled`, even if the editor subsequently exits successfully. It does not close Zed, terminate the terminal editor, or delete the draft. An unsuccessful editor exit returns `failed` with its exit code.
- `edit_proposed_text` is a nonblocking alias for `begin_edit`: it returns an edit ID, not final text. Clients must call `finish_edit` before using the draft. Editing does not replace any required approval to post content.

## Storage

Drafts and metadata live in `~/.local/state/edit-proposed-text/` with private directory permissions. They survive MCP restarts, and results can be retrieved repeatedly. They are retained until you manually remove the corresponding `edit-*` directory; close its editor first. Restart or reload the MCP connection after rebuilding to discover the new tools.

## Edit agent

`agent/` holds `laptop-portal-agent`, a small Go service for when the MCP server runs on a headless machine and you edit on a laptop. It joins your tailnet as its own node through [tsnet](https://tailscale.com/docs/reference/tsnet-server-api), so it is reachable only over Tailscale and does not need Remote Login or an open port on the laptop. It opens each draft as a local file with `zed --wait --new` (the tab is named after the purpose), opens a server's own files over SSH for `open_in_zed`, and keeps the result until the MCP server collects it, so an edit survives the laptop sleeping and MCP restarts.

Only tailnet peers granted the `timcki.xyz/cap/edit-proposed-text` capability can use it, checked per request with the tailnet's WhoIs, and each server can only read its own edits. Adding a server is a policy change: tag it `tag:claude-server`. Traffic is plain HTTP inside WireGuard, which already encrypts and authenticates both ends.

### Tailnet policy

```jsonc
"tagOwners": {
  "tag:edit-agent":    ["autogroup:admin"],
  "tag:claude-server": ["autogroup:admin"],
},
"grants": [
  {
    "src": ["tag:claude-server"],
    "dst": ["tag:edit-agent"],
    "ip":  ["tcp:8080"],
    "app": { "timcki.xyz/cap/edit-proposed-text": [{}] },
  },
],
```

If the policy still has an allow-all rule, other devices can connect to the port but are refused without the capability.

### Install on the laptop

[`agent/LAPTOP_SETUP.md`](agent/LAPTOP_SETUP.md) is a step-by-step version for an agent running on the laptop, logging in through a URL instead of an auth key.

```bash
cd agent
go build -trimpath -o ~/.local/bin/laptop-portal-agent .
# first run: join the tailnet with a tagged auth key (or open the login URL it prints)
TS_AUTHKEY=tskey-auth-... ~/.local/bin/laptop-portal-agent
# then keep it running in your login session
cp xyz.timcki.laptop-portal-agent.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/xyz.timcki.laptop-portal-agent.plist
```

Flags: `-hostname` (default `edit-agent`), `-addr` (`:8080`), `-dir` (`~/Library/Application Support/edit-proposed-text-agent`), `-cap`, `-tag`, `-zed`. Logs go to `~/Library/Logs/laptop-portal-agent.log`. macOS may ask for Local Network access; without it traffic is relayed and still works.

### Use it from a server

```bash
claude mcp add laptop-portal -e EDIT_PROPOSED_TEXT_AGENT_URL=http://edit-agent:8080 -- /path/to/laptop-portal-mcp
curl http://edit-agent:8080/v1/health   # {"caller":"<this server>","ok":true}
```
