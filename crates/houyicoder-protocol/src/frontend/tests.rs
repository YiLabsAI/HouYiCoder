//! Tests for this module.

use super::*;

/// The COMMANDS table must list every variant exactly once, in ALL order,
/// so name()/help()/parse() never panic and ALL stays in sync.
#[test]
fn test_commands_table_covers() {
    assert_eq!(COMMANDS.len(), SlashCommand::ALL.len());
    let from_table: Vec<SlashCommand> = COMMANDS.iter().map(|d| d.variant).collect();
    assert_eq!(from_table, SlashCommand::ALL.to_vec());
    for v in SlashCommand::ALL {
        assert!(!v.name().is_empty());
        assert!(!v.help().is_empty());
    }
}

#[test]
fn test_parse_round_trips_names() {
    for v in SlashCommand::ALL {
        assert_eq!(SlashCommand::parse(v.name()), Some(v));
    }
}

#[test]
fn test_parse_rejects_unknown() {
    assert_eq!(SlashCommand::parse("/nope"), None);
    assert_eq!(SlashCommand::parse("login"), None);
}

/// /debug is registered (parseable + has a name/help) so it is discoverable
/// in the palette like /status and /context, not a hidden string command.
#[test]
fn test_debug_is_registered() {
    assert_eq!(SlashCommand::parse("/debug"), Some(SlashCommand::Debug));
    assert!(SlashCommand::Debug.name().contains("debug"));
    assert!(!SlashCommand::Debug.help().is_empty());
}

/// Arg-capable commands advertise a non-empty hint (so the palette can show it
/// after a space); arg-incapable ones do not. Every takes_arg command must
/// have a hint, but a hint does not imply takes_arg (/resume /export /debug
/// take an optional arg but run argless on select).
#[test]
fn test_arg_hint_arg_capable() {
    for c in SlashCommand::ALL {
        if c.takes_arg() {
            assert!(
                !c.arg_hint().is_empty(),
                "{} takes an arg but has no hint",
                c.name()
            );
        }
    }
    // The optional-arg commands advertise a hint even though they are not
    // takes_arg (their argless form runs on select; the hint guides the
    // spaced form).
    for name in ["resume", "export", "debug"] {
        let cmd = SlashCommand::parse(&format!("/{name}"));
        let cmd = cmd.unwrap_or_else(|| panic!("{name} parsed"));
        assert!(
            !cmd.arg_hint().is_empty(),
            "{name} should advertise an arg hint"
        );
    }
}

#[test]
fn test_all_variants_runs_runtime() {
    // The const fn that builds ALL is evaluated at compile time for the
    // const; call it at runtime so its body is covered too.
    let v = all_variants();
    assert_eq!(v, SlashCommand::ALL);
    assert_eq!(v.len(), SlashCommand::ALL.len());
}
