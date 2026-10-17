use crate::common::Echo;
use jensen_harness_core::agent::Session;
use jensen_harness_core::providers::{Role, StopReason};

#[tokio::test]
async fn prompt_appends_both_turns_in_order() {
    let mut session = Session::new();
    let stop_reason = session.prompt(&Echo, "hello").await.unwrap();
    assert_eq!(stop_reason, StopReason::EndTurn);
    let [first, second] = session.turns() else {
        panic!("expected exactly two turns, got {}", session.turns().len());
    };
    assert_eq!(first.role, Role::User);
    assert_eq!(first.text, "hello");
    assert_eq!(second.role, Role::Assistant);
    assert_eq!(second.text, "echo: hello");
}
