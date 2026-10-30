use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::{FrontendRequest, SessionId};

use super::Outbound;
use super::UnknownReportBudget;
use super::outbound::frame_for;
use crate::agent_message::ClientCommand;

#[test]
fn test_frame_direct_shell_run() {
    let outbound = frame_for(ClientCommand::DirectShellRun {
        req_id: RequestId(7),
        session_id: SessionId("s1".into()),
        command: "cargo test".into(),
    });
    match outbound {
        Outbound::Request {
            payload:
                FrontendRequest::DirectShellRun {
                    session_id,
                    command,
                },
            ..
        } => {
            assert_eq!(session_id.0, "s1");
            assert_eq!(command, "cargo test");
        }
        _ => panic!("a DirectShellRun command frames a DirectShellRun request"),
    }
}

#[test]
fn test_unknown_report_budget_spends() {
    // The bound's false branch has no other observable: the driver's
    // only report for a dropped notification is a diagnostic line.
    let mut budget = UnknownReportBudget::new();
    for _ in 0..UnknownReportBudget::LIMIT {
        assert!(budget.grant());
    }
    assert!(!budget.grant());
}

/// The recap command travels as one request carrying both fields. Sending
/// them as two requests would let a failure land between them and leave the
/// file with a switch that names a window it never received.
#[test]
fn test_recap_command_carries_both() {
    let out = frame_for(ClientCommand::RecapConfigSet {
        req_id: RequestId(7),
        enabled: true,
        idle_secs: 600,
    });
    match out {
        Outbound::Request { payload, .. } => match payload {
            FrontendRequest::RecapConfigSet { enabled, idle_secs } => {
                assert!(enabled, "the switch rides the request");
                assert_eq!(idle_secs, 600, "and so does the window");
            }
            other => panic!("expected RecapConfigSet, got {other:?}"),
        },
        _ => panic!("expected a request for the recap command"),
    }
}
