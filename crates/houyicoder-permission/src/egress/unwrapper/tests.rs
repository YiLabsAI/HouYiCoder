//! Tests for the shell construct unwrapper.

use super::super::split_clauses;
use super::*;

/// A loop header and case labels peel away, leaving the programs the
/// classifiers must judge. The clause walk receives one clause at a time,
/// the way the collector splits on semicolons first.
#[test]
fn test_unwrap_loop_and_case() {
    // The label branch after a case header, as its own clause.
    let peeled = unwrap_clause(r#"*) break"#).expect("the branch peels");
    assert_eq!(peeled, "", "the default branch label carries no program");

    // The loop body clause, as the clause splitter hands it over.
    let peeled = unwrap_clause("do sleep 15").expect("the loop body peels");
    assert!(peeled.contains("sleep 15"), "the body survives: {peeled}");

    // The case head with its first label.
    let peeled =
        unwrap_clause(r#"case "$s" in *in_progress*|*queued*|"")"#).expect("the case head peels");
    assert_eq!(peeled, "", "the head and label carry no program");
}

/// An assignment with a substitution peels its shell: the marker stands
/// where the body was, and the body is the caller's recursion to judge.
#[test]
fn test_unwrap_assignment() {
    let clause = r#"s=$(curl -s "https://api.github.com/jobs" | python3 -c "print(1)")"#;
    let peeled = unwrap_clause(clause).expect("the assignment peels");
    assert_eq!(
        peeled, "",
        "the shell and the marker carry nothing to judge"
    );
    let bodies = substitution_bodies(clause).expect("the body extracts");
    assert!(
        bodies[0].contains("curl") && bodies[0].contains("python3"),
        "the captured pipeline is the recursion's input: {bodies:?}"
    );
}

/// The bodies the substitution extractor finds are exactly the paren
/// contents, quote-aware: a substitution inside a double-quoted string is
/// still found, and a backquote refuses the whole clause.
#[test]
fn test_substitution_bodies() {
    let clause = "for i in $(seq 1 10); do echo \"$(date)\"; done";
    let bodies = substitution_bodies(clause).expect("tracked substitutions");
    assert_eq!(bodies.len(), 2, "the generator and the capture: {bodies:?}");
    assert_eq!(bodies[0], "seq 1 10");
    assert_eq!(bodies[1], "date");

    let refused = "echo `date`";
    assert!(substitution_bodies(refused).is_none(), "backquotes refuse");
}

/// A word with a quoted closing paren is not a case label: the quotes make
/// it data, and the classifier sees the program that carries it.
#[test]
fn test_quoted_paren_word_stays() {
    let clause = r#"echo "text)" and more"#;
    let peeled = unwrap_clause(clause).expect("the clause peels");
    assert!(peeled.contains("echo"), "the program survives: {peeled}");
}

/// A plain variable assignment before a program peels, leaving the program.
#[test]
fn test_plain_assignment_peels() {
    let clause = "FOO=bar curl -s https://example.com";
    let peeled = unwrap_clause(clause).expect("the assignment peels");
    assert!(
        peeled.starts_with("curl"),
        "the program after the assignment survives: {peeled}"
    );
}

/// An assignment to a name that redirects a request tool's behavior refuses
/// the clause: peeling it would judge a destination the assignment changed.
#[test]
fn test_dangerous_env_refuses() {
    for clause in [
        "CURL_HOME=/tmp/config curl -sS https://docs.rs/",
        "HTTPS_PROXY=http://evil.example curl -s https://api.github.com",
    ] {
        let peeled = unwrap_clause(clause);
        assert!(peeled.is_none(), "must refuse: {clause}");
    }
}

/// Temporary trace of the clause path for the polling shape.
#[test]
fn test_trace_polling() {
    let command = r#"cd /repo && for i in $(seq 1 40); do sleep 15; s=$(curl -s "https://api.github.com/repos/org/repo/actions/runs/1/jobs" | python3 -c "import json,sys; d=json.load(sys.stdin); print('|'.join(f\"{j['name']}:{j['status']}\" for j in d.get('jobs',[])))" 2>/dev/null); echo "$i $s"; case "$s" in *in_progress*|*queued*|"") ;; *) break;; esac; done"#;
    for clause in split_clauses(command) {
        let peeled = unwrap_clause(&clause);
        let bodies = substitution_bodies(&clause);
        println!("TRACE clause {clause:?}");
        println!("       peeled {peeled:?}");
        println!("       bodies {bodies:?}");
    }
}
