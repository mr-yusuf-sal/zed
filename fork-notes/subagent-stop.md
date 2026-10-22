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

## In progress: a stop button for each subagent in the running work menu

The protocol already supports this. When a subagent session is announced with
`capabilities.cancel`, a `session/cancel` sent with that subagent's session id stops only
that subagent. Claude Code calls `query.stopTask(taskId)`, and the subagent then reports
`idle` with `stopReason: cancelled`.

### Committed so far (it compiles, but nothing calls it yet)

- `crates/acp_thread/src/connection.rs`: the `AgentConnection` trait gets
  `can_stop_subagent(session_id, tool_call_id) -> bool` and
  `stop_subagent(session_id, tool_call_id, cx)`. By default they report `false` and do
  nothing.
- `crates/acp_thread/src/acp_thread.rs`: `AcpThread::can_stop_subagent` and
  `AcpThread::stop_subagent` forward to the connection, with the thread's own session id.

### Still to do

1. `crates/agent_servers/src/acp.rs`. `fork-notes/subagent-stop-acp-edit.py` is a draft
   of this step. Run it from the repo root, but fix two things in it first:
   - Delete its `connection.rs` and `acp_thread.rs` blocks. Those edits are already
     committed.
   - The `ClientContext` replacement that ends in `async move |cx| {` matches twice. Anchor
     it on the line `// Set up the foreground dispatch loop to process work items from
     handlers.` instead.
   - The two test helpers that build a `ClientContext` (around `connect_fake_agent_with_handle`
     and the other fake connect helper) need
     `let subagents = Rc::new(RefCell::new(HashMap::default()));` before it. Their
     `new_for_test(client_conn, sessions, agent_capabilities, …)` calls get `subagents`
     after `sessions`. The config-option test helper passes `Rc::default()` there.

   What the step does:
   - `ClientContext.subagents` becomes `Rc<RefCell<HashMap<SessionId, SubagentSession>>>`,
     shared with a new `AcpConnection.subagents` field.
   - `SubagentSession` gets a `cancellable: bool` field. It is set from
     `update.capabilities.cancel`, which only the first announcement carries.
   - `root_session_id` is split so that the root can be found from the map alone
     (`root_subagent_session`).
   - `AcpConnection::cancellable_subagent(session_id, tool_call_id)` finds the running,
     cancellable subagent whose `tool_call_id` matches and whose root is `session_id`.
   - `AcpConnection` implements `can_stop_subagent` and `stop_subagent`. The stop sends
     `acp::CancelNotification::new(subagent_id)`.
   - The forwarding test wrapper (`self.inner.cancel(...)`) delegates both methods.
2. `crates/agent_ui/src/conversation_view/thread_view.rs`:
   - `running_subagents` also returns `thread.can_stop_subagent(&tool_call.id)`.
   - In `render_running_work`, a subagent row that can be stopped uses
     `ContextMenu::entry_with_end_slot(label, None, scroll_to, IconName::Stop, "Stop Subagent".into(), stop)`.
     The `stop` handler calls `thread.update(cx, |thread, cx| thread.stop_subagent(&tool_call_id, cx))`.
3. Add a test next to `test_subagent_sessions_show_in_their_parent_session`:
   - Announce the subagent with `"capabilities": {"cancel": {}}`.
   - Check that `can_stop_subagent` is true.
   - Call `stop_subagent` and check that the fake agent receives a `session/cancel` whose
     `sessionId` is the subagent's session.
4. Run `cargo check`, then `cargo test -p agent_servers -p acp_thread -p agent_host`, then
   clippy. `script/clippy` stops on `disallowed_methods` lints that `agent_host` already
   had; run `cargo clippy ... -- --deny warnings -A clippy::disallowed_methods`.
5. Commit, push, then `tools\ZedFork\build.ps1 -Install -Publish`.

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
