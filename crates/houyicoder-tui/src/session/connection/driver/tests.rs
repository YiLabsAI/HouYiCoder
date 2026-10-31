use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::FrontendRequest;

use super::Outbound;
use super::UnknownReportBudget;
use super::outbound::frame_for;
use crate::agent_message::ClientCommand;

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
