#!/usr/bin/env python3
"""
NRM-091 Proof: Request-watch "went idle/stopped without responding" notice names the real cause.

Demonstrates:
Case 1 (Forwarded message):
  - flex sends request #1 to lead
  - lead receives delivery via hook
  - lead forwards task by sending message to review
  - lead transitions to listening (idle) via hcom listen
  - Notice names the cause: (sent #... to @review (forwarded?))

Case 2 (Delivered via listen, never shown in a turn):
  - lead sends request to fora
  - fora receives delivery via keepalive listener (hcom listen, via listen)
  - fora has listening status but never an active turn
  - fora stops via hcom stop
  - Notice names the cause: (delivered via listen but never shown in a turn)

Case 3 (No activity):
  - lead sends request to worker
  - worker receives delivery via hook
  - worker transitions to listening without any messages or turns
  - Notice names the cause: (no activity)
"""

import json
import os
import shutil
import sqlite3
import subprocess
import sys


def find_binary():
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
    for k in list(env.keys()):
        if k.startswith("HCOM_") and k != "HCOM_DIR":
            env.pop(k, None)
        if "ANTIGRAVITY" in k:
            env.pop(k, None)
    return env


def main():
    hcom_bin = find_binary()
    print(f"Using hcom binary: {hcom_bin}")

    # Use project root tmp directory per project rules
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    hcom_dir = os.path.join(root, "tmp", "hcom-proof-nrm091")
    if os.path.exists(hcom_dir):
        shutil.rmtree(hcom_dir)
    os.makedirs(hcom_dir, exist_ok=True)

    env = clean_env(hcom_dir)

    # Initialize DB by running hcom list
    subprocess.check_output([hcom_bin, "list"], env=env)
    db_path = os.path.join(hcom_dir, "hcom.db")

    print("\n" + "=" * 70)
    print("CASE 1: Recipient forwarded/sent message to another agent before going idle")
    print("=" * 70)

    # Register instances
    conn = sqlite3.connect(db_path)
    conn.execute("INSERT OR REPLACE INTO instances (name, tool, last_event_id, created_at, status) VALUES ('flex', 'claude', 0, 1000.0, 'listening')")
    conn.execute("INSERT OR REPLACE INTO instances (name, tool, last_event_id, created_at, status) VALUES ('lead', 'claude', 0, 1000.0, 'listening')")
    conn.execute("INSERT OR REPLACE INTO instances (name, tool, last_event_id, created_at, status) VALUES ('review', 'claude', 0, 1000.0, 'listening')")
    conn.commit()

    # Step 1: flex sends request to lead
    out = subprocess.check_output([
        hcom_bin, "send", "@lead",
        "--intent", "request",
        "--name", "flex",
        "Task: please review PR #45"
    ], env=env).decode().strip()
    print(f"1. flex sent request to lead: {out}")

    req1_id = conn.execute("SELECT id FROM events WHERE type = 'message' AND json_extract(data, '$.from') = 'flex' ORDER BY id DESC LIMIT 1").fetchone()[0]

    # Step 2: delivery record recorded for lead via hook
    conn.execute(
        "INSERT INTO events (type, instance, timestamp, data) VALUES ('delivery', 'lead', datetime('now'), ?)",
        (json.dumps({"via": "hook", "from_id": 0, "to_id": req1_id, "message_ids": [req1_id], "count": 1}),)
    )
    conn.commit()
    print(f"2. delivery record recorded for lead (request #{req1_id}, via hook)")

    # Step 3: lead forwards task to review
    out = subprocess.check_output([
        hcom_bin, "send", "@review",
        "--name", "lead",
        "Task: HOF-20260916-040, scoped re-review of the RMB-045 fixes"
    ], env=env).decode().strip()
    print(f"3. lead sent message to review: {out}")
    fwd_id = conn.execute("SELECT id FROM events WHERE type = 'message' AND json_extract(data, '$.from') = 'lead' ORDER BY id DESC LIMIT 1").fetchone()[0]

    # Step 4: lead transitions to listening (goes idle)
    subprocess.check_output([hcom_bin, "listen", "--name", "lead", "0"], env=env)
    print("4. lead transitioned to listening (went idle)")

    # Step 5: Inspect generated notification for flex
    notice1 = conn.execute(
        "SELECT json_extract(data, '$.text') FROM events WHERE type = 'message' AND json_extract(data, '$.delivered_to[0]') = 'flex' ORDER BY id DESC LIMIT 1"
    ).fetchone()[0]
    print(f"\nVerbatim Notice Case 1:\n{notice1}")
    assert f"(sent #{fwd_id} to @review (forwarded?))" in notice1, f"Notice does not name forwarded cause: {notice1}"
    print("-> PASS: Notice names forwarded message and target correctly!")

    print("\n" + "=" * 70)
    print("CASE 2: Request delivered to keepalive listener via listen, but never shown in a turn")
    print("=" * 70)

    # Register fora as adhoc (Droid tool identity in hcom)
    conn.execute("INSERT OR REPLACE INTO instances (name, tool, last_event_id, created_at, status) VALUES ('fora', 'adhoc', 0, 1000.0, 'listening')")
    conn.commit()

    # Step 1: lead sends request to fora
    out = subprocess.check_output([
        hcom_bin, "send", "@fora",
        "--intent", "request",
        "--name", "lead",
        "NRM-088 doorbell test: reply pong"
    ], env=env).decode().strip()
    print(f"1. lead sent request to fora: {out}")
    req2_id = conn.execute("SELECT id FROM events WHERE type = 'message' AND json_extract(data, '$.from') = 'lead' AND json_extract(data, '$.delivered_to[0]') = 'fora' ORDER BY id DESC LIMIT 1").fetchone()[0]

    # Step 2: delivered via listen (keepalive listener calls hcom listen --json)
    subprocess.check_output([hcom_bin, "listen", "--name", "fora", "--json", "0"], env=env)
    print(f"2. fora keepalive listener called 'hcom listen --name fora --json 0' (delivered via listen)")

    # Step 3: fora stops (life stopped event via hcom stop)
    subprocess.check_output([hcom_bin, "stop", "--name", "fora"], env=env)
    print("3. fora stopped")

    # Step 4: Inspect generated notification for lead
    notice2 = conn.execute(
        "SELECT json_extract(data, '$.text') FROM events WHERE type = 'message' AND json_extract(data, '$.delivered_to[0]') = 'lead' AND json_extract(data, '$.from') = '[hcom-events]' ORDER BY id DESC LIMIT 1"
    ).fetchone()[0]
    print(f"\nVerbatim Notice Case 2:\n{notice2}")
    assert "(delivered via listen but never shown in a turn)" in notice2, f"Notice does not name listen-unprompted cause: {notice2}"
    print("-> PASS: Notice names unprompted listen cause correctly!")

    print("\n" + "=" * 70)
    print("CASE 3: Recipient goes idle with no other activity")
    print("=" * 70)

    conn.execute("INSERT OR REPLACE INTO instances (name, tool, last_event_id, created_at, status) VALUES ('worker', 'claude', 0, 1000.0, 'listening')")
    conn.commit()

    # Step 1: lead sends request to worker
    out = subprocess.check_output([
        hcom_bin, "send", "@worker",
        "--intent", "request",
        "--name", "lead",
        "Task: please run check"
    ], env=env).decode().strip()
    print(f"1. lead sent request to worker: {out}")
    req3_id = conn.execute("SELECT id FROM events WHERE type = 'message' AND json_extract(data, '$.from') = 'lead' AND json_extract(data, '$.delivered_to[0]') = 'worker' ORDER BY id DESC LIMIT 1").fetchone()[0]

    # Step 2: delivered via hook
    conn.execute(
        "INSERT INTO events (type, instance, timestamp, data) VALUES ('delivery', 'worker', datetime('now'), ?)",
        (json.dumps({"via": "hook", "from_id": 0, "to_id": req3_id, "message_ids": [req3_id], "count": 1}),)
    )
    conn.commit()
    print(f"2. delivery record recorded for worker (request #{req3_id}, via hook)")

    # Step 3: worker goes idle without sending any messages
    subprocess.check_output([hcom_bin, "listen", "--name", "worker", "0"], env=env)
    print("3. worker went idle (listening)")

    notice3 = conn.execute(
        "SELECT json_extract(data, '$.text') FROM events WHERE type = 'message' AND json_extract(data, '$.delivered_to[0]') = 'lead' AND json_extract(data, '$.from') = '[hcom-events]' ORDER BY id DESC LIMIT 1"
    ).fetchone()[0]
    print(f"\nVerbatim Notice Case 3:\n{notice3}")
    assert "(no activity)" in notice3, f"Notice does not name no activity: {notice3}"
    print("-> PASS: Notice names (no activity) correctly!")

    conn.close()
    print("\n" + "=" * 70)
    print("ALL 3 PROOF CASES PASSED VERIFIED!")
    print("=" * 70)


if __name__ == "__main__":
    main()
