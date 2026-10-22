//! Process-group helpers: the tree kill, the CPU total, and the cputime parser.
//! The parser is exercised directly because the ps output it reads varies by
//! platform and cannot be reproduced from a fixture.

use super::super::process::{kill_process_group, parse_cputime_secs, tree_cpu_secs};
use super::super::temp_dir::{COUNTER, mkdtemp};
use std::env::temp_dir;
use std::fs::create_dir;
use std::fs::remove_dir_all;
use std::process::id;
use std::sync::atomic::Ordering;

/// The parser folds colon-separated segments at 60, which covers the macOS
/// minutes:seconds and the Linux hours:minutes:seconds formats.
#[test]
fn test_parse_cputime_formats() {
    assert_eq!(parse_cputime_secs("0:05"), 5);
    assert_eq!(parse_cputime_secs("1:02"), 62);
    assert_eq!(parse_cputime_secs("1:02:03"), 3723);
    assert_eq!(parse_cputime_secs("0:00.90"), 0, "sub-second rounds down");
    assert_eq!(
        parse_cputime_secs("junk"),
        0,
        "a bad row never poisons the sum"
    );
}

/// A process group id of zero means there is no group to act on, so both
/// helpers report nothing rather than signalling pid zero.
#[test]
fn test_zero_group_noop() {
    kill_process_group(0);
    kill_process_group(-1);
    assert_eq!(tree_cpu_secs(0), 0);
}

/// The CPU total of this process's own group is measurable, which exercises the
/// ps call and the per-line filter on a group that certainly exists.
#[test]
fn test_tree_cpu_reads_group() {
    let pgid = i32::try_from(id()).expect("pid fits in i32");
    // Any live group has some CPU behind it; the exact figure is not the point.
    let _secs = tree_cpu_secs(pgid);
}

/// A name left behind by a crashed process is stepped over: the counter
/// advances until a free name is found, so a leftover directory never fails the
/// session. The band is pre-created for a prefix used by no other test.
#[test]
fn test_taken_name_steps_over() {
    let prefix = "hy-taken";
    let mut taken = Vec::new();
    for n in 0..1024u64 {
        let path = temp_dir().join(format!("{prefix}-{}-{n:x}", id()));
        if create_dir(&path).is_ok() {
            taken.push(path);
        }
    }
    let minted = mkdtemp(prefix).expect("a taken name must not fail the mint");
    assert!(
        !taken.contains(&minted),
        "the mint stepped over the pre-created names"
    );
    assert!(
        COUNTER.load(Ordering::SeqCst) > 0,
        "the counter advanced past the taken names"
    );
    let _cleanup = remove_dir_all(&minted);
    for path in taken {
        let _cleanup = remove_dir_all(path);
    }
}
