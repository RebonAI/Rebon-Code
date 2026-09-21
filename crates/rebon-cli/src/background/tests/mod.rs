mod mirror_lease_and_options;
mod worker_finalization_with_a_session;

#[test]
fn only_verification_agent_sessions_are_hidden_from_chats() {
    assert!(super::should_hide_agent_session_from_chats(Some(
        "verification"
    )));
    assert!(super::should_hide_agent_session_from_chats(Some(
        "Verification"
    )));
    assert!(!super::should_hide_agent_session_from_chats(Some(
        "general-purpose"
    )));
    assert!(!super::should_hide_agent_session_from_chats(None));
}
