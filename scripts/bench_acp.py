#!/usr/bin/env python3
"""Same-machine ACP benchmark.

Measures cold-start handshake latency (initialize, session/new) and resident
memory for a local agent binary, and the pure protocol turnaround for streamed
text deltas and a cancel. Prints a small table; no assertions, so it can run
on any machine and the numbers are copied into the acceptance doc.
"""

import json
import os
import statistics
import subprocess
import sys
import time

DEVNULL = subprocess.DEVNULL

# The official initialize request carries the protocol version and the
# client capability block; a strict agent rejects an empty params object.
INIT = {
    "jsonrpc": "2.0",
    "id": 1,
    "method": "initialize",
    "params": {"protocolVersion": 1, "clientCapabilities": {}},
}


def pct(values, p):
    if not values:
        return None
    ordered = sorted(values)
    k = (len(ordered) - 1) * p / 100.0
    lo = int(k)
    hi = min(lo + 1, len(ordered) - 1)
    return ordered[lo] + (ordered[hi] - ordered[lo]) * (k - lo)


def send(proc, obj):
    proc.stdin.write(json.dumps(obj) + "\n")
    proc.stdin.flush()


def recv(proc):
    return proc.stdout.readline().strip()


def handshake(proc):
    send(proc, INIT)
    if '"id":1' not in recv(proc):
        raise RuntimeError("initialize failed")
    send(
        proc,
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {"cwd": os.getcwd(), "mcpServers": []},
        },
    )
    reply = recv(proc)
    if '"id":2' not in reply:
        raise RuntimeError("session/new failed")
    return json.loads(reply)["result"]["sessionId"]


def bench_handshake(name, cmd, n=10, env=None):
    inits, news, rss = [], [], []
    for _ in range(n):
        merged = {**os.environ, **(env or {})}
        try:
            proc = subprocess.Popen(
                cmd,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=DEVNULL,
                text=True,
                bufsize=1,
                env=merged,
            )
        except OSError:
            return None
        try:
            t0 = time.perf_counter()
            send(proc, INIT)
            if '"id":1' not in recv(proc):
                continue
            t1 = time.perf_counter()
            send(
                proc,
                {
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session/new",
                    "params": {"cwd": os.getcwd(), "mcpServers": []},
                },
            )
            if '"id":2' not in recv(proc):
                continue
            t2 = time.perf_counter()
            inits.append((t1 - t0) * 1000.0)
            news.append((t2 - t1) * 1000.0)
            try:
                out = subprocess.check_output(
                    ["ps", "-o", "rss=", "-p", str(proc.pid)], text=True
                ).strip()
                rss.append(int(out) / 1024.0)
            except (subprocess.CalledProcessError, ValueError):
                pass
        finally:
            proc.kill()
            proc.wait()
    return {
        "name": name,
        "init_p50": pct(inits, 50),
        "init_p95": pct(inits, 95),
        "new_p50": pct(news, 50),
        "new_p95": pct(news, 95),
        "rss": statistics.median(rss) if rss else None,
        "n": len(inits),
    }


def bench_protocol(example, n=10):
    first = []
    for _ in range(n):
        env = {**os.environ, "HOUYICODER_TEST_STUB_DELAY_MS": "0"}
        proc = subprocess.Popen(
            [example],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=DEVNULL,
            text=True,
            bufsize=1,
            env=env,
        )
        try:
            sid = handshake(proc)
            prompt = {
                "jsonrpc": "2.0",
                "id": 3,
                "method": "session/prompt",
                "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "hi"}]},
            }
            t0 = time.perf_counter()
            send(proc, prompt)
            for _ in range(64):
                line = recv(proc)
                if "session/update" in line:
                    first.append((time.perf_counter() - t0) * 1000.0)
                    break
                if '"id":3' in line:
                    break
        finally:
            proc.kill()
            proc.wait()

    cancel = []
    for _ in range(n):
        env = {**os.environ, "HOUYICODER_ACP_STUB_HANG": "1"}
        proc = subprocess.Popen(
            [example],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=DEVNULL,
            text=True,
            bufsize=1,
            env=env,
        )
        try:
            sid = handshake(proc)
            prompt = json.dumps(
                {
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "session/prompt",
                    "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "hi"}]},
                }
            )
            cancel_frame = json.dumps(
                {"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}}
            )
            t0 = time.perf_counter()
            proc.stdin.write(prompt + "\n" + cancel_frame + "\n")
            proc.stdin.flush()
            for _ in range(64):
                line = recv(proc)
                if '"id":3' in line:
                    cancel.append((time.perf_counter() - t0) * 1000.0)
                    break
        finally:
            proc.kill()
            proc.wait()

    return {
        "name": "houyi --acp (mock streaming)",
        "first_p50": pct(first, 50),
        "first_p95": pct(first, 95),
        "cancel_p50": pct(cancel, 50),
        "cancel_p95": pct(cancel, 95),
        "n": len(first),
    }


def bench_handshake_pty(name, cmd, n=5):
    """Measure a handshake over a pseudo-terminal.

    Some agents block-buffer stdout when it is a pipe, flushing only at exit,
    so a piped per-message latency measures buffering rather than the
    protocol. A PTY makes the agent line-buffer like an interactive client.
    Log lines share the PTY, so only JSON-RPC lines are counted.
    """
    import pty
    import select
    import termios

    inits, news = [], []
    for _ in range(n):
        master, slave = pty.openpty()
        attrs = termios.tcgetattr(slave)
        attrs[3] &= ~termios.ECHO
        termios.tcsetattr(slave, termios.TCSANOW, attrs)
        try:
            proc = subprocess.Popen(cmd, stdin=slave, stdout=slave, stderr=slave, close_fds=True)
        except OSError:
            os.close(master)
            os.close(slave)
            return None
        os.close(slave)

        def await_id(wanted, timeout_s=12.0):
            deadline = time.perf_counter() + timeout_s
            buf = b""
            while time.perf_counter() < deadline:
                ready, _, _ = select.select([master], [], [], 0.2)
                if not ready:
                    continue
                try:
                    chunk = os.read(master, 8192)
                except OSError:
                    return False
                buf += chunk
                for line in buf.split(b"\n"):
                    text = line.decode("utf-8", "replace").strip()
                    if '"jsonrpc"' in text and f'"id":{wanted}' in text:
                        return True
            return False

        try:
            t0 = time.perf_counter()
            os.write(master, (json.dumps(INIT) + "\n").encode())
            if not await_id(1):
                continue
            t1 = time.perf_counter()
            os.write(
                master,
                (
                    json.dumps(
                        {
                            "jsonrpc": "2.0",
                            "id": 2,
                            "method": "session/new",
                            "params": {"cwd": os.getcwd(), "mcpServers": []},
                        }
                    )
                    + "\n"
                ).encode(),
            )
            if not await_id(2):
                continue
            t2 = time.perf_counter()
            inits.append((t1 - t0) * 1000.0)
            news.append((t2 - t1) * 1000.0)
        finally:
            proc.kill()
            proc.wait()
            os.close(master)
    return {
        "name": name,
        "init_p50": pct(inits, 50),
        "init_p95": pct(inits, 95),
        "new_p50": pct(news, 50),
        "new_p95": pct(news, 95),
        "rss": None,
        "n": len(inits),
    }


def fmt(value):
    return "n/a" if value is None else f"{value:.2f}"


def main():
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    houyi = os.path.join(root, "target", "release", "houyi")
    example = os.path.join(root, "target", "release", "examples", "acp_stdio")

    rows = [
        bench_handshake("houyi --acp (pipe)", [houyi, "--acp"]),
        bench_handshake_pty("houyi --acp (pty)", [houyi, "--acp"]),
        bench_handshake_pty("devin acp (pty)", ["devin", "acp"]),
    ]
    print("| binary | initialize p50/p95 (ms) | session/new p50/p95 (ms) | RSS (MB) | runs |")
    print("|---|---|---|---|---|")
    for row in rows:
        if row is None:
            continue
        print(
            f"| {row['name']} | {fmt(row['init_p50'])}/{fmt(row['init_p95'])} "
            f"| {fmt(row['new_p50'])}/{fmt(row['new_p95'])} | {fmt(row['rss'])} | {row['n']} |"
        )

    proto = bench_protocol(example)
    print()
    print("| path | first delta p50/p95 (ms) | cancel stop p50/p95 (ms) | runs |")
    print("|---|---|---|---|")
    print(
        f"| {proto['name']} | {fmt(proto['first_p50'])}/{fmt(proto['first_p95'])} "
        f"| {fmt(proto['cancel_p50'])}/{fmt(proto['cancel_p95'])} | {proto['n']} |"
    )


if __name__ == "__main__":
    sys.exit(main())