use super::*;

/// Shared drift vector: the commands where the conservative consent
/// matcher + core's aggressive snapshot matcher MUST agree (clear-cut
/// discards). Core's agent::git_discard holds an identical vector + an
/// expected table; keep both in sync. Asymmetric cases (single-path
/// checkout, bare stash) live in their own crate-specific tests.
const DRIFT_VECTOR: &[&str] = &[
    "git checkout .",
    "git checkout -- file.rs",
    "git checkout -f main",
    "git checkout --force",
    "git restore file.rs",
    "git restore -w file.rs",
    "git clean -fd",
    "git clean --force",
    "git stash drop",
    "git stash clear",
    "git stash pop",
    "git branch -D feature",
    "git push --force origin main",
    "git push --force-with-lease",
    "git checkout main && git status",
    "git status",
    "git log --oneline",
    "git stash list",
    "git stash apply",
];

#[test]
fn test_drift_vector_consent_matches() {
    // Matches core's drift_vector_conservative_matches — if either drifts,
    // the other crate's test fails too. Expected: true for the 14 clear
    // discards, false for the 5 non-discards (switch + status ops). The safe
    // branch delete is asserted on its own below: the two gates deliberately
    // disagree on it.
    let expected = [
        true, true, true, true, true, true, true, true, true, true, true, true, true, true, false,
        false, false, false, false,
    ];
    assert_eq!(DRIFT_VECTOR.len(), expected.len());
    for (cmd, exp) in DRIFT_VECTOR.iter().zip(expected.iter()) {
        // The scan reads the command as written: the case of a flag is what
        // separates a guarded branch delete from a forced one.
        let (sub, args) = cmd
            .split(['&', ';', '|', '\n'])
            .find_map(|seg| git_sub_and_args(&seg.split_whitespace().collect::<Vec<_>>()))
            .unwrap_or_else(|| panic!("no git sub in {cmd}"));
        assert_eq!(
            git_discard_consent_word(sub, &args).is_some(),
            *exp,
            "consent drift on {cmd}"
        );
    }
}

/// The bypasses that used to slip past the consent gate (review cases A,
/// C, + the --force-with-lease= nit). Each now correctly consents.
#[test]
fn test_consent_bypasses_closed() {
    for cmd in [
        "git -C /path checkout .",
        "git -c user.name=x checkout .",
        "git --work-tree /x checkout .",
        "git --git-dir=/x checkout .",
        "git restore file.rs",
        "git clean -fd",
        "git stash drop",
        "git branch -D feature",
        "git push --force origin main",
        "git push --force-with-lease=origin/main",
        "(git checkout .)",
        "{ git checkout . ; }",
        "FOO=bar git checkout .",
        "bash -c \"git checkout .\"",
    ] {
        assert!(
            should_ask_before_git("bash", cmd).is_some(),
            "{cmd} should consent (bypass closed)"
        );
    }
    // Non-discards + single-path checkout (asymmetric: consent does NOT
    // ask; the snapshot trigger in core does snapshot it).
    for cmd in [
        "git checkout main",
        "git checkout src/main.rs",
        "git status",
        "git log",
        "git stash list",
        "git stash apply",
        "git branch feature",
        "git restore --staged file.rs",
        "ls -la",
    ] {
        assert!(
            should_ask_before_git("bash", cmd).is_none(),
            "{cmd} should NOT consent"
        );
    }
}

/// The two branch-delete spellings are not the same act. Git refuses to delete
/// a branch holding unmerged work under the lowercase flag and deletes it
/// anyway under the uppercase one, so only the forced spelling discards
/// anything. Asking about the guarded spelling spends the user's attention on
/// an operation that cannot lose work.
#[test]
fn test_branch_delete_spellings_differ() {
    let ask = |cmd: &str| {
        let (sub, args) = cmd
            .split(['&', ';', '|', '\n'])
            .find_map(|seg| git_sub_and_args(&seg.split_whitespace().collect::<Vec<_>>()))
            .unwrap_or_else(|| panic!("no git sub in {cmd}"));
        git_discard_consent_word(sub, &args).is_some()
    };
    assert!(
        !ask("git branch -d feature"),
        "the guarded delete asks nothing"
    );
    assert!(
        !ask("git branch --delete feature"),
        "the long guarded spelling asks nothing either"
    );
    assert!(ask("git branch -D feature"), "the forced delete asks");
    assert!(
        ask("git branch -d -f feature"),
        "force beside the guarded spelling makes it forced"
    );
    assert!(
        ask("git branch --delete --force feature"),
        "the long form of the same pair asks too"
    );
    // Reading a branch list discards nothing.
    assert!(!ask("git branch --list"));
}
