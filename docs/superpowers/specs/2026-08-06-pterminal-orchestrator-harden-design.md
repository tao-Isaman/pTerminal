# pTerminal Orchestrator Hardening — Design Spec

**Date:** 2026-08-06
**Status:** Approved by user
**Baseline:** master @ fe0dd60 (editor + orchestrator merged)

Two capabilities that give the orchestrator more agency without letting it spawn/close
agents (deliberately out of scope). Both extend machinery from the editor+orchestrator
feature (the `resolve_target` resolver and the `status.md` generator).

---

## Feature 1: Broadcast messaging

Two new address patterns in the message protocol, understood by BOTH the orchestrator's
outbox and every workspace agent's outbox, with reach rules that keep cross-workspace
delivery an orchestrator privilege.

**Patterns**
- `all`
  - Orchestrator outbox → every non-exited AGENT tab in every REAL workspace (orchestrator
    excluded).
  - Real-workspace outbox → every non-exited agent tab in THAT workspace, EXCLUDING the
    sender (matched by `from` == tab title). Local peer broadcast.
- `<workspace>/*`
  - Resolves to every non-exited agent tab in the named workspace.
  - Orchestrator outbox: any workspace.
  - Real-workspace outbox: allowed only when `<workspace>` is the sender's own workspace
    name; targeting another workspace → undeliverable banner (agents don't reach across
    workspaces — that's the orchestrator's role).

**Resolver.** The existing pure `resolve_target` gains:
- a result variant `Broadcast(Vec<(ws_index, tab_index)>)` (possibly empty),
- a `Sender` parameter: `enum Sender { Orchestrator, Workspace { index: usize, from: String } }`,
so the reach rules are pure and unit-testable. Single-target rules (`orchestrator`,
`<ws>/<agent>`, bare-unique, ambiguous, unknown, self-exclusion) are unchanged.

**Delivery.** For `Broadcast`, iterate the target list and, for each, type
`[broadcast from <from>] <flattened text>` into that agent's PTY and queue the submit Enter
via the existing `pending_submit` two-step mechanism (never inline `\r`). Zero matches → an
informational error-banner line ("broadcast to '<to>': no matching agents"), NOT a crash;
the message offset still advances (successful parse). Malformed/offset handling unchanged.

**Reserved names.** `unique_title` never assigns `all` (nor the existing `orchestrator`) to
a real agent; `*` and `/*` are patterns, not titles.

---

## Feature 2: Richer live status

`status.md` (generated for the orchestrator, shown in its F2 panel) gains, without changing
its change-detected/debounced write model:

**Per agent line:**
`- <workspace>/<title> — <status> — cwd <cwd> — <N> subagents — last active <HH:MM:SS>`
- `<N> subagents` = count of that tab's currently-running children (`done_at.is_none()`);
  omitted or "0 subagents" when none (pick "0 subagents" for a stable format).
- `last active <HH:MM:SS>` = the wall-clock time of the agent's last status change, printed
  as absolute UTC `HH:MM:SS`. Stored on the tab and updated only when status changes, so the
  generated string does NOT churn every frame (critical: a relative "Ns ago" would rewrite
  status.md every second and defeat the debounce).

**Per workspace:** under the `## <name>  (<path>)` header, a line
`shared.md: <excerpt>` where excerpt = the last ~200 chars of that workspace's shared.md
with newlines flattened to spaces and trimmed; empty/absent shared.md → `shared.md: (empty)`.

**Plumbing.** `Tab` gains `last_activity: std::time::SystemTime` (set at spawn, updated in
`drain_events` when the parsed status differs from the tab's prior status). The status
formatter's `WsStatus`/agent tuple grows the two fields; a per-workspace `shared_excerpt`
is read in `refresh_orchestrator_status` (read head/tail of shared.md, truncate+flatten).
UTC `HH:MM:SS` is computed from `SystemTime` via `duration_since(UNIX_EPOCH)` arithmetic
(secs % 86400 → h/m/s) — no new dependency.

---

## Error handling

- Broadcast with no matches → informational banner, offset advances, no crash.
- A real-workspace agent aiming `<other-workspace>/*` → undeliverable banner, not delivered.
- shared.md read failure for the excerpt → `shared.md: (unavailable)`, status generation
  continues for other workspaces.
- Nothing here runs git or deletes files; broadcast only types into existing PTYs.

## Testing

- `resolve_target` broadcast matrix (pure): `all` from orchestrator → all real agents;
  `all` from a workspace → that workspace's agents minus sender; `<ws>/*` from orchestrator →
  that workspace's agents; `<ws>/*` from a different workspace's agent → Unknown/denied;
  exited agents excluded; empty result when no agents.
- `unique_title` reserves `all`.
- status formatter: subagent count, `HH:MM:SS` rendering from a fixed SystemTime, excerpt
  truncation+flatten, empty-shared.md placeholder; churn check (same status → identical
  string).
- Manual: orchestrator `all` reaches every agent; a workspace agent's `all` reaches its
  peers not itself and not other workspaces; `<ws>/*` from orchestrator; status.md shows
  subagent count, last-active time, and a shared.md excerpt that updates when an agent
  appends.

## Out of scope (deliberate)

Orchestrator spawning/closing agents or workspaces; broadcast acknowledgements/read
receipts; cross-workspace broadcast from a plain agent; relative "N seconds ago" activity
(absolute time only, for churn-free writes); status history/graphs.
