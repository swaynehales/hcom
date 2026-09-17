#!/usr/bin/env python3
"""
NRM-090 Proof: Claude /clear name persistence for adhoc (unmanaged) sessions.

Demonstrates:
1. Caller A (PID A) starts an adhoc Claude session and obtains an allocated instance name.
2. Caller A executes SessionEnd(reason=clear) via an intermediate shell (/bin/sh).
   Instance transitions to inactive (exit:clear); reservation key is stored for Caller A.
3. Foreign Caller B (PID B != PID A) invokes SessionStart(source=clear) in the same directory within 10s.
   Reservation is NOT consumed; Caller B receives the vanilla hint; instance remains inactive.
4. Original Caller A invokes SessionStart(source=clear) via a NEW intermediate shell within 10s.
   Reservation is consumed; instance is restored to listening; new session ID is bound.
"""

import json
import os
import re
import shutil
import subprocess
import sys


def find_binary():
    # If HCOM_BIN env var is set, use it; otherwise look for release or debug binary
    if "HCOM_BIN" in os.environ:
        return os.environ["HCOM_BIN"]
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    release_bin = os.path.join(root, "target", "release", "hcom")
    if os.path.isfile(release_bin):
        return release_bin
    debug_bin = os.path.join(root, "target", "debug", "hcom")
    if os.path.isfile(debug_bin):
        return debug_bin
    raise FileNotFoundError("hcom binary not found. Build release or debug first.")


def clean_env(hcom_dir):
    env = os.environ.copy()
    env["HCOM_DIR"] = hcom_dir
    env["CLAUDECODE"] = "1"
    # Adhoc session: no HCOM_PROCESS_ID or HCOM_LAUNCHED
    for k in list(env.keys()):
        if k.startswith("HCOM_") and k != "HCOM_DIR":
            env.pop(k, None)
        if "ANTIGRAVITY" in k:
            env.pop(k, None)
    return env


def main():
    hcom_bin = find_binary()
    print(f"Using hcom binary: {hcom_bin}")

    hcom_dir = "/tmp/hcom-proof-nrm090"
    if os.path.exists(hcom_dir):
        shutil.rmtree(hcom_dir)
    os.makedirs(hcom_dir, exist_ok=True)

    env_a = clean_env(hcom_dir)
    env_a["CLAUDE_CODE_SESSION_ID"] = "sess-1"

    pid_a = os.getpid()
    print(f"\n--- STEP 1: Caller A (PID {pid_a}) starts adhoc session ---")
    out_start = subprocess.check_output([hcom_bin, "start"], env=env_a).decode()
    match = re.search(r"\[hcom:([a-z]{4})\]", out_start)
    if not match:
        print(f"Failed to parse allocated name from start output:\n{out_start}")
        sys.exit(1)
    inst_name = match.group(1)
    print(f"Allocated instance name: '{inst_name}' (session: sess-1)")

    # Verify initial active state
    list_out = subprocess.check_output([hcom_bin, "list", "--json"], env=env_a).decode()
    inst = json.loads(list_out)[0]
    print(f"Initial state: name={inst['name']} status={inst['status']} session={inst['session_id']}")
    assert inst["name"] == inst_name
    assert inst["status"] in ("launching", "listening")

    print(f"\n--- STEP 2: Caller A SessionEnd(reason=clear) via intermediate shell ---")
    payload_end = json.dumps({
        "session_id": "sess-1",
        "transcript_path": "/tmp/test.jsonl",
        "reason": "clear",
    })
    # Run through /bin/sh to prove intermediate shell handling
    cmd_end = f'echo "sh1_pid=$$ sh1_ppid=$(ps -o ppid= -p $$ | tr -d \' \')" >&2; echo \'{payload_end}\' | {hcom_bin} sessionend'
    p_end = subprocess.Popen(
        ["/bin/sh", "-c", cmd_end],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env_a,
    )
    _, err_end = p_end.communicate()
    print(f"SessionEnd executed: {err_end.decode().strip()}")
    assert p_end.returncode == 0

    list_out = subprocess.check_output([hcom_bin, "list", "--json"], env=env_a).decode()
    inst = json.loads(list_out)[0]
    print(f"State after clear: name={inst['name']} status={inst['status']} context={inst['status_context']}")
    assert inst["name"] == inst_name
    assert inst["status"] == "inactive"
    assert inst["status_context"] == "exit:clear"

    print(f"\n--- STEP 3: Foreign Caller B (PID B != PID A) attempts SessionStart(clear) ---")
    # Subprocess runs with a fresh PID != pid_a
    neg_script = f"""
import os, sys, json, subprocess
env_b = os.environ.copy()
pid_b = os.getpid()
print(f"Caller B PID: {{pid_b}} (Caller A PID: {pid_a})")
payload = json.dumps({{"session_id": "sess-2-foreign", "transcript_path": "/tmp/test.jsonl", "source": "clear"}})
p = subprocess.Popen(
    ['{hcom_bin}', 'sessionstart'],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    env=env_b
)
out, err = p.communicate(payload.encode())
print(f"Caller B SessionStart exit code: {{p.returncode}}")
print(f"Caller B SessionStart stdout: {{out.decode().strip()}}")
sys.exit(0 if p.returncode == 0 else 1)
"""
    p_neg = subprocess.Popen([sys.executable, "-c", neg_script], env=env_a)
    p_neg.wait()
    assert p_neg.returncode == 0

    # Verify instance was NOT claimed by Caller B and remains inactive
    list_out = subprocess.check_output([hcom_bin, "list", "--json"], env=env_a).decode()
    inst = json.loads(list_out)[0]
    print(f"State after Caller B: name={inst['name']} status={inst['status']} session={inst['session_id']}")
    assert inst["name"] == inst_name
    assert inst["status"] == "inactive", "Foreign start must NOT restore instance"
    assert inst["session_id"] != "sess-2-foreign", "Foreign start must NOT bind session"
    print("Verification: Foreign Caller B rejected; instance remains inactive.")

    print(f"\n--- STEP 4: Original Caller A (PID A) executes SessionStart(clear) ---")
    payload_start = json.dumps({
        "session_id": "sess-3-successor",
        "transcript_path": "/tmp/test.jsonl",
        "source": "clear",
    })
    # Run through a NEW /bin/sh to prove ancestor walk reaches pid_a
    cmd_start = f'echo "sh2_pid=$$ sh2_ppid=$(ps -o ppid= -p $$ | tr -d \' \')" >&2; echo \'{payload_start}\' | {hcom_bin} sessionstart'
    p_start = subprocess.Popen(
        ["/bin/sh", "-c", cmd_start],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env_a,
    )
    out_start, err_start = p_start.communicate()
    print(f"SessionStart executed: {err_start.decode().strip()}")
    assert p_start.returncode == 0

    parsed = json.loads(out_start.decode().strip())
    ctx_text = parsed.get("hookSpecificOutput", {}).get("additionalContext", "")
    assert inst_name in ctx_text, f"Expected '{inst_name}' in bootstrap text"
    print(f"Bootstrap verified: contains '[hcom:{inst_name}]'")

    list_out = subprocess.check_output([hcom_bin, "list", "--json"], env=env_a).decode()
    inst = json.loads(list_out)[0]
    print(f"Final state: name={inst['name']} status={inst['status']} session={inst['session_id']}")
    assert inst["name"] == inst_name
    assert inst["status"] == "listening", "Original caller must restore instance to listening"
    assert inst["session_id"] == "sess-3-successor", "Successor session must be bound"

    print(f"\n=== SUCCESS: NRM-090 proof verified cleanly ===")


if __name__ == "__main__":
    main()
