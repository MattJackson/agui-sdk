//! The security model's application duties (`index.mdx` "Security"), as
//! the client helps with them: consent before execution, honest approval
//! records, validation of what is acted on, confidentiality of reasoning
//! material, and an audit trail.

#![cfg(feature = "client")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_client;

use std::sync::Arc;

use agui_sdk::client::{AgentClient, DecidedBy, RunRequest};
use agui_sdk::diagnostics::CollectingSink;
use agui_sdk::ids::ToolCallId;
use agui_sdk::tool::Tool;
use agui_sdk::Message;
use serde_json::json;
use support_client::*;

fn delete_tool() -> Tool {
    Tool::new("delete_file", "Deletes a file").with_parameters(json!({
        "type": "object",
        "required": ["path"],
        "properties": {"path": {"type": "string", "pattern": "^/tmp/"}}
    }))
}

fn thread_with_call(args: &str) -> agui_sdk::client::Thread<MockTransport> {
    let mut frames = vec![run_started("t1", "r1")];
    frames.extend(tool_call("tc1", "delete_file", args, "m1"));
    frames.push(run_finished("t1", "r1"));
    let client = AgentClient::with_transport(mock(frames)).build().unwrap();
    let mut thread = client.thread_with_id("t1").with_tools([delete_tool()]);
    block_on(thread.run_and_absorb(RunRequest::new().run_id("r1"))).unwrap();
    thread
}

/// covers: IDX-05
#[test]
fn executing_a_call_requires_an_approval_first() {
    let mut thread = thread_with_call(r#"{"path":"/tmp/x"}"#);
    // The only way to stage a result is with an ApprovedToolCall, obtained
    // from `approve` for a call that is pending.
    let approval = thread
        .approve(&ToolCallId::from("tc1"), DecidedBy::User { user_id: None })
        .unwrap();
    thread.complete(approval, "deleted").unwrap();
    assert!(thread
        .approve(&ToolCallId::from("tc1"), DecidedBy::User { user_id: None })
        .is_err());
}

/// covers: IDX-06
#[test]
fn a_policy_decision_is_never_recorded_as_the_users() {
    let thread = thread_with_call(r#"{"path":"/tmp/x"}"#);
    let auto = thread
        .approve(
            &ToolCallId::from("tc1"),
            DecidedBy::Policy {
                name: "allow-tmp".into(),
            },
        )
        .unwrap();
    assert!(matches!(auto.decided_by(), DecidedBy::Policy { name } if name == "allow-tmp"));
    assert!(!matches!(auto.decided_by(), DecidedBy::User { .. }));
}

/// covers: IDX-07
#[test]
fn what_is_acted_on_is_validated_and_the_unchecked_part_is_named() {
    let thread = thread_with_call(r#"{"path":"/etc/passwd"}"#);
    let call = thread
        .approve(&ToolCallId::from("tc1"), DecidedBy::User { user_id: None })
        .unwrap();
    // `pattern` is outside the subset: the pass is explicitly partial, so the
    // application knows it must check the path itself.
    let check = call.check_args_subset().unwrap();
    assert_eq!(check.unevaluated, ["/properties/path/pattern"]);
    let thread = thread_with_call(r#"{"file":"/tmp/x"}"#);
    let call = thread
        .approve(&ToolCallId::from("tc1"), DecidedBy::User { user_id: None })
        .unwrap();
    assert!(call.check_args_subset().is_err());
}

/// covers: IDX-10
#[test]
fn encrypted_reasoning_goes_back_only_to_the_agent_that_produced_it() {
    let frames = run_of(
        "t1",
        "r1",
        vec![
            json!({"type": "REASONING_START", "messageId": "rs1"}),
            json!({"type": "REASONING_MESSAGE_START", "messageId": "rm1", "role": "reasoning"}),
            json!({"type": "REASONING_MESSAGE_CONTENT", "messageId": "rm1", "delta": "thinking"}),
            json!({"type": "REASONING_MESSAGE_END", "messageId": "rm1"}),
            json!({"type": "REASONING_ENCRYPTED_VALUE", "subtype": "message", "entityId": "rm1", "encryptedValue": "opaque-blob"}),
            json!({"type": "REASONING_END", "messageId": "rs1"}),
        ],
    );
    let transport = mock(frames);
    let client = AgentClient::with_transport(transport.clone())
        .build()
        .unwrap();
    let mut thread = client.thread_with_id("t1");
    let report = block_on(thread.run_and_absorb(RunRequest::new().run_id("r1"))).unwrap();
    // Kept, opaque, in the history the client returns...
    let kept = report.messages.iter().find_map(|m| match m {
        Message::Reasoning(r) => r.encrypted_value.clone(),
        _ => None,
    });
    assert_eq!(kept.as_deref(), Some("opaque-blob"));
    // ...never in diagnostics (no payload values there)...
    assert!(report
        .diagnostics
        .iter()
        .all(|d| !d.message.contains("opaque-blob")));
    // ...and sent back on the thread's next run to the same agent.
    let _ = block_on(thread.run_and_absorb(RunRequest::new().run_id("r2")));
    let sent = transport.last_request().unwrap();
    assert!(sent.to_string().contains("opaque-blob"));
}

/// covers(client): IDX-11
#[test]
fn diagnostics_carry_the_run_for_an_audit_log() {
    let sink = Arc::new(CollectingSink::new());
    let frames = run_of("t1", "r1", vec![json!({"type": "VENDOR_EVENT"})]);
    let client = AgentClient::with_transport(mock(frames))
        .diagnostic_sink(Arc::clone(&sink))
        .build()
        .unwrap();
    block_on(std::future::IntoFuture::into_future(
        client.run(RunRequest::new().thread_id("t1").run_id("r1")),
    ))
    .unwrap();
    let logged = sink.take();
    assert_eq!(logged.len(), 1);
    assert_eq!(logged[0].run_id.as_ref().unwrap().as_str(), "r1");
    // And approvals name who decided, for the same log.
    let thread = thread_with_call(r#"{"path":"/tmp/x"}"#);
    let call = thread
        .approve(
            &ToolCallId::from("tc1"),
            DecidedBy::Policy { name: "p".into() },
        )
        .unwrap();
    assert_eq!(
        format!("{:?}", call.decided_by()),
        r#"Policy { name: "p" }"#
    );
}
