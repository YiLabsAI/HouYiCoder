//! The read-only classifier, over the commands a model reads with and the ones
//! that need approval.

use super::read_only;

/// A pure read-only cmdlet with a literal path is allowed without approval.
#[test]
fn test_read_only_cmdlet_passes() {
    assert!(
        read_only("Get-ChildItem -Path src"),
        "a read of a directory is read-only"
    );
    assert!(
        read_only("Get-Content -LiteralPath README.md"),
        "a read of a file is read-only"
    );
    assert!(
        read_only("Test-Path src/main.rs"),
        "a path test is read-only"
    );
    assert!(
        read_only("Get-Location"),
        "the current location is read-only"
    );
    assert!(
        read_only("Select-String -Path log.txt -Pattern error"),
        "a search is read-only"
    );
}

/// A pipeline, a sequence, or a redirect makes the command dynamic.
#[test]
fn test_dynamic_construct_rejected() {
    assert!(
        !read_only("Get-ChildItem | Select-Object Name"),
        "a pipeline is not read-only"
    );
    assert!(
        !read_only("Get-Content file; Remove-Item file"),
        "a sequence is not read-only"
    );
    assert!(
        !read_only("Get-Content file > out.txt"),
        "a redirect is not read-only"
    );
    assert!(
        !read_only("Get-Content file & Do-Thing"),
        "a background is not read-only"
    );
}

/// A substitution, a type reference, or a block makes the command dynamic.
#[test]
fn test_substitution_rejected() {
    assert!(
        !read_only("Get-Content $file"),
        "a variable is not read-only"
    );
    assert!(
        !read_only("Get-Content $(Get-Path)"),
        "a subexpression is not read-only"
    );
    assert!(
        !read_only("Get-Content file`n"),
        "a backtick is not read-only"
    );
    assert!(
        !read_only("[System.IO.File]::ReadAllText('x')"),
        "a type reference is not read-only"
    );
    assert!(
        !read_only("Get-Content file {param}"),
        "a block is not read-only"
    );
}

/// A parameter bound to an expression by a colon is dynamic.
#[test]
fn test_dynamic_parameter_rejected() {
    assert!(
        !read_only("Get-Content -Path:$env:SECRET"),
        "a parameter bound to an expression is not read-only"
    );
    assert!(
        !read_only("Get-ChildItem -Filter:(Get-Filter)"),
        "a parameter bound to a subexpression is not read-only"
    );
}

/// A cmdlet that writes or runs is never read-only, whatever flags it carries.
#[test]
fn test_write_cmdlet_rejected() {
    assert!(!read_only("Remove-Item file"), "a delete is not read-only");
    assert!(
        !read_only("Set-Content file text"),
        "a write is not read-only"
    );
    assert!(
        !read_only("Invoke-Expression Get-Thing"),
        "an invoke is not read-only"
    );
}

/// An empty command is not read-only.
#[test]
fn test_empty_rejected() {
    assert!(!read_only(""), "an empty command is not read-only");
    assert!(!read_only("   "), "a blank command is not read-only");
}

/// A safe flag on a read-only cmdlet keeps it read-only.
#[test]
fn test_safe_flag_passes() {
    assert!(
        read_only("Get-ChildItem -Recurse -Filter *.rs"),
        "safe flags stay read-only"
    );
    assert!(
        read_only("Get-Content -Path log -TotalCount 10 -ErrorAction SilentlyContinue"),
        "a count and an error action are safe"
    );
}
