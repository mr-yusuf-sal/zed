import re


def edit(path, replacements):
    s = open(path, encoding='utf-8').read()
    for old, new, count in replacements:
        assert s.count(old) == count, (path, old[:80], s.count(old))
        s = s.replace(old, new)
    open(path, 'w', encoding='utf-8', newline='').write(s)


edit('crates/agent_servers/src/acp.rs', [
    ('''    request_elicitations: Entity<ElicitationStore>,
    subagents: RefCell<HashMap<acp::SessionId, SubagentSession>>,
}
''', '''    request_elicitations: Entity<ElicitationStore>,
    subagents: Rc<RefCell<HashMap<acp::SessionId, SubagentSession>>>,
}
''', 1),
    ('''    tool_call_id: Option<acp::ToolCallId>,
    running: bool,
}
''', '''    tool_call_id: Option<acp::ToolCallId>,
    running: bool,
    /// Whether the agent stops the subagent alone when it gets the
    /// subagent's session in `session/cancel`.
    cancellable: bool,
}
''', 1),
    # AcpConnection field
    ('''pub struct AcpConnection {
    id: AgentId,
''', '''pub struct AcpConnection {
    id: AgentId,
    subagents: Rc<RefCell<HashMap<acp::SessionId, SubagentSession>>>,
''', 1),
    # stdio: create shared map
    ('''        let request_elicitations = cx.new(|_| ElicitationStore::default());

        // Set up the foreground dispatch channel for bridging Send handler
''', '''        let request_elicitations = cx.new(|_| ElicitationStore::default());
        let subagents = Rc::new(RefCell::new(HashMap::default()));

        // Set up the foreground dispatch channel for bridging Send handler
''', 1),
    ('''        // Set up the foreground dispatch loop to process work items from handlers.
        let dispatch_context = ClientContext {
            sessions: sessions.clone(),
            session_list: client_session_list.clone(),
            request_elicitations: request_elicitations.clone(),
            subagents: RefCell::default(),
        };''', '''        // Set up the foreground dispatch loop to process work items from handlers.
        let dispatch_context = ClientContext {
            sessions: sessions.clone(),
            session_list: client_session_list.clone(),
            request_elicitations: request_elicitations.clone(),
            subagents: subagents.clone(),
        };''', 1),
    ('''        Ok(Self {
            id: agent_id,
            auth_methods,
''', '''        Ok(Self {
            id: agent_id,
            subagents,
            auth_methods,
''', 1),
    # new_for_test
    ('''    fn new_for_test(
        connection: ConnectionTo<Agent>,
        sessions: Rc<RefCell<HashMap<acp::SessionId, AcpSession>>>,
''', '''    fn new_for_test(
        connection: ConnectionTo<Agent>,
        sessions: Rc<RefCell<HashMap<acp::SessionId, AcpSession>>>,
        subagents: Rc<RefCell<HashMap<acp::SessionId, SubagentSession>>>,
''', 1),
    ('''        Self {
            id: agent_id,
            telemetry_id: "test".into(),
''', '''        Self {
            id: agent_id,
            subagents,
            telemetry_id: "test".into(),
''', 1),
    # the two remaining ClientContext sites in tests share a map with their connection
    ('''            request_elicitations: request_elicitations.clone(),
            subagents: RefCell::default(),
        };''', '''            request_elicitations: request_elicitations.clone(),
            subagents: subagents.clone(),
        };''', 2),
    ('''            AcpConnection::new_for_test(
                client_conn,
                sessions,
                agent_capabilities,
''', '''            AcpConnection::new_for_test(
                client_conn,
                sessions,
                subagents,
                agent_capabilities,
''', 2),
    ('''        let request_elicitations = cx.new(|_| ElicitationStore::default());
        let dispatch_context = ClientContext {''', '''        let request_elicitations = cx.new(|_| ElicitationStore::default());
        let subagents = Rc::new(RefCell::new(HashMap::default()));
        let dispatch_context = ClientContext {''', 2),
    ('''            AcpConnection::new_for_test(
                client_conn,
                sessions,
                acp::AgentCapabilities::default(),
''', '''            AcpConnection::new_for_test(
                client_conn,
                sessions,
                Rc::default(),
                acp::AgentCapabilities::default(),
''', 1),
    # cancel impl: add stop methods
    ('''        let params = acp::CancelNotification::new(session_id.clone());
        self.connection.send_notification(params).log_err();
    }

    fn request_elicitations(&self) -> Option<Entity<ElicitationStore>> {
        Some(self.request_elicitations.clone())
    }
''', '''        let params = acp::CancelNotification::new(session_id.clone());
        self.connection.send_notification(params).log_err();
    }

    fn can_stop_subagent(&self, session_id: &acp::SessionId, tool_call_id: &acp::ToolCallId) -> bool {
        self.cancellable_subagent(session_id, tool_call_id)
            .is_some()
    }

    fn stop_subagent(
        &self,
        session_id: &acp::SessionId,
        tool_call_id: &acp::ToolCallId,
        _cx: &mut App,
    ) {
        if let Some(subagent_id) = self.cancellable_subagent(session_id, tool_call_id) {
            self.connection
                .send_notification(acp::CancelNotification::new(subagent_id))
                .log_err();
        }
    }

    fn request_elicitations(&self) -> Option<Entity<ElicitationStore>> {
        Some(self.request_elicitations.clone())
    }
''', 1),
    # wrapper delegation
    ('''        fn cancel(&self, session_id: &acp::SessionId, cx: &mut App) {
            self.inner.cancel(session_id, cx)
        }
''', '''        fn cancel(&self, session_id: &acp::SessionId, cx: &mut App) {
            self.inner.cancel(session_id, cx)
        }

        fn can_stop_subagent(
            &self,
            session_id: &acp::SessionId,
            tool_call_id: &acp::ToolCallId,
        ) -> bool {
            self.inner.can_stop_subagent(session_id, tool_call_id)
        }

        fn stop_subagent(
            &self,
            session_id: &acp::SessionId,
            tool_call_id: &acp::ToolCallId,
            cx: &mut App,
        ) {
            self.inner.stop_subagent(session_id, tool_call_id, cx)
        }
''', 1),
    # root session over the shared map
    ('''fn root_session_id(ctx: &ClientContext, session_id: &acp::SessionId) -> acp::SessionId {
    let subagents = ctx.subagents.borrow();
    let mut session_id = session_id;''', '''fn root_session_id(ctx: &ClientContext, session_id: &acp::SessionId) -> acp::SessionId {
    root_subagent_session(&ctx.subagents.borrow(), session_id)
}

fn root_subagent_session(
    subagents: &HashMap<acp::SessionId, SubagentSession>,
    session_id: &acp::SessionId,
) -> acp::SessionId {
    let mut session_id = session_id;''', 1),
    # record cancellable on announce
    ('''                    .or_insert_with(|| SubagentSession {
                        parent_session_id: session_id,
                        tool_call_id: None,
                        running: false,
                    });
                if let Some(running) = running {''', '''                    .or_insert_with(|| SubagentSession {
                        parent_session_id: session_id,
                        tool_call_id: None,
                        running: false,
                        cancellable: false,
                    });
                // Only the announcement lists the capabilities.
                if let Some(capabilities) = update.get("capabilities") {
                    subagent.cancellable = capabilities.get("cancel").is_some();
                }
                if let Some(running) = running {''', 1),
])

# helper on AcpConnection, next to the other inherent methods
edit('crates/agent_servers/src/acp.rs', [(
    '''impl AcpConnection {
    pub fn subscribe_debug_messages(''',
    '''impl AcpConnection {
    /// The session of the running subagent that does the work of
    /// `tool_call_id` in `session_id`, when the agent stops it on its own.
    fn cancellable_subagent(
        &self,
        session_id: &acp::SessionId,
        tool_call_id: &acp::ToolCallId,
    ) -> Option<acp::SessionId> {
        let subagents = self.subagents.borrow();
        subagents
            .iter()
            .find(|(subagent_id, subagent)| {
                subagent.running
                    && subagent.cancellable
                    && subagent.tool_call_id.as_ref() == Some(tool_call_id)
                    && root_subagent_session(&subagents, subagent_id) == *session_id
            })
            .map(|(subagent_id, _)| subagent_id.clone())
    }

    pub fn subscribe_debug_messages(''', 1)])
