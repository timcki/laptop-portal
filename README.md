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
- In Zellij, it uses the current session and the existing editor selection (`PI_EDIT_TEXT_EDITOR`, `VISUAL`, `EDITOR`, then `nvim`). Outside Zellij, it sends the draft to the [edit agent](#edit-agent) when `EDIT_PROPOSED_TEXT_AGENT_URL` is set, and otherwise opens Zed with `--wait`. On macOS it checks standard CLI and app installation paths, so a desktop runner does not need Zed on its `PATH`.
- Save and close the draft tab (or exit the terminal editor), then tell the agent you're ready. `finish_edit({edit_id})` returns `completed` with the exact saved `text`, `changed`, and `mode`. Saving without closing is still `pending`.
- `finish_edit` never waits. If it returns `pending`, the agent should stop and wait for your reply, not poll in a long-running MCP call. There is no editing deadline, avoiding client-side tool call timeouts. A `pending` result may carry a `note`, e.g. when the edit agent is unreachable.
- `cancel_edit({edit_id})` permanently marks an edit `canceled`, even if the editor subsequently exits successfully. It does not close Zed, terminate the terminal editor, or delete the draft. An unsuccessful editor exit returns `failed` with its exit code.
- `edit_proposed_text` is a nonblocking alias for `begin_edit`: it returns an edit ID, not final text. Clients must call `finish_edit` before using the draft. Editing does not replace any required approval to post content.

## Storage

Drafts and metadata live in `~/.local/state/edit-proposed-text/` with private directory permissions. They survive MCP restarts, and results can be retrieved repeatedly. They are retained until you manually remove the corresponding `edit-*` directory; close its editor first. Restart or reload the MCP connection after rebuilding to discover the new tools.

## Edit agent

`agent/` holds `edit-proposed-text-agent`, a small Go service for when the MCP server runs on a headless machine and you edit on a laptop. It joins your tailnet as its own node through [tsnet](https://tailscale.com/docs/reference/tsnet-server-api), so it is reachable only over Tailscale and does not need Remote Login or an open port on the laptop. It opens each draft as a local file with `zed --wait --new` (the tab is named after the purpose) and keeps the result until the MCP server collects it, so an edit survives the laptop sleeping and MCP restarts.

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
go build -trimpath -o ~/.local/bin/edit-proposed-text-agent .
# first run: join the tailnet with a tagged auth key (or open the login URL it prints)
TS_AUTHKEY=tskey-auth-... ~/.local/bin/edit-proposed-text-agent
# then keep it running in your login session
cp xyz.timcki.edit-proposed-text-agent.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/xyz.timcki.edit-proposed-text-agent.plist
```

Flags: `-hostname` (default `edit-agent`), `-addr` (`:8080`), `-dir` (`~/Library/Application Support/edit-proposed-text-agent`), `-cap`, `-tag`, `-zed`. Logs go to `~/Library/Logs/edit-proposed-text-agent.log`. macOS may ask for Local Network access; without it traffic is relayed and still works.

### Use it from a server

```bash
claude mcp add edit-proposed-text -e EDIT_PROPOSED_TEXT_AGENT_URL=http://edit-agent:8080 -- /path/to/edit-proposed-text-mcp
curl http://edit-agent:8080/v1/health   # {"caller":"<this server>","ok":true}
```
