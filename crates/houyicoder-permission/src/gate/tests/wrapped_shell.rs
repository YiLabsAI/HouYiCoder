//! Commands a recorded session ran, replayed through the gate.
//!
//! The session wrapped a push in a timeout and redirected its output, handed
//! read-only parsers quoted heredoc bodies, and looped over local git output.
//! Each of those shapes has a recorded command here, so a regression in the
//! shell scan shows up as a decision that no longer matches what the operator
//! saw: the pushes were silent, and the local reads were not pushes.

use super::*;
use crate::decision::{Decision, Outcome};
use houyicoder_api::sandbox::{Containment, Coverage, SideEffect};
use std::env::temp_dir;
use std::fs::{create_dir_all, remove_dir_all};
use std::path::PathBuf;
use std::process::id;
use std::sync::Arc;

struct ScratchFence {
    scratch: PathBuf,
}

impl Containment for ScratchFence {
    fn coverage(&self) -> Coverage {
        Coverage::Fenced {
            writable_roots: vec![self.scratch.clone()],
        }
    }
    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
}

/// The pushes the session ran against the integration branch. The first five
/// were allowed without a prompt; the last two sit behind a merge and the gate
/// asked for them, so the whole set must ask for the same reason.
const PUSHES: &[&str] = &[
    "cd /Users/von/workspace/hicoder && timeout 560 git push origin dev > $TMPDIR/pushdev.log 2>&1; echo \"PUSH_EXIT=$?\"; tail -40 $TMPDIR/pushdev.log",
    "cd /Users/von/workspace/hicoder && timeout 590 git push origin dev > $TMPDIR/pushdev2.log 2>&1; echo \"PUSH_EXIT=$?\"; tail -8 $TMPDIR/pushdev2.log",
    "cd /Users/von/workspace/hicoder && timeout 590 git push origin dev > $TMPDIR/pushdev3.log 2>&1; echo \"PUSH_EXIT=$?\"; tail -10 $TMPDIR/pushdev3.log; git ls-remote origin refs/heads/dev",
    "cd /Users/von/workspace/hicoder && timeout 900 git push origin dev > $TMPDIR/pushdev4.log 2>&1; echo \"PUSH_EXIT=$?\"; tail -8 $TMPDIR/pushdev4.log; git ls-remote origin refs/heads/dev",
    "cd /Users/von/workspace/hicoder && timeout 900 git push origin dev > $TMPDIR/pushdev5.log 2>&1; echo \"PUSH_EXIT=$?\"; tail -6 $TMPDIR/pushdev5.log; git ls-remote origin refs/heads/dev",
    "cd /Users/von/workspace/hicoder && git checkout main && git merge --ff-only dev && git push origin main > $TMPDIR/pushmain.log 2>&1; echo \"PUSH_EXIT=$?\"; tail -12 $TMPDIR/pushmain.log; git ls-remote origin refs/heads/main refs/heads/dev",
    "cd /Users/von/workspace/hicoder && git checkout main && git merge --ff-only dev && git push origin main > $TMPDIR/pushmain2.log 2>&1; echo \"PUSH_EXIT=$?\"; tail -6 $TMPDIR/pushmain2.log; git ls-remote origin refs/heads/dev refs/heads/main",
];

/// Commands that read local state: commit logs, a pattern search over the
/// bug log, and quoted-heredoc parsers fed from a trusted host. Each one asked
/// in the session because the scan read a word inside data as a command.
const LOCAL_READS: &[&str] = &[
    "cd /Users/von/workspace/hicoder && for h in $(git rev-list --reverse origin/dev..dev); do s=$(git show -s --format=%s $h); n=$(grep -c \"$h\" wiki/review/bug-log.md); echo \"$h $n $s\"; done; echo \"=== hashes cited in bug-log that are in range\"; git rev-list origin/dev..dev | while read h; do :; done; for h in 36f068f7 f031c2b 5ed6cc8a 038db009 ca4028a5; do printf \"%s ancestor-of-origin/dev=\" $h; git merge-base --is-ancestor $h origin/dev && echo yes || echo NO; done",
    "cd /Users/von/workspace/hicoder && for pat in \"summary_tokens\" \"color_hint: 109\" \"Align toward the right edge\" \"c.is_reserved\" \"grid_row_spans\" \"Agents\\\", bold\" \"test_results.insert\\|test_results.push\\|test_results.into_iter().rev()\"; do printf \"%-42s \" \"$pat\"; git log --format='%h' -S \"$pat\" origin/dev~13..origin/dev -- crates/ | tr '\\n' ' '; echo; done",
    "cd /Users/von/workspace/hicoder && echo \"=== summary subtraction\"; git log --format='%h %s' -S \"summary_tokens\" -p origin/dev~13..origin/dev -- crates/houyicoder-service/src/server/context_handler.rs | grep -E \"^[0-9a-f]{8}|summary_tokens\" | head -20; echo \"=== test cap\"; grep -n \"20\\b\\|truncate\\|MAX_TEST\" crates/houyicoder-core/src/agent/compaction/backbone.rs | head -8",
    "cd /Users/von/workspace/hicoder && for h in 69f8927c 7857b007 5cf9a0e7 274af0c0 956bbd17 be9e0150 bc5190f4 70a059f2; do git log -1 --format='%B' $h > $TMPDIR/m.txt; printf \"%s \" $h; python3 scripts/commit_msg_lint.py $TMPDIR/m.txt >/dev/null 2>&1 && echo OK || echo FAIL; done",
    "cd /Users/von/workspace/hicoder && for h in $(git rev-list origin/dev..dev); do git log -1 --format='%B' $h > $TMPDIR/m.txt; printf \"%s \" $(git log -1 --format='%h %s' $h); python3 scripts/commit_msg_lint.py $TMPDIR/m.txt >/dev/null 2>&1 && echo OK || echo FAIL; done",
    "cd /Users/von/workspace/hicoder && grep -n \"72\\|wrap\" scripts/commit_msg_check.sh | head -10; echo \"=== all 8 through the real hook script\"; for h in $(git rev-list --reverse origin/dev..dev); do printf \"%s \" $(git log -1 --format='%h %s' $h); git log -1 --format='%B' $h > $TMPDIR/m.txt; bash scripts/commit_msg_check.sh $TMPDIR/m.txt >/dev/null 2>&1 && echo OK || echo FAIL; done",
    "cd /Users/von/workspace/hicoder && for h in $(git rev-list --reverse origin/dev..dev); do git log -1 --format='%B' $h > $TMPDIR/m.txt; printf \"%s %s \" $(git log -1 --format='%h' $h) OK; bash scripts/commit_msg_check.sh $TMPDIR/m.txt >/dev/null 2>&1 && echo \"\" || echo FAIL; done; echo \"=== tree \u{672a}\u{53d8}?\"; [ \"$(git rev-parse HEAD^{tree})\" = \"$(git rev-parse 70a059f2^{tree})\" ] && echo TREE_SAME || echo TREE_DIFF",
    "cd /Users/von/workspace/hicoder && curl -sL -o $TMPDIR/run.html -w \"%{http_code}\\n\" \"https://github.com/YiLabsAI/HouYiCoder/actions/runs/42650371812\" && python3 - <<'EOF'\nimport re,html\nt=open('/private/var/folders/g7/dblg1fws4731glk1_rzhqs7w0000gn/T/hy-tmp-28503-1/run.html',encoding='utf-8',errors='ignore').read()\nfor m in re.finditer(r'aria-label=\"([^\"]*(?:succeeded|failed|in progress|queued)[^\"]*)\"',t):\n    print(html.unescape(m.group(1))[:120])\nprint('---status words---')\nfor w in ['completed successfully','Some checks were not successful','in_progress','queued']:\n    print(w, t.count(w))\nEOF",
    "cd /Users/von/workspace/hicoder && curl -sL -o $TMPDIR/run2.html -w \"%{http_code}\\n\" \"https://github.com/YiLabsAI/HouYiCoder/actions/runs/42650371812\" && python3 - <<'EOF'\nimport re,html,os\np=os.environ['TMPDIR']+'/run2.html'\nt=open(p,encoding='utf-8',errors='ignore').read()\nprint('len',len(t))\nfor pat in ['test (ubuntu-latest)','test (windows-latest)','test (macos-latest)','coverage','lint','Success','Failure','Queued','In progress']:\n    print(f'{pat!r}:', t.count(pat))\n# job titles near status\nfor m in re.finditer(r'\"name\":\"([^\"]+)\",\"conclusion\":\"([^\"]*)\"',t):\n    print('job',m.group(1),m.group(2))\nfor m in re.finditer(r'workflow-job[^>]*>.{0,400}?</a>', t, re.S):\n    s=re.sub(r'<[^>]+>',' ',m.group(0))\n    print('ROW', ' '.join(s.split())[:160])\nEOF",
    "cd /Users/von/workspace/hicoder && for j in 129274793769 129274794487 129274794865; do echo \"== $j\"; curl -sL \"https://github.com/YiLabsAI/HouYiCoder/actions/runs/42650371812/job/$j\" | python3 -c \"\nimport sys,re\nt=sys.stdin.read()\nm=re.search(r'<title>(.*?)</title>',t,re.S)\nprint(' title:', ' '.join(m.group(1).split())[:120] if m else '?')\nfor k in ['completed successfully','has failed','is in progress','queued']:\n    if k in t: print(' hit:',k)\n\"; done",
    "cd /Users/von/workspace/hicoder && for j in 129274793769 129274794487 129274794865; do echo \"== $j\"; curl -sL \"https://github.com/YiLabsAI/HouYiCoder/actions/runs/42650371812/job/$j\" | python3 -c \"\nimport sys,re\nt=sys.stdin.read()\nfor m in re.finditer(r'data-testid=\\\"job-summary[^\\\"]*\\\"[^>]*>(.*?)</',t,re.S):\n    print(' summary:', ' '.join(re.sub(r'<[^>]+>',' ',m.group(1)).split())[:200])\nm=re.search(r'\\\"status\\\":\\\"(\\w+)\\\",\\\"conclusion\\\":(\\\"[^\\\"]*\\\"|null)',t)\nprint(' status/json:', m.groups() if m else 'none')\nfor m in re.finditer(r'aria-label=\\\"([^\\\"]{0,80}(?:success|fail|progress|queued)[^\\\"]{0,40})\\\"',t):\n    print(' aria:', m.group(1)[:100])\n\"; done",
    "cd /Users/von/workspace/hicoder && curl -sL \"https://github.com/YiLabsAI/HouYiCoder/commit/9f1990e2/checks\" -o $TMPDIR/checks.html -w \"%{http_code}\\n\" && python3 - <<'EOF'\nimport re,os\nt=open(os.environ['TMPDIR']+'/checks.html',encoding='utf-8',errors='ignore').read()\nprint('len',len(t))\nfor w in ['windows','macos','ubuntu','coverage','lint']:\n    print(w,t.count(w))\nfor m in re.finditer(r'href=\"(/[^\"]*/job/\\d+)\"[^>]*>(.*?)</a>',t,re.S):\n    print(' '.join(re.sub(r'<[^>]+>',' ',m.group(2)).split())[:80],'|',m.group(1).rsplit('/',1)[-1])\nEOF",
    "cd ~/.houyicoder/sessions/3879e09d-0718-4513-b641-071ba3f35dfc && python3 - <<'EOF'\nimport json\npath='log.jsonl'\nhits=[]\nfor i,line in enumerate(open(path,encoding='utf-8',errors='ignore'),1):\n    if 'git push origin dev' in line or 'git push origin main' in line:\n        try: e=json.loads(line)\n        except: continue\n        ev=e.get('event',{})\n        t=ev.get('type') or ev.get('event') or list(ev)[:1]\n        hits.append((i,t,line[:260]))\nprint('push lines:',len(hits))\nfor h in hits[:12]: print(h[0],h[1],h[2][:200].replace('\\n',' '))\nEOF",
    "cd ~/.houyicoder/sessions/3879e09d-0718-4513-b641-071ba3f35dfc && python3 - <<'EOF'\nimport json\n# map every ToolCall whose input mentions git push -> its call_id, then find its permission outcome\ncalls={}\nfor i,line in enumerate(open('log.jsonl',encoding='utf-8',errors='ignore'),1):\n    try: e=json.loads(line)\n    except: continue\n    ev=e.get('event',{})\n    if ev.get('type')=='ToolCall':\n        s=json.dumps(ev,ensure_ascii=False)\n        if 'git push origin' in s:\n            calls[ev.get('call_id')]=(i,ev)\nprint('push ToolCalls:',len(calls))\nasked={}; decided={}\nfor i,line in enumerate(open('log.jsonl',encoding='utf-8',errors='ignore'),1):\n    try: e=json.loads(line)\n    except: continue\n    ev=e.get('event',{})\n    t=ev.get('type'); cid=ev.get('call_id')\n    if cid in calls:\n        if t=='PermissionAsked': asked[cid]=i\n        if t=='PermissionDecision': decided[cid]=(i,ev.get('verdict'),ev.get('reason'))\nfor cid,(i,ev) in calls.items():\n    print(f\"line {i} {cid} asked={asked.get(cid)} decided={decided.get(cid)}\")\n    print('   input:', json.dumps(ev.get('input'),ensure_ascii=False)[:180])\nEOF",
    "cd /Users/von/workspace/hicoder && curl -sL -o $TMPDIR/acts.html -w \"%{http_code}\\n\" \"https://github.com/YiLabsAI/HouYiCoder/actions?query=branch%3Adev\" && python3 - <<'EOF'\nimport re,os,html\nt=open(os.environ['TMPDIR']+'/acts.html',encoding='utf-8',errors='ignore').read()\nprint('len',len(t))\n# find run rows with sha\nfor m in re.finditer(r'href=\"(/YiLabsAI/HouYiCoder/actions/runs/\\d+)\"',t):\n    print(m.group(1))\n    break\nfor sha in set(re.findall(r'[0-9a-f]{7,40}',t))[:0]: pass\nfor m in re.finditer(r'(9ea83216|9f1990e2|e0796932)',t):\n    print('sha seen',m.group(1))\n    break\nprint('9ea83216 count',t.count('9ea83216'),'9f1990e2 count',t.count('9f1990e2'))\nEOF",
    "cd /Users/von/workspace/hicoder && curl -sL \"https://github.com/YiLabsAI/HouYiCoder/actions/runs/42654406747\" -o $TMPDIR/r9.html -w \"%{http_code}\\n\" && python3 - <<'EOF'\nimport re,os\nt=open(os.environ['TMPDIR']+'/r9.html',encoding='utf-8',errors='ignore').read()\nprint('len',len(t))\nfor k in ['completed successfully','has failed','is in progress','queued','Success','Failure']:\n    print(k, t.count(k))\nprint('linux/macos/windows counts', t.count('ubuntu-latest'), t.count('macos-latest'), t.count('windows-latest'))\nEOF",
    "cd /Users/von/workspace/hicoder && for i in $(seq 1 28); do sleep 60; h=$(curl -sL \"https://github.com/YiLabsAI/HouYiCoder/actions/runs/42654406727\"); if echo \"$h\" | grep -q \"completed successfully\"; then echo \"RUN_GREEN after $((i))m\"; break; elif echo \"$h\" | grep -q \"has failed\"; then echo \"RUN_FAILED after $((i))m\"; break; fi; done; echo \"done\"",
    "cd /Users/von/workspace/hicoder && for i in $(seq 1 40); do sleep 15; s=$(curl -s \"https://api.github.com/repos/YiLabsAI/HouYiCoder/actions/runs/42601548385/jobs\" | python3 -c \"\nimport json,sys\nd=json.load(sys.stdin)\nprint('|'.join(f\\\"{j['name'].replace(' ', '_')}:{j['status']}:{j['conclusion']}\\\" for j in d.get('jobs',[])))\n\" 2>/dev/null); echo \"$i $s\"; case \"$s\" in *in_progress*|*queued*|\"\") ;; *) break;; esac; done",
];

/// Commands the session asked about that still ask: a write outside the
/// session scratch space, a socket bound outside it, a history rewrite, and a
/// step the text check cannot resolve.
const ASKED: &[&str] = &[
    "cd /Users/von/workspace/hicoder && printf 'probe\\n' > ~/.houyicoder/sessions/.probe-$$ 2>&1 && echo \"HOME_SESSIONS_WRITABLE\" && rm -f ~/.houyicoder/sessions/.probe-$$; python3 -c \"\nimport socket,os\np=os.path.expanduser('~/.houyicoder/sessions/probe-'+str(os.getpid())+'.sock')\ntry:\n    s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);s.bind(p);s.close();os.unlink(p);print('SOCK_OK',p)\nexcept Exception as e: print('SOCK_FAIL',p,type(e).__name__,e)\n\"",
    "cd /Users/von/workspace/hicoder && eval \"$(python3 scripts/cov_lcov.py --env 2>/dev/null)\"; cargo test -p houyicoder-cli --bin houyi detach::tests::test_list_sessions_reads_pid -- --nocapture 2>&1 | tail -15",
    "cd /Users/von/workspace/hicoder && python3 -c \"\nimport os\nfor p in ['/etc','/private/etc']:\n    try:\n        print(p,'stat ok', os.stat(p).st_mode)\n    except Exception as e: print(p,'stat FAIL',e)\ntry:\n    print('listdir /etc:', len(os.listdir('/etc')))\nexcept Exception as e: print('listdir /etc FAIL', type(e).__name__, e)\nprint('realpath', os.path.realpath('/etc'))\n\"; echo \"=== RUST probe via rustc\"; printf 'fn main(){ match std::fs::canonicalize(\"/etc\") { Ok(p)=>println!(\"ok {p:?}\"), Err(e)=>println!(\"err {e}\") } }' > $TMPDIR/p.rs && rustc -O -o $TMPDIR/p $TMPDIR/p.rs 2>&1 | tail -2 && $TMPDIR/p",
    "cd /Users/von/workspace/hicoder && python3 -c \"\nimport socket,os\np='/Users/von/houyicoder/sessions/probe-%d.sock'%os.getpid()\ntry:\n    s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);s.bind(p);s.close();os.unlink(p);print('SOCK_OK',p)\nexcept Exception as e: print('SOCK_FAIL',p,type(e).__name__,e)\n\"; grep -rn \"fn home_directory\" -A10 crates/houyicoder-cli/src/*.rs crates/houyicoder-cli/src/**/*.rs 2>/dev/null | head -15",
    "cd /Users/von/workspace/hicoder && echo \"=== file write probe\"; for p in /Users/von/houyicoder/sessions/probe.txt /Users/von/.houyicoder/sessions/probe.txt; do printf \"%-45s \" \"$p\"; if printf 'x' > \"$p\" 2>/dev/null; then echo WRITE_OK; rm -f \"$p\"; else echo WRITE_DENIED; fi; done; echo \"=== all detach commits ancestors of dev?\"; for h in affb839c 2d41cecf d1baefa1 21d1838d 1dabfa33 19420af9 3658361c; do printf \"%s \" $h; git merge-base --is-ancestor $h dev && echo ancestor || echo \"NOT-on-dev\"; done",
    "cd /Users/von/workspace/hicoder && python3 scripts/commit_msg_lint.py $TMPDIR/newmsg.txt; echo \"LINT_EXIT=$?\"; git log -1 --format='%h %s' be9e0150; git branch backup-dev-before-reword be9e0150 >/dev/null 2>&1; echo \"backup tag:\"; git tag -f backup-reword-$(date +%s) be9e0150 >/dev/null && echo done",
    "cd /Users/von/workspace/hicoder && GIT_SEQUENCE_EDITOR=\"sed -i '' 's/^pick be9e0150/reword be9e0150/'\" GIT_EDITOR=\"cp $TMPDIR/newmsg.txt\" git rebase -i be9e0150^ > $TMPDIR/rebase.log 2>&1; echo \"REBASE_EXIT=$?\"; tail -5 $TMPDIR/rebase.log; git log --oneline origin/dev..dev",
    "cd /Users/von/workspace/hicoder && GIT_SEQUENCE_EDITOR=\"sed -i '' 's/^pick c84ef6a4/reword c84ef6a4/'\" GIT_EDITOR=\"cp $TMPDIR/newmsg2.txt\" git rebase -i c84ef6a4^ > $TMPDIR/rb2.log 2>&1; echo \"RB_EXIT=$?\"; tail -2 $TMPDIR/rb2.log; git log --oneline origin/dev..dev; [ \"$(git rev-parse HEAD^{tree})\" = \"$(git rev-parse 936e9f18^{tree})\" ] && echo TREE_SAME || echo TREE_DIFF",
    "cd /Users/von/workspace/hicoder && GIT_SEQUENCE_EDITOR=\"sed -i '' 's/^pick c84ef6a4/reword c84ef6a4/'\" GIT_EDITOR=\"cp $TMPDIR/newmsg3.txt\" git rebase -i c84ef6a4^ > $TMPDIR/rb3.log 2>&1; echo \"RB_EXIT=$?\"; tail -2 $TMPDIR/rb3.log; git log --oneline origin/dev..dev",
    "cd /Users/von/workspace/hicoder && GIT_SEQUENCE_EDITOR=\"sed -i '' 's/^pick e0796932/reword e0796932/'\" GIT_EDITOR=\"cp $TMPDIR/newmsg5.txt\" git rebase -i e0796932^ > $TMPDIR/rb5.log 2>&1; echo \"RB_EXIT=$?\"; tail -2 $TMPDIR/rb5.log; git log --oneline -2; [ \"$(git rev-parse HEAD^{tree})\" = \"$(git rev-parse e0796932^{tree})\" ] && echo TREE_SAME || echo TREE_DIFF",
];

fn gate() -> (DefaultModeGate, PathBuf) {
    let scratch = temp_dir().join(format!("hy-wrapped-shell-{}", id()));
    create_dir_all(&scratch).expect("mkdir scratch");
    let gate = DefaultModeGate::with_mode(PermissionMode::Auto)
        .with_trusted_hosts()
        .with_containment(Arc::new(ScratchFence {
            scratch: scratch.clone(),
        }));
    (gate, scratch)
}

/// Every recorded push asks, and the prompt names the operation. A push
/// wrapped in a timeout, its output redirected, is still the same operation.
#[test]
fn test_session_pushes_ask() {
    let (gate, scratch) = gate();
    for command in PUSHES {
        match gate.decide(&bash_req(command)) {
            Decision::Ask(reason) => {
                assert_eq!(reason.validator, "network_egress", "{command}");
                assert!(
                    reason
                        .detail
                        .contains("git push publishes local commits to a remote repository"),
                    "the prompt must name the push: {command} -> {}",
                    reason.detail
                );
            }
            other => panic!("push must ask: {command} -> {other:?}"),
        }
    }
    drop(remove_dir_all(&scratch));
}

/// A read that stayed local in the session does not ask. The push text inside
/// a search pattern, a loop header, or a quoted parser body is data, and the
/// loop itself is structure rather than an effect.
#[test]
fn test_session_local_reads_allow() {
    let (gate, scratch) = gate();
    for command in LOCAL_READS {
        match gate.decide(&bash_req(command)) {
            Decision::Allow(_) => {}
            other => panic!("local read must not ask: {command} -> {other:?}"),
        }
    }
    drop(remove_dir_all(&scratch));
}

/// The prompts the session needed still hold: a write outside the scratch
/// space, a socket bound outside it, a history rewrite, and an unprovable
/// evaluation.
#[test]
fn test_session_asked_still_asks() {
    let (gate, scratch) = gate();
    for command in ASKED {
        assert_eq!(
            gate.decide(&bash_req(command)).outcome(),
            Outcome::Ask,
            "must still ask: {command}"
        );
    }
    drop(remove_dir_all(&scratch));
}
