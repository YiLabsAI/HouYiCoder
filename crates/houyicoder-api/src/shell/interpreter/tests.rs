//! Tests for the read-only rule and the interpreter code removal.

use super::{is_read_only_program, without_read_only_code};

/// A program that reads what it is given and prints part of it is read-only:
/// it opens files for reading, imports data modules, and names none of the
/// primitives that would write, start a program, or reach the network.
#[test]
fn test_data_program_reads_only() {
    for code in [
        "import json",
        "open('x')",
        "open('x',encoding='utf-8')",
        "import json; print(json.loads('{}'))",
        "for i,line in enumerate(open('x',encoding='utf-8',errors='ignore'),1): print(i)",
        "import os; print(os.environ['HOME'])",
        "d={}; print(d.get('a',{}))",
        "hits=[]; hits.append(('a','b','c'))",
    ] {
        assert!(is_read_only_program(code), "should read: {code}");
    }
    let body = "import json\npath='log.jsonl'\nhits=[]\nfor i,line in enumerate(open(path,encoding='utf-8',errors='ignore'),1):\n    if 'git push origin dev' in line or 'git push origin main' in line:\n        try: e=json.loads(line)\n        except: continue\n        ev=e.get('event',{})\n        t=ev.get('type') or ev.get('event') or list(ev)[:1]\n        hits.append((i,t,line[:260]))\nprint('push lines:',len(hits))\nfor h in hits[:12]: print(h[0],h[1],h[2][:200].replace('\\n',' '))";
    assert!(
        is_read_only_program(body),
        "a log parser reads only: {body}"
    );
    // An update mode opens for writing, so it is not a read however harmless
    // the rest of the call looks.
    assert!(
        !is_read_only_program("open('x','r+')"),
        "an update mode writes"
    );
}

/// The path a read-only program reaches may itself be the exposure: a startup
/// file, a private key, or the settings store carries the account and the
/// instructions every later turn reads. The path sits inside a string
/// literal, so the read-only scan is the only rule that sees it.
#[test]
fn test_credential_path_refused() {
    for code in [
        "print(open('/Users/von/.bashrc').read())",
        "print(open('/Users/von/.ssh/id_rsa').read())",
        "print(open('/Users/von/.aws/credentials').read())",
        "print(open('.houyicoder/settings.json').read())",
        "print(open('AGENTS.md').read())",
    ] {
        assert!(!is_read_only_program(code), "not a plain read: {code}");
    }
    assert!(
        is_read_only_program("print(open('report.json').read())"),
        "an ordinary file still reads"
    );
}

/// The corpus is the one rule table for every caller, so a line added here is
/// judged once and the verdict is the one each caller must reach.
#[test]
fn test_read_only_corpus_matches() {
    let corpus = include_str!("../../../tests/fixtures/read_only_code.txt");
    let mut entries = 0;
    for line in corpus.lines() {
        let Some((verdict, code)) = line.split_once('|') else {
            continue;
        };
        entries += 1;
        assert_eq!(
            is_read_only_program(code),
            verdict == "read",
            "corpus says {verdict}: {code}"
        );
    }
    assert!(entries > 20, "the corpus must cover the rules: {entries}");
}

/// The separators inside a one-liner are why the body is removed rather than
/// skipped: a caller splits the command on them, and a body left in place
/// would be cut into segments the caller reads as commands.
#[test]
fn test_inline_code_removed() {
    let command = "python3 -c \"import json; print(json.loads('{}'))\" && git status";
    let out = without_read_only_code(command).expect("a reader is verified");
    assert!(!out.contains("json.loads"), "the body is gone: {out}");
    assert!(out.contains("git status"), "the rest is kept: {out}");
    assert!(
        without_read_only_code("python3 -c 'import shutil; shutil.rmtree(\"/x\")'").is_none(),
        "a writer is not verified"
    );
}

/// A quoted heredoc is the shape a parser arrives in on the session's own
/// commands, so the removal has to read the body and the closing delimiter.
#[test]
fn test_heredoc_body_removed() {
    let command = "curl -sL -o /tmp/x.html https://untrusted.example.com && python3 - <<'EOF'\nimport json,sys\nd=json.load(sys.stdin)\nprint(len(d))\nEOF";
    let out = without_read_only_code(command).expect("a parser is verified");
    assert!(!out.contains("json.load"), "the body is gone: {out}");
    assert!(out.contains("curl -sL"), "the fetch is kept: {out}");
    assert!(!out.contains("EOF"), "the delimiter line is gone: {out}");
}

/// The shell expands an unquoted body before the program reads it, so the
/// text the program runs is not the text the rule judged.
#[test]
fn test_unquoted_heredoc_refused() {
    let command = "python3 <<EOF\nimport json\nprint($HOME)\nEOF";
    assert!(without_read_only_code(command).is_none());
    let several = "python3 - <<'A' <<'B'\nimport json\nA\nimport json\nB";
    assert!(
        without_read_only_code(several).is_none(),
        "two bodies cannot both be the input"
    );
}

/// A body whose closing delimiter never appears never ends, so the command
/// text after it is unknown rather than read-only.
#[test]
fn test_unterminated_heredoc_refused() {
    let command = "python3 - <<'EOF'\nimport json\nprint(1)";
    assert!(without_read_only_code(command).is_none());
}

/// A verified parser may not open a protected file, because the body is the
/// input rather than the command line the other checks read.
#[test]
fn test_credential_heredoc_refused() {
    let command = "python3 - <<'EOF'\nprint(open('/Users/von/.ssh/id_rsa').read())\nEOF";
    assert!(without_read_only_code(command).is_none());
}

/// A word that is not an interpreter is left where it is, so the caller reads
/// the command as written.
#[test]
fn test_other_words_kept() {
    for command in ["grep python3 notes.txt", "git status"] {
        assert_eq!(without_read_only_code(command).as_deref(), Some(command));
    }
}
