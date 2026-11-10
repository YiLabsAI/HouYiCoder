//! Tests for the shell construct unwrapper.

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
    let refused = "echo \"`date`\"";
    assert!(
        substitution_bodies(refused).is_none(),
        "a backquote runs inside double quotes too"
    );
}

/// Arithmetic expansion runs no command, so it contributes no body; a command
/// substitution nested inside it still runs and is reported.
#[test]
fn test_arithmetic_carries_no_command() {
    let bodies = substitution_bodies("echo $((1 << 2))").expect("arithmetic is tracked");
    assert!(bodies.is_empty(), "a number runs nothing: {bodies:?}");

    let bodies = substitution_bodies("echo $(( $(date) + 1 ))").expect("the pairs balance");
    assert_eq!(bodies, vec!["date".to_string()]);
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

/// Arithmetic expansion computes a number, so it stands in as a value: a
/// quoted word keeps its shape and an unquoted one stays one word.
#[test]
fn test_arithmetic_expands_to_value() {
    let peeled = unwrap_clause(r#"echo "after $((i))m""#).expect("the word stays whole");
    assert!(peeled.contains("after 0m"), "the value stands in: {peeled}");

    let peeled = unwrap_clause("sleep $((60 * 60))").expect("the argument stays one word");
    assert!(
        peeled.split_whitespace().count() == 2,
        "the expansion is one value, not three words: {peeled}"
    );
}

/// A command substitution inside double quotes is still a substitution: the
/// marker becomes its own word so the clause refuses rather than treating a
/// rewritten argument as text the shell passes as written.
#[test]
fn test_quoted_substitution_refuses() {
    assert!(unwrap_clause(r#"echo "prefix$(date)suffix""#).is_none());
    // At a command position the marker peels, and the body is the caller's
    // recursion to judge.
    let peeled = unwrap_clause(r#"h="$(date)""#).expect("a capture peels");
    assert_eq!(peeled, "", "the capture carries nothing to judge");
}
