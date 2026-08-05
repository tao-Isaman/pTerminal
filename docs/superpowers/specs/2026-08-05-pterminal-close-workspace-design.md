# pTerminal Close Workspace — Design Spec

**Date:** 2026-08-05
**Status:** Approved by user (confirm-then-kill variant)
**Baseline:** master @ 1b8961c

## What

Right-click a workspace row in the sidebar → **Close workspace** → one confirmation dialog
→ the workspace and all its tabs are removed from pTerminal. "Forget", never destroy.

## Semantics

- Tab processes are dropped (PTY teardown handles threads). Nothing on disk changes:
  no worktree removal, no branch deletion, no file edits. Git state untouchable from this
  path by construction.
- The workspace leaves `state.json` entirely (saved_tabs, kept_worktrees, msg_offset,
  active_tab). Agent sessions remain resumable later via `pterminal resume --id <sid>`.
- Works with running tabs: the confirmation dialog states the consequences
  (N running tabs terminated — processes only; worktrees stay on disk, reminders
  forgotten; sessions resumable via the resume CLI).
- Confirmation dialog is identity-tracked (workspace resolved safely if state shifts
  while it is open) and follows the existing dialog conventions (error dialog wins,
  mouse/keyboard guards).

## Index-shift hardening (the debt this feature forces)

On removal: `active_ws` re-pointed/clamped; ALL transient index- or selection-carrying
state cleared (`new_tab`, `closing`, `selected_child`, `pending_claim`,
`pending_folder_pick` result handling unaffected but drafts dropped); index-keyed maps
`roster_written` and `partial_pending` cleared (self-repopulate next frame);
`pending_submit` entries for the dead tabs drop via the existing tab-gone rule; watcher
rebuilt without the closed workspace's `.pterminal` dir.

## Re-add rule

`finish_add_workspace` initializes `msg_offset` to the CURRENT byte length of the repo's
`messages.jsonl` (0 when absent) — a re-added workspace must not replay message history
into fresh agents.

## Testing

Unit tests on the removal state-mutation (workspace gone from state, active_ws clamping,
transient state cleared) and the re-add msg_offset rule; UI (context menu + dialog)
verified live with screenshots.

## Out of scope (deliberate)

Deleting worktrees/branches/files from disk; bulk close-all; undo/reopen-last;
drag-reorder of workspaces.
