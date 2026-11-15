# Remote link

Lets one trusted machine on your tailnet see this pTerminal's workspaces and
send a message to an agent. It is **off by default** and **read-only by
default**.

## What a peer can and cannot do

| Can | Cannot |
|---|---|
| list workspaces, tabs and their status | type into a terminal |
| read the last screen lines of a tab | run a command or a script |
| read a workspace's `shared.md` | open, write or delete files |
| send a message to an agent or the orchestrator, **only if "Allow messages" is on** | change settings, make tokens, see the token |

A message is delivered by pTerminal's own messaging as
`[message from remote:<peer name>] …`. The agent then works under its own
permission mode, so your usual prompts still protect this machine.

Every request, allowed or refused, is logged in the window and in
`remote-audit.jsonl` next to `state.json` (time, peer address, path, result,
never the token or the message text).

## Turning it on (Windows, the machine being watched)

1. Install Tailscale from <https://tailscale.com/download> and sign in. Do
   the same on the other machine with the same account, so both appear in the
   same tailnet.
2. In pTerminal, click **Remote link** at the bottom of the left sidebar.
3. Click **Use my Tailscale IP** (or type it). pTerminal refuses any address
   that is not a Tailscale address (100.64.0.0/10, fd7a:115c:a1e0::/48) or
   loopback. It never listens on `0.0.0.0`.
4. Choose a peer name, for example `zerem`.
5. Click **Make a pairing token**, then copy the token. It is shown once; only
   its hash is kept. **Revoke** or making a new token cuts the old one off.
6. Tick **Remote link on**. Tick **Allow messages to agents** only if the peer
   should be able to send work.
7. If Windows Firewall asks, allow pTerminal on **Private** networks (the
   tailnet adapter).

## The peer (Linux)

```
pterminal remote pair http://100.x.y.z:47400 <token>   # saved to ~/.config/pterminal/remote-peer.json (mode 600)
pterminal remote ls
pterminal remote tail <workspace> <tab> [lines]
pterminal remote context <workspace>
pterminal remote send <workspace> <agent|orchestrator|all> <text…>
```

`PTERMINAL_REMOTE_URL` and `PTERMINAL_REMOTE_TOKEN` work instead of the file.
The client refuses to send the token to anything but a Tailscale or loopback
address.

## Guardrails (tests in `crates/pterminal-core/src/remote.rs`)

- never listens on all interfaces or off the tailnet (`RemoteConfig::check`, used by `start`)
- every route, and every unknown path, needs the token; off means off
- there is no route that runs or types anything (the route list is pinned)
- read-only link refuses messages; messages go only to agents, size-bounded
- tokens are random, stored as a SHA-256 hash, never in the audit log
