# Running subagents and background processes: status

Last updated: 2026-10-22.

## Done

- `5900f201e5` agent_ui: when both kinds are running, the running work button reads
  `1 sub · 1 bg`, so the toolbar no longer wraps.
- `e3eddba96a` agent_servers: a subagent launched in the background stays counted
  until Claude Code reports that it stopped.
  - Zed advertises `clientCapabilities.subagents: {}` to Claude Code. `initialize` goes
    out as an untyped message because the protocol crate has no field for it.
  - An untyped handler takes the `subagent_update` and `session_message` updates.
  - The tool calls of a subagent session are moved into the root session, tagged with
    `_meta.claudeCode.parentToolUseId`.
  - The agent host routes the requests of a subagent session to the owner of its root
    session.
  - Checked live on 2026-10-22: the count stays after the Agent tool call completes.
- `tools` repo `7418aa7`: `build.ps1 -Install` no longer kills the agent hosts
  (`zed --agent-host`). It moves the running exe aside, as `install-detached.ps1` does,
  and it publishes before installing.
- The release `custom-811f89c6…` had been left as a draft by an interrupted publish.
  It was published and is now the latest.
- agent_ui: each subagent in the running work menu that Claude Code can stop on its own
  has a Stop button. It stops only that subagent, and the turn goes on.
  - Claude Code announces a subagent with `capabilities: { cancel: {} }` in its first
    `subagent_update`. Zed records that. The stop sends `session/cancel` with the
    subagent's session id, Claude Code calls `query.stopTask` for that subagent, and the
    subagent reports `idle` with `stopReason: cancelled`.
  - Checked against the source of `@agentclientprotocol/claude-agent-acp` 0.94.0
    (`announceNativeSubagent`, `cancelChild`). The agent host passes `session/cancel`
    on to the agent unchanged.
  - The button is the menu entry's end slot. `ContextMenu` shows an end slot only when
    the menu has an end slot action, so the menu sets `menu::EndSlot`, which has no key
    binding. A row with a Stop button has no thread icon, because `entry_with_end_slot`
    takes none.
  - `test_stopping_a_subagent_cancels_only_its_session` checks that the fake agent gets
    a `session/cancel` for the subagent's session and none for the parent's. fmt, check,
    the tests of `agent_servers`, `acp_thread` and `agent_host`, and clippy pass on
    Linux. Clippy ran with `-- --deny warnings -A clippy::disallowed_methods`, because
    `agent_host` already had `disallowed_methods` lints.
  - Not tried against a live Claude Code, and the menu has not been seen on screen.

## Still to do

- Push, then run `tools\ZedFork\build.ps1 -Install -Publish`, and try the Stop button on a
  running subagent.

## Not possible cleanly: stopping a background bash process

Claude Code has a method for this, `_session/async_task/stop`. It only works when the
`asyncTasks` runtime is on, and that runtime is on only for a client that identifies itself
as JetBrains AIR (`clientCapabilities._meta.jetbrains.air`). Identifying as AIR changes more
than 30 behaviours in the adapter, among them the subagent form (`air` instead of `rfd`),
elicitations, modes and tool rendering. That is too broad to turn on only to stop
processes. The options are:

- Ask the agent to stop the process. The model has its own tools for this.
- Have Zed kill the process tree of the command at the OS level. This is fragile, because
  the command runs under `claude.exe` and Zed only has its display terminal.
- Wait until the stop is available without AIR, either upstream in
  `@agentclientprotocol/claude-agent-acp` or through an ACP RFD.
