use super::*;

#[test]
fn test_split_first_line_single() {
    let (first, hidden) = split_first_line("hello");
    assert_eq!(first, "hello");
    assert_eq!(hidden, 0);
}

#[test]
fn test_split_first_line_multi() {
    let (first, hidden) = split_first_line("line one\nline two\nline three");
    assert_eq!(first, "line one");
    assert_eq!(hidden, 2);
}
