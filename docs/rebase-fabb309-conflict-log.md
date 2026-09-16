# Rebase of the fork stack onto upstream fabb309 — conflict log

Branch `work/rebase-fabb309`, replay of `79ebde1..347cabd` (81 commits) onto
upstream `fabb309`. 79 commits replayed (one commit split into two by a
follow-up cleanup), 3 dropped as redundant with upstream work:

- `c35a41a` fix(transcript): report unavailable model history honestly —
  upstream `fc2276d` is the same change plus the device-aware signature;
  upstream supersedes.
- `7f00bd6` fix(transcript): make prefix resolution literal and unambiguous —
  upstream `8719445` is the same fix at larger scope; upstream supersedes.
- `a7b9e8a` fix(launcher): preserve selected Node runtime precedence —
  upstream `fabb309` (PR #117) is a superset (RunnerBinaries, tool path
  carried explicitly to `hcom pty`, main.rs changes); upstream supersedes.

## Conflicts and resolutions

| # | commit | file | resolution |
|---|--------|------|------------|
| 1 | `5be0c36` fix(send): report paused PTY delivery accurately | `src/delivery.rs` | Upstream `92546ff` replaced the bare `update_heartbeat` with `refresh_liveness` (wake-state publishing + heartbeat). Kept BOTH: our `gate_status_tracker.observe_blocked_for`/`apply_update` first, then upstream's `refresh_liveness` in place of our bare `db.update_heartbeat` (refresh_liveness subsumes it). Upstream intent kept, ours re-applied on top. |
| 2 | `6428101` test(transcript): reject false no-message diagnostics | `src/commands/transcript.rs` | Upstream changed `no_transcript_error` to `(db, name, display_name, device)` and `--agent` → `--participant`, and added the remote-device test. Took upstream's shape; re-applied our commit's point — the `assert!(!error.contains("no messages have been exchanged"))` guard — on the upstream message. |
| 3 | `c35a41a` (skipped, see above) | — | — |
| 4 | `6c6323a` feat(delivery): record deliveries per recipient (NRM-057) | `src/commands/send.rs` | Upstream `db9ab43` made `send_message` return `(i64, Vec<String>)` and added `--json` with the basic `{event_id, delivered_to}` payload. Our commit's `send_message_with_id` wrapper became redundant — took upstream's single-function shape (kept the `event_id` variable name), and kept OUR richer `json_send_feedback(db, event_id, &delivered_to)` for `--json`. The rest of NRM-057 (per-recipient delivery state) arrived via later commits in the stack. |
| 5 | `a7b9e8a` (skipped, see above) | — | — |
| 6 | `7f00bd6` (skipped, see above) | — | — |
| 7 | `87fd47c` feat(send): add JSON result with delivery state | `src/commands/send.rs` | Same shape as #4: upstream owns the `--json` flag plumbing; ours owns the payload. Resolved to `json_send_feedback(db, event_id, &delivered_to)`. |
| 8 | `be8ce2d` feat(name-stability) stream A (D1/D2/D3/D8) | `src/commands/start.rs` | Upstream still had the legacy raw-delete block (with its remote-row skip) before the tool selection; our stream A replaces it with `finalize_instance_stop` succession handling that already skips `target_remote`. Took ours (`let tool = ctx.tool.as_str();`), dropping the legacy block; the remote-skip intent lives on in the D1 path's `!target_remote` guard. |
| 9 | `af7cf13` fix(name-stability): hook shutdown paths pass the ending session id | `src/hooks/kimi.rs` (deleted upstream) | Upstream `4a0c408` moved kimi into `src/hooks/kimi/` (modularisation). Deleted-by-them/modified-by-us: re-applied our `finalize_session_gated(..., payload.session_id.as_deref())` change in the new location `src/hooks/kimi/handlers.rs:355`. Other files in the commit (gemini, cursor, copilot, omp, pi) applied cleanly. |
| 10 | `b1a0615` fix(name-stability): gate round … claims require a session ref | `src/commands/start.rs` | The test `test_start_rebind_allows_matching_stopped_snapshot_reclaim` had diverged three ways (upstream untouched; an earlier our-stack commit made it a `for session_id in [None, Some]` loop; our final tree has the single-case `start_rebind_opts` form). Resolved to the FINAL tree's shape (single case, `make_ctx` with `CLAUDECODE=1`, expects `claude` + exit 0) — the loop's `None` iteration predates the later relaxation; the final behavior is what must survive. |
| 11 | `81be0d8` fix(name-stability) gate round 2 | `src/commands/start.rs` | `make_ctx` test helper: upstream builds an empty env map; ours scrubs ambient session-id vars from the real env. Took ours (strictly safer for the claim tests; upstream tests don't depend on ambient values). |
| 12 | `66cc880` fix(rebind): record succession from_tool/to_tool | `src/commands/start.rs` | Same helper, `make_claude_ctx`: took ours (ambient scrub + tool-marker scrub, then force CLAUDECODE and the single session-id source). |
