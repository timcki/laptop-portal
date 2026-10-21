# Laptop setup: laptop-portal-agent

Instructions for a coding agent running on the user's Mac. The goal: run `laptop-portal-agent` on this laptop so Claude Code sessions on tailnet servers (first: `bigchungus`) can open proposed text (PR bodies, Linear comments, commit messages) in the laptop's Zed and get the edited text back.

How it works, briefly: the agent joins the tailnet as its own node `edit-agent` (via tsnet, independent of the Tailscale app), listens only on the tailnet at `:8080`, serves only peers the tailnet policy grants `timcki.xyz/cap/edit-proposed-text`, opens each draft as a local file with `zed --wait --new`, and keeps the result until the server collects it. Full design: `README.md` in the same repo, section "Edit agent".

## Ground rules

- **Ask the user before**: editing the tailnet policy, anything that needs an admin console click, installing anything outside `~/.local/bin`, `~/Library/LaunchAgents` and `~/Library/Application Support/edit-proposed-text-agent`.
- **Do not** enable Remote Login / sshd, change macOS firewall settings, or touch the Tailscale app's own configuration.
- **Do not** create, request, print or store Tailscale auth keys. This setup logs the node in through a login URL the user opens instead.
- Show the user each step's verification output. If a check fails, stop and report it rather than improvising a workaround.

## Facts

| | |
|---|---|
| Laptop | macOS, Apple Silicon, Tailscale **standalone** app, Remote Login **off** |
| Server | `bigchungus` (NixOS, user `amp`), repo at `~/Repos/laptop-portal` |
| Source | https://github.com/timcki/laptop-portal (public), agent in `agent/` |
| Go | 1.26.6 or newer (`agent/go.mod`) |
| Node name / port | `edit-agent`, `tcp:8080` |
| Capability | `timcki.xyz/cap/edit-proposed-text` |
| Tag | `tag:edit-agent` |
| Tailscale CLI | `/Applications/Tailscale.app/Contents/MacOS/Tailscale` (alias it as `tailscale` below) |


## 1. Preflight

```bash
alias tailscale=/Applications/Tailscale.app/Contents/MacOS/Tailscale
uname -m                                   # expect arm64
whoami; echo "$HOME"                       # the plist assumes /Users/tim; adjust in step 4 if not
tailscale status | grep -i bigchungus      # bigchungus must be online
tailscale ip -4 bigchungus                 # note this 100.x address for step 2
ls /Applications/Zed.app/Contents/MacOS/cli || ls "/Applications/Zed Preview.app/Contents/MacOS/cli"
go version                                 # need 1.26.6+; if missing or older, ask the user before installing (e.g. brew install go)
tailscale status | grep -i edit-agent      # expect nothing; if a node already exists, stop and ask
```

## 2. Tailnet policy (user does this, you prepare it)

Show the user this snippet, filled in with bigchungus's address from step 1, and ask them to merge it into the policy at https://login.tailscale.com/admin/acls/file. Merge into existing `tagOwners` / `hosts` / `grants` keys if they exist; do not remove anything. Ask them to save and confirm.

```jsonc
"tagOwners": {
  "tag:edit-agent": ["autogroup:admin"],
},
"hosts": {
  "bigchungus": "100.x.y.z",
},
"grants": [
  {
    "src": ["bigchungus"],
    "dst": ["tag:edit-agent"],
    "ip":  ["tcp:8080"],
    "app": { "timcki.xyz/cap/edit-proposed-text": [{}] },
  },
],
```

Notes to pass on:
- This must be saved **before** step 5: the node advertises `tag:edit-agent`, and login fails with "requested tags are invalid or not permitted" if the tag has no owner.
- Using bigchungus's IP keeps its current ownership. To add servers later, either add them to `src`, or introduce `tag:claude-server` (tagging a server removes its user identity, so rules that name the user for it then need the tag).
- If the policy still has an allow-all rule, other devices can reach port 8080 but are refused without the capability.

## 3. Build the binary from source

```bash
git clone https://github.com/timcki/laptop-portal ~/Repos/laptop-portal   # or git pull if it exists
cd ~/Repos/laptop-portal/agent
git log -1 --oneline                                   # report the commit you built
go test ./...                                          # must pass
mkdir -p ~/.local/bin
go build -trimpath -o ~/.local/bin/laptop-portal-agent .
file ~/.local/bin/laptop-portal-agent             # Mach-O 64-bit executable arm64
codesign -dv ~/.local/bin/laptop-portal-agent 2>&1 | grep -i signature   # adhoc is expected
```

If the user declines installing Go, a prebuilt binary is on the server at `amp@bigchungus:Repos/laptop-portal/agent/dist/laptop-portal-agent-darwin-arm64`; `scp` it to `~/.local/bin/laptop-portal-agent`, `chmod 755` it, and check `shasum -a 256` against the checksum the user gives you.

## 4. Install the launchd agent (do not load it yet)

```bash
cp ~/Repos/laptop-portal/agent/xyz.timcki.laptop-portal-agent.plist ~/Library/LaunchAgents/
# only if $HOME is not /Users/tim:
sed -i '' "s|/Users/tim|$HOME|g" ~/Library/LaunchAgents/xyz.timcki.laptop-portal-agent.plist
plutil -lint ~/Library/LaunchAgents/xyz.timcki.laptop-portal-agent.plist
```

## 5. First run: join the tailnet

Run it in the foreground once, so the login URL is visible:

```bash
~/.local/bin/laptop-portal-agent
```

- Every few seconds it prints `To start this tsnet server, restart with TS_AUTHKEY set, or go to: https://login.tailscale.com/a/...`. Ignore the auth key part; give the user that URL to open and approve, logged in as a tailnet admin.
- macOS may ask to allow **Local Network** access. Tell the user allowing it gives direct connections; denying still works through relays.
- Wait for `serving ... on edit-agent:8080 with /Applications/Zed.app/Contents/MacOS/cli`, then stop it with Ctrl-C. The node state is saved in `~/Library/Application Support/edit-proposed-text-agent/tsnet`, so later starts need no login.

Verify:

```bash
tailscale status | grep edit-agent       # node present
```

Ask the user to check in the admin console (Machines) that `edit-agent` shows the tag `tag:edit-agent`. If it is untagged, the policy from step 2 was not saved first: delete the node in the console, remove `~/Library/Application Support/edit-proposed-text-agent/tsnet`, and redo step 5.

## 6. Start it under launchd

```bash
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/xyz.timcki.laptop-portal-agent.plist
launchctl print gui/$(id -u)/xyz.timcki.laptop-portal-agent | grep -E 'state|pid'   # state = running
tail -n 20 ~/Library/Logs/laptop-portal-agent.log                                     # "serving ..."
```

## 7. Verify access control from the laptop

The laptop's own Tailscale node is **not** granted the capability, so it must be refused. This proves the check works:

```bash
curl -s --max-time 5 http://edit-agent:8080/v1/health
# expect: {"error":"peer lacks timcki.xyz/cap/edit-proposed-text"}
```

A connection error instead means the node is down or the grant's `ip` rule blocks the laptop entirely; the latter is also acceptable (report which one you saw). An `{"ok":true,...}` answer means the grant is broader than intended: stop and tell the user.

## 8. Hand back to the server

Report to the user: the step 7 result, the node's tag, and the log tail. The server side is done from bigchungus, not here:

```bash
# on bigchungus
curl -s http://edit-agent:8080/v1/health   # expect {"caller":"bigchungus...","ok":true}
claude mcp add laptop-portal -e EDIT_PROPOSED_TEXT_AGENT_URL=http://edit-agent:8080 \
  -- ~/Repos/laptop-portal/target/release/laptop-portal-mcp
```

The first real draft opens a new Zed window on the laptop named after its purpose (for example `linear-comment-int-3919.md`). Save and close the tab to hand the text back.

## Migrating from edit-proposed-text

If the agent was installed before the repo was renamed (binary `edit-proposed-text-agent`, label `xyz.timcki.edit-proposed-text-agent`), switch it over. The tailnet node, its login and open drafts stay, because the state directory keeps its old name:

```bash
launchctl bootout gui/$(id -u)/xyz.timcki.edit-proposed-text-agent
cd ~/Repos && mv edit-proposed-text laptop-portal && cd laptop-portal
git remote set-url origin https://github.com/timcki/laptop-portal && git pull
cd agent && go test ./... && go build -trimpath -o ~/.local/bin/laptop-portal-agent .
cp xyz.timcki.laptop-portal-agent.plist ~/Library/LaunchAgents/   # adjust /Users/tim as in step 4
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/xyz.timcki.laptop-portal-agent.plist
rm ~/Library/LaunchAgents/xyz.timcki.edit-proposed-text-agent.plist ~/.local/bin/edit-proposed-text-agent
```

Then check step 6 and step 7 as for a new install. The old log `~/Library/Logs/edit-proposed-text-agent.log` can be deleted.

## Operations

- Logs: `~/Library/Logs/laptop-portal-agent.log`; add `-verbose` to the plist's `ProgramArguments` for tsnet internals.
- Restart: `launchctl kickstart -k gui/$(id -u)/xyz.timcki.laptop-portal-agent`. Drafts still open at a restart come back to the server as `failed`, and the server re-sends them.
- Update: `git pull` in `~/Repos/laptop-portal`, rerun the `go test` and `go build` from step 3, then restart.
- Uninstall: `launchctl bootout gui/$(id -u)/xyz.timcki.laptop-portal-agent`, delete the binary, the plist and `~/Library/Application Support/edit-proposed-text-agent`, then ask the user to remove the `edit-agent` machine and the policy entries in the admin console.
