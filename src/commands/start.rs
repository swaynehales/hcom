//! Start command: `hcom start [--name <agent-id>] [--as <name>] [--orphan <name|pid>]`
//!
//! Runs inside an already-running tool session rather than launching a new one.
//! Used for adhoc/manual setup, identity rebinding, and orphan recovery:
//! - Bare start: detect vanilla tool or create adhoc instance
//! - `--name <agent-id>`: register a subagent (a router-level global flag, not
//!   parsed by `StartArgs` — resolved in `run()` via `flags.name`)
//! - `--orphan`: recover orphaned PTY process
//! - `--as`: rebind session identity

use anyhow::{Result, bail};
use rusqlite::OptionalExtension;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::bootstrap;
use crate::claude_actor;
use crate::config::HcomConfig;
use crate::db::{HcomDb, InstanceRow};
use crate::identity;
use crate::instance_binding;
use crate::instance_lifecycle as lifecycle;
use crate::instance_names;
use crate::instances;
use crate::log::log_info;
use crate::paths;
use crate::pidtrack;
use crate::relay;
use crate::router::GlobalFlags;
use crate::shared::constants::{ST_ACTIVE, ST_INACTIVE};
use crate::shared::context::HcomContext;

/// Parsed arguments for `hcom start`.
#[derive(clap::Parser, Debug)]
#[command(name = "start", about = "Start hcom participation")]
pub struct StartArgs {
    /// Rebind to a different instance name
    #[arg(long = "as")]
    pub as_name: Option<String>,
    /// Adopt a name with no row and no tombstone (D6 escape hatch; the
    /// resulting claim is provisional)
    #[arg(long = "adopt-unknown")]
    pub adopt_unknown: bool,
    /// Take over a LIVE identity (D4b: live takeover and nothing else; the
    /// displaced session ref is logged)
    #[arg(long)]
    pub force: bool,
    /// Recover orphaned PTY process by name or PID
    #[arg(long)]
    pub orphan: Option<String>,
}

/// Claim-policy flags threaded into the rebind/claim path.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaimOptions {
    pub adopt_unknown: bool,
    pub force: bool,
}

/// Run the start command.
pub fn run(argv: &[String], flags: &GlobalFlags) -> Result<i32> {
    // Filter out global flags already consumed by the router (start, --name X, --go)
    let mut filtered = vec!["start".to_string()];
    let mut skip_next = false;
    for arg in argv {
        if skip_next {
            skip_next = false;
            continue;
        }
        match arg.as_str() {
            "start" | "--go" => continue,
            "--name" => {
                skip_next = true;
                continue;
            }
            _ => filtered.push(arg.clone()),
        }
    }

    use clap::Parser;
    let start_args = match StartArgs::try_parse_from(&filtered) {
        Ok(a) => a,
        Err(e) => {
            e.print().ok();
            return Ok(if e.use_stderr() { 1 } else { 0 });
        }
    };

    let orphan_target = start_args.orphan;
    let rebind_target = start_args.as_name;
    let claim_opts = ClaimOptions {
        adopt_unknown: start_args.adopt_unknown,
        force: start_args.force,
    };

    let db = HcomDb::open()?;
    let hcom_dir = paths::hcom_dir();

    let ctx = HcomContext::from_os();
    let verified_actor = claude_actor::resolve_env_actor(&db).map_err(anyhow::Error::new)?;
    if let (Some(actor), Some(name)) = (verified_actor.as_ref(), flags.name.as_deref()) {
        claude_actor::ensure_explicit_matches(&db, actor, name).map_err(anyhow::Error::new)?;
    }

    let requested_name = flags
        .name
        .as_deref()
        .map(|name| identity::resolve_display_name(&db, name).unwrap_or_else(|| name.to_string()));

    // A verified child actor can only promote/use its existing row. It cannot
    // rebind or recover another identity, and it does not need --name.
    if let Some(actor) = verified_actor.as_ref()
        && let Some(actor_row) = db.get_instance_full(&actor.name)?
        && instances::is_subagent_instance(&actor_row)
    {
        if rebind_target.is_some() {
            println!("[HCOM] Subagents cannot use --as. End your turn.");
            return Ok(1);
        }
        if orphan_target.is_some() {
            println!("[HCOM] Subagents cannot use --orphan. End your turn.");
            return Ok(1);
        }
        return start_subagent(&db, &actor_row);
    }

    // Without a capability, retain the ordinary manual fallback. A direct
    // indexed child lookup supports the documented --name <agent-id> form
    // without scanning duplicated parent JSON.
    let subagent_via_name = if verified_actor.is_none() {
        requested_name
            .as_deref()
            .and_then(|id| detect_subagent(&db, id))
    } else {
        None
    };
    let subagent_via_as = if verified_actor.is_none() {
        rebind_target
            .as_deref()
            .and_then(|id| detect_subagent(&db, id))
    } else {
        None
    };

    if subagent_via_as.is_some() || (subagent_via_name.is_some() && rebind_target.is_some()) {
        println!("[HCOM] Subagents cannot change identity. End your turn.");
        return Ok(1);
    }

    if let Some(orphan) = orphan_target {
        return start_from_orphan(&db, &hcom_dir, &orphan, &ctx);
    }

    if let Some(rebind) = rebind_target {
        let current_name = verified_actor
            .as_ref()
            .map(|actor| actor.name.as_str())
            .or(requested_name.as_deref());
        return start_rebind_opts(&db, &rebind, &ctx, current_name, claim_opts);
    }

    if let Some(subagent) = subagent_via_name {
        return start_subagent(&db, &subagent);
    }

    // A verified root actor stays the root even while children exist.
    let effective_name = verified_actor
        .as_ref()
        .map(|actor| actor.name.as_str())
        .or(requested_name.as_deref());
    start_bare(&db, &hcom_dir, &ctx, effective_name)
}

/// Resolve a live child row directly by agent_id (or by its exact row name).
fn detect_subagent(db: &HcomDb, check_id: &str) -> Option<InstanceRow> {
    let name = db
        .get_instance_by_agent_id(check_id)
        .ok()
        .flatten()
        .unwrap_or_else(|| check_id.to_string());
    let row = db.get_instance_full(&name).ok().flatten()?;
    row.parent_name.as_ref().filter(|name| !name.is_empty())?;
    Some(row)
}

/// Promote an existing dormant child row into active hcom participation.
fn start_subagent(db: &HcomDb, info: &InstanceRow) -> Result<i32> {
    let parent_name = info.parent_name.as_deref().unwrap_or("");
    if parent_name.is_empty() || info.agent_id.as_deref().unwrap_or("").is_empty() {
        bail!(
            "Subagent row '{}' is missing parent/agent identity",
            info.name
        );
    }

    let was_announced = info.name_announced != 0;
    lifecycle::set_status(db, &info.name, ST_ACTIVE, "tool:start", Default::default());
    instance_binding::capture_and_store_launch_context(db, &info.name);

    log_info(
        "lifecycle",
        "start.subagent",
        &format!(
            "name={} parent={} agent_id={} announced={}",
            info.name,
            parent_name,
            info.agent_id.as_deref().unwrap_or(""),
            was_announced
        ),
    );

    if was_announced {
        println!("hcom already started for {}", info.name);
        return Ok(0);
    }

    let bootstrap = bootstrap::get_subagent_bootstrap(&info.name, parent_name);
    if !bootstrap.is_empty() {
        println!("{bootstrap}");
    }
    let mut updates = serde_json::Map::new();
    updates.insert("name_announced".into(), serde_json::json!(true));
    instances::update_instance_position(db, &info.name, &updates);

    Ok(0)
}

/// Recover orphaned PTY process by PID or name.
fn start_from_orphan(
    db: &HcomDb,
    hcom_dir: &std::path::Path,
    target: &str,
    _ctx: &HcomContext,
) -> Result<i32> {
    let active_pids: HashSet<u32> = db
        .iter_instances_full()?
        .iter()
        .filter_map(|inst| inst.pid.map(|p| p as u32))
        .collect();
    let orphans = pidtrack::get_orphan_processes(hcom_dir, Some(&active_pids));

    if orphans.is_empty() {
        bail!("No orphan processes found.");
    }

    // Match by PID or name
    let orphan = if let Ok(pid) = target.parse::<u32>() {
        match orphans.iter().find(|o| o.pid == pid) {
            Some(o) => o,
            None => bail!("Orphan PID {} not found.", pid),
        }
    } else {
        let matches: Vec<_> = orphans
            .iter()
            .filter(|o| o.names.contains(&target.to_string()))
            .collect();
        match matches.len() {
            0 => bail!("Orphan '{}' not found.", target),
            1 => matches[0],
            _ => {
                let pids: Vec<String> = matches.iter().map(|m| m.pid.to_string()).collect();
                bail!(
                    "Multiple orphans match '{}' (PIDs: {}). Use --orphan <pid>.",
                    target,
                    pids.join(", ")
                );
            }
        }
    };

    let pid = orphan.pid;

    if orphan.process_id.is_empty() {
        bail!(
            "Orphan PID {} has no process_id and cannot be recovered.",
            pid
        );
    }

    let preferred_name = orphan.names.last().cloned().unwrap_or_default();
    let can_reuse = !preferred_name.is_empty()
        && identity::is_valid_base_name(&preferred_name)
        && db.get_instance_full(&preferred_name)?.is_none();
    let name = if can_reuse {
        preferred_name
    } else {
        instance_names::generate_unique_name(db)?
    };

    // Core DB registration
    let _ = pidtrack::recover_single_orphan_to_db(db, orphan, &name);

    db.log_event(
        "life",
        &name,
        &json!({
            "action": "started",
            "by": "cli",
            "reason": "orphan_recover",
            "orphan_pid": pid,
        }),
    )
    .ok();

    pidtrack::remove_pid(hcom_dir, pid);

    println!("[hcom:{}]", name);
    if can_reuse {
        println!("Recovered orphan PID {} as '{}'.", pid, name);
    } else {
        println!(
            "Recovered orphan PID {} as new identity '{}' (name conflict/unavailable).",
            pid, name
        );
    }

    log_info(
        "start",
        "orphan.recovered",
        &format!("name={} pid={} tool={}", name, pid, orphan.tool),
    );

    Ok(0)
}

#[derive(Debug, Clone)]
struct ChildLink {
    name: String,
    parent_name: Option<String>,
}

fn snapshot_child_links(db: &HcomDb, session_id: Option<&str>) -> Result<Vec<ChildLink>> {
    let Some(session_id) = session_id.filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let mut stmt = db
        .conn()
        .prepare("SELECT name, parent_name FROM instances WHERE parent_session_id = ?")?;
    let rows = stmt.query_map(rusqlite::params![session_id], |row| {
        Ok(ChildLink {
            name: row.get(0)?,
            parent_name: row.get(1)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn restore_child_links_after_root_rebind(
    tx: &rusqlite::Transaction<'_>,
    links: &[ChildLink],
    session_id: &str,
    old_root: &str,
    new_root: &str,
) -> Result<()> {
    for link in links {
        let parent_name = match link.parent_name.as_deref() {
            Some(parent) if parent == old_root => Some(new_root),
            other => other,
        };
        tx.execute(
            "UPDATE instances SET parent_session_id = ?, parent_name = ? WHERE name = ?",
            rusqlite::params![session_id, parent_name, &link.name],
        )?;
    }
    Ok(())
}

/// D3 claim liveness: computed status over stored fields and heartbeat, with
/// an alive-pid backstop — a stale-heartbeat row whose process is still
/// running counts live, because `stop_instance` would group-kill it.
fn claim_target_is_live(db: &HcomDb, row: &InstanceRow) -> bool {
    let computed = lifecycle::get_instance_status(row, db);
    if computed.status != ST_INACTIVE {
        return true;
    }
    if let Some(pid) = row.pid
        && pid > 0
        && crate::sys::process::is_alive(pid as u32)
    {
        return true;
    }
    false
}

fn row_is_remote(row: &InstanceRow) -> bool {
    row.origin_device_id.as_deref().is_some_and(|v| !v.is_empty())
}

/// Rebind session identity (`--as <name>`), preserving last_event_id and any
/// live Claude child hierarchy owned by the current root actor.
fn start_rebind_opts(
    db: &HcomDb,
    rebind_target: &str,
    ctx: &HcomContext,
    explicit_name: Option<&str>,
    opts: ClaimOptions,
) -> Result<i32> {
    let hcom_dir = paths::hcom_dir();

    // D5: validate the requested name on every claim entry point; the Ok
    // value is the resolved base name, so it is the name claimed.
    let target_name = match identity::validate_claim_name(db, rebind_target) {
        Ok(name) => name,
        Err(err) => {
            eprintln!("Error: {err}");
            return Ok(1);
        }
    };

    // Guard: refuse to reclaim a subagent slot. Subagents share their parent's
    // session_id, so `hcom start --as <subagent_name>` from inside a subagent
    // bash would rebind session_bindings[parent_sid] to the subagent name,
    // clobbering the parent's identity. `--as` is documented for top-level
    // restartable identities (compaction/resume/clear), not for subagent
    // lifecycle — which has its own SubagentStart bootstrap path.
    if db.was_subagent_name(&target_name) {
        eprintln!(
            "Error: '{target_name}' is a subagent slot; cannot be reclaimed with --as.\n\
             Subagents register via 'hcom start --name <agent-id>' in the SubagentStart context. If your session ended, stop working and end your turn."
        );
        return Ok(1);
    }

    let explicit_current_name = explicit_name.unwrap_or("");

    // Resolve session_id from process binding or existing instance
    let mut session_id: Option<String> = None;
    if let Some(ref process_id) = ctx.process_id
        && let Ok(Some((sid, _))) = db.get_process_binding_full(process_id)
    {
        session_id = sid.filter(|s| !s.is_empty());
    }
    if session_id.is_none()
        && !explicit_current_name.is_empty()
        && let Ok(Some(current_data)) = db.get_instance_full(explicit_current_name)
    {
        session_id = current_data.session_id.filter(|s| !s.is_empty());
    }
    if session_id.is_none() && ctx.tool == crate::tool::Tool::Claude {
        // A vanilla Claude session has neither a process binding nor, before its
        // first start, a row to read the id back from. Its own session id is
        // what makes the rebind stick: without it the reclaimed name stays
        // unbound and the identity it replaces is never cleaned up.
        session_id = resolve_claude_session_id(&ctx.raw_env);
    }
    let current_name = if !explicit_current_name.is_empty() {
        explicit_current_name.to_string()
    } else if let Some(ref sid) = session_id {
        db.get_session_binding(sid)?.unwrap_or_default()
    } else {
        String::new()
    };
    let child_links = snapshot_child_links(db, session_id.as_deref())?;

    let target_meta = load_rebind_target_metadata(db, &target_name).ok();
    if let Some(ref meta) = target_meta {
        ensure_rebind_compatible(&target_name, meta, ctx)?;
    }

    // Preserve last_event_id from target (cursor preservation)
    let mut last_event_id = target_meta.as_ref().map(|m| m.last_event_id);
    let target_data = db.get_instance_full(&target_name)?;

    // D6: a name with no row and no tombstone is not a free claim — it lets
    // anyone adopt an archived role name with no metadata to check. Gated
    // behind --adopt-unknown on start --as only (D4b).
    if target_data.is_none() && target_meta.is_none() && !opts.adopt_unknown {
        eprintln!(
            "Error: '{target_name}' has no identity history (no row, no tombstone). Refusing an unknown-name claim — pass --adopt-unknown to adopt it as a provisional name."
        );
        return Ok(1);
    }

    // D3 liveness gate. The caller's own row (compaction re-run: its session
    // is still bound to the target) and remote rows (relay continuity, updated
    // in place) are exempt; everything else live refuses — unless --force
    // (D4b: live takeover and nothing else).
    let own_session = session_id.as_deref().is_some_and(|sid| {
        db.get_session_binding(sid)
            .ok()
            .flatten()
            .as_deref()
            == Some(target_name.as_str())
    });
    let target_remote = target_data.as_ref().is_some_and(|row| row_is_remote(row));
    if let Some(ref row) = target_data
        && !own_session
        && !target_remote
        && claim_target_is_live(db, row)
    {
        if !opts.force {
            eprintln!(
                "Error: '{target_name}' is live. Refusing to claim a running identity — stop it first, reclaim it after it exits, or pass --force to take it over."
            );
            return Ok(1);
        }
        let _ = db.log_event(
            "life",
            &target_name,
            &json!({
                "action": "force_claim",
                "by": "start --as",
                "reason": "live takeover",
                "displaced_name": target_name,
                "displaced_session_id": row.session_id,
                "claimer_session_id": session_id,
            }),
        );
        log_info(
            "start",
            "claim.force",
            &format!("instance={} displaced_session={:?}", target_name, row.session_id),
        );
    }

    // Final fallback: use current max to avoid re-delivering old messages
    if last_event_id.is_none() {
        last_event_id = Some(db.get_last_event_id());
    }

    let tool = ctx.tool.as_str();
    let cwd_override = ctx.cwd.to_string_lossy().to_string();

    // D1 phase 1 — predecessor stops run OUTSIDE the claim transaction
    // (finalize_instance_stop opens its own BEGIN IMMEDIATE, which does not
    // nest). D8: the stops go through finalize_instance_stop directly, never
    // stop_instance — no subagent recursion, no pid signals on a possibly
    // alive process, and the CAS means a row replaced mid-teardown is not
    // touched.
    if let Some(ref row) = target_data
        && !target_remote
        && !own_session
    {
        let snapshot = db
            .get_instance_snapshot(&target_name)
            .ok()
            .flatten()
            .unwrap_or(serde_json::Value::Null);
        if let Err(e) = db.finalize_instance_stop(
            &target_name,
            row.created_at,
            row.session_id.as_deref(),
            row.agent_id.as_deref(),
            &json!({
                "action": "stopped",
                "by": "start --as",
                "reason": "succession-claimed",
                "snapshot": snapshot,
            }),
        ) {
            eprintln!("[hcom] warn: succession stop of {target_name} failed: {e}");
        }
    }

    // D8 ordering — rename path: migrate notify endpoints BEFORE the rename
    // stop, so the wake ports survive under the target name when the old
    // identity is tombstoned. The rename tombstone carries `renamed_to` so a
    // later resume of the old name refuses instead of relaunching a moved
    // identity.
    if !current_name.is_empty() && current_name != target_name {
        if ctx.process_id.is_some()
            && let Err(e) = db.migrate_notify_endpoints(&current_name, &target_name)
        {
            eprintln!("[hcom] warn: migrate_notify_endpoints failed: {e}");
        }
        if let Ok(Some(cur_row)) = db.get_instance_full(&current_name) {
            let snapshot = db
                .get_instance_snapshot(&current_name)
                .ok()
                .flatten()
                .unwrap_or(serde_json::Value::Null);
            if let Err(e) = db.finalize_instance_stop(
                &current_name,
                cur_row.created_at,
                cur_row.session_id.as_deref(),
                cur_row.agent_id.as_deref(),
                &json!({
                    "action": "stopped",
                    "by": "start --as",
                    "reason": "renamed",
                    "renamed_to": target_name,
                    "session_id": serde_json::Value::Null,
                    "snapshot": snapshot,
                }),
            ) {
                eprintln!("[hcom] warn: rename stop of {current_name} failed: {e}");
            }
        }
    }

    // D1 phase 2 — the claim: one BEGIN IMMEDIATE covering the CAS re-check,
    // row insert (or in-place update for the caller's own row), binding
    // cleanup, session/process bindings, and child-link restore. Two fresh
    // claimers serialize here; the loser's CAS read finds a row and bails.
    let sid = session_id.clone();
    let claim = db.with_immediate_transaction(|tx| {
        let existing: Option<(f64, i64)> = tx
            .query_row(
                "SELECT created_at, last_event_id FROM instances WHERE name = ?",
                rusqlite::params![&target_name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (in_place_created_at, in_place_cursor) = match existing {
            Some((created_at, cursor)) => {
                if !own_session {
                    bail!("'{target_name}' was claimed concurrently");
                }
                (Some(created_at), Some(cursor))
            }
            None => (None, None),
        };

        tx.execute(
            "DELETE FROM session_bindings WHERE instance_name = ?",
            rusqlite::params![&target_name],
        )?;
        tx.execute(
            "DELETE FROM process_bindings WHERE instance_name = ?",
            rusqlite::params![&target_name],
        )?;
        if let Some(ref s) = sid {
            tx.execute(
                "UPDATE instances SET session_id = NULL WHERE session_id = ? AND name != ?",
                rusqlite::params![s, &target_name],
            )?;
        }

        let now = crate::shared::time::now_epoch_f64();
        let mut data = serde_json::Map::new();
        data.insert(
            "session_id".into(),
            match sid {
                Some(ref s) if !s.is_empty() => json!(s),
                _ => serde_json::Value::Null,
            },
        );
        data.insert("directory".into(), json!(cwd_override));
        data.insert("tool".into(), json!(tool));
        data.insert("background".into(), json!(0));
        data.insert("name_announced".into(), json!(1));
        if let Some(eid) = last_event_id {
            data.insert("last_event_id".into(), json!(eid));
        }
        if let Some(created_at) = in_place_created_at {
            data.insert("created_at".into(), json!(created_at));
            if let Some(cursor) = in_place_cursor
                && last_event_id.is_none()
            {
                data.insert("last_event_id".into(), json!(cursor));
            }
        } else {
            data.insert("created_at".into(), json!(now));
            data.insert("last_stop".into(), json!(0));
            data.insert("transcript_path".into(), json!(""));
            data.insert("tag".into(), serde_json::Value::Null);
            data.insert("status".into(), json!(ST_INACTIVE));
            data.insert("status_time".into(), json!(crate::shared::time::now_epoch_i64()));
            data.insert("status_context".into(), json!("new"));
            data.insert(
                "wait_timeout".into(),
                json!(HcomConfig::effective_timeout()),
            );
        }
        HcomDb::save_instance_named_in_tx(tx, &target_name, &data)?;

        if let Some(ref s) = sid {
            tx.execute(
                "INSERT INTO session_bindings (session_id, instance_name, created_at)
                 VALUES (?, ?, ?)
                 ON CONFLICT(session_id) DO UPDATE SET
                     instance_name = excluded.instance_name,
                     created_at = excluded.created_at",
                rusqlite::params![s, &target_name, now],
            )?;
        }
        if let Some(ref process_id) = ctx.process_id {
            let pid_sid = sid.as_deref().unwrap_or("");
            tx.execute(
                "INSERT OR REPLACE INTO process_bindings (process_id, session_id, instance_name, updated_at)
                 VALUES (?, ?, ?, ?)",
                rusqlite::params![
                    process_id,
                    if pid_sid.is_empty() {
                        None
                    } else {
                        Some(pid_sid)
                    },
                    &target_name,
                    now
                ],
            )?;
        }

        if let Some(ref s) = sid {
            let old_root = if current_name.is_empty() {
                target_name.as_str()
            } else {
                current_name.as_str()
            };
            restore_child_links_after_root_rebind(tx, &child_links, s, old_root, &target_name)?;
        }
        Ok(())
    });
    if let Err(e) = claim {
        eprintln!("Error: claim of '{target_name}' failed: {e}");
        return Ok(1);
    }

    // GAP 1 — the ordinary succession is a logged transfer: a claim over a
    // stopped-or-tombstoned name records the label moving between session
    // refs, with the same displaced/claimer fields --force logs. A consented
    // succession and a forced live takeover must stay distinguishable, so
    // this is action "succession", never "force_claim".
    let displaced_session = if own_session || target_remote {
        None
    } else if let Some(ref row) = target_data {
        row.session_id.clone().filter(|s| !s.is_empty())
    } else {
        target_meta
            .as_ref()
            .map(|m| m.session_id.clone())
            .filter(|s| !s.is_empty())
    };
    if let Some(displaced_sid) = displaced_session {
        let _ = db.log_event(
            "life",
            &target_name,
            &json!({
                "action": "succession",
                "by": "start --as",
                "reason": "label transferred to a new session ref",
                "displaced_name": target_name,
                "displaced_session_id": displaced_sid,
                "claimer_session_id": session_id,
                "last_event_id": last_event_id,
            }),
        );
        log_info(
            "start",
            "claim.succession",
            &format!(
                "instance={} displaced_session={displaced_sid} claimer_session={:?}",
                target_name, session_id
            ),
        );
    }

    // Post-claim bookkeeping outside the transaction: Claude actor state and
    // the validated-session cache are keyed by session id and are recovered by
    // a later hook if they fail here.
    if let Some(ref sid) = session_id {
        let old_root = if current_name.is_empty() {
            target_name.as_str()
        } else {
            current_name.as_str()
        };
        if old_root != target_name
            && let Err(e) = db.rebind_claude_root_actor_state(sid, old_root, &target_name)
        {
            eprintln!("[hcom] warn: rebind_claude_root_actor_state failed for {target_name}: {e}");
        }
        if ctx.tool == crate::tool::Tool::Claude
            && let Err(e) = db.mark_claude_session_validated(sid, &target_name)
        {
            // The cache still names the identity being replaced, and it is keyed
            // by session generation, so it does not expire on its own. Left
            // stale, every hook for this session resolves to no_instance: no
            // status, no delivery, and the reclaimed row is flagged
            // launch_failed ~30s later while the session is alive and bound.
            eprintln!("[hcom] warn: mark_claude_session_validated failed for {target_name}: {e}");
        }
    }

    // Fresh-claim parity with initialize_instance_in_position_file's created
    // path: default subscriptions and the created life event. Best-effort.
    if !own_session && !target_remote {
        let _ = db.log_event(
            "life",
            &target_name,
            &json!({
                "action": "created",
                "by": "start --as",
                "is_hcom_launched": false,
                "is_subagent": false,
                "parent_name": "",
            }),
        );
        instance_binding::auto_subscribe_defaults(db, &target_name, tool);
    }

    if ctx.process_id.is_some() {
        crate::notify::wake(db, &target_name, crate::notify::WakeKind::DELIVERY_LOOPS);
    }

    // Print bootstrap
    let hcom_config = HcomConfig::load(None).unwrap_or_else(|_| {
        let mut c = HcomConfig::default();
        c.normalize();
        c
    });

    let bootstrap_text = bootstrap::get_bootstrap(
        db,
        &hcom_dir,
        &target_name,
        tool,
        false,
        false,
        &ctx.notes,
        &hcom_config.tag,
        relay::is_relay_enabled(&hcom_config),
        None,
    );

    println!("[hcom:{}]", target_name);
    println!("{}", bootstrap_text);
    // Same reason as bare start: keep the new name visible in a tailed snapshot.
    println!("[hcom:{}]", target_name);

    log_info(
        "start",
        "rebind.complete",
        &format!("from={} to={}", current_name, target_name),
    );

    Ok(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RebindTargetMetadata {
    tool: String,
    directory: String,
    last_event_id: i64,
    session_id: String,
}

fn ensure_rebind_compatible(
    target_name: &str,
    meta: &RebindTargetMetadata,
    ctx: &HcomContext,
) -> Result<()> {
    let current_tool = ctx.tool.as_str();
    if !meta.tool.is_empty() && meta.tool != current_tool {
        bail!(
            "Refusing to reclaim '{target_name}': latest identity used tool '{}' but current session is '{}'",
            meta.tool,
            current_tool
        );
    }

    let current_dir = ctx.cwd.to_string_lossy();
    if !meta.directory.is_empty() && !same_path(&meta.directory, &current_dir) {
        bail!(
            "Refusing to reclaim '{target_name}': latest identity used directory '{}' but current session is '{}'",
            meta.directory,
            current_dir
        );
    }

    Ok(())
}

fn same_path(left: &str, right: &str) -> bool {
    normalize_path_for_compare(left) == normalize_path_for_compare(right)
}

fn normalize_path_for_compare(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

/// Load rebind metadata from the live row first, then the latest stopped snapshot.
fn load_rebind_target_metadata(db: &HcomDb, name: &str) -> Result<RebindTargetMetadata> {
    if let Some(inst) = db.get_instance_full(name)? {
        return Ok(RebindTargetMetadata {
            tool: inst.tool,
            directory: inst.directory,
            last_event_id: inst.last_event_id,
            session_id: inst.session_id.unwrap_or_default(),
        });
    }

    let mut stmt = db.conn().prepare(
        "SELECT data FROM events WHERE type='life' AND instance=? ORDER BY id DESC LIMIT 10",
    )?;

    let rows: Vec<String> = stmt
        .query_map(rusqlite::params![name], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();

    for data_str in &rows {
        if let Ok(data) = serde_json::from_str::<serde_json::Value>(data_str)
            && data.get("action").and_then(|v| v.as_str()) == Some("stopped")
            && let Some(snapshot) = data.get("snapshot")
        {
            return Ok(RebindTargetMetadata {
                tool: snapshot
                    .get("tool")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                directory: snapshot
                    .get("directory")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                last_event_id: snapshot
                    .get("last_event_id")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
                session_id: snapshot
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }

    bail!("No rebind metadata found for '{}'", name)
}

/// Resolve the Claude session id visible to a CLI invocation.
///
/// Two sources, in order:
/// - `HCOM_CLAUDE_UNIX_SESSION_ID`: hcom's own SessionStart hook appends this
///   export to `CLAUDE_ENV_FILE`, which Claude runs before each Bash command.
/// - `CLAUDE_CODE_SESSION_ID`: Claude sets this directly in every Bash and
///   PowerShell subprocess, and it matches the `session_id` hooks receive.
///
/// The env-file round trip is the fragile one: it needs `CLAUDE_ENV_FILE` to
/// exist and our SessionStart to have run in this session generation. Without
/// the second source, a session that misses it cannot be recognized on a repeat
/// `hcom start`, which then mints a SECOND identity — the first stays bound to
/// nothing and later reports as launch_failed. Both values are set by Claude
/// for the session running this command, so either one binds identity.
fn resolve_claude_session_id(env: &HashMap<String, String>) -> Option<String> {
    ["HCOM_CLAUDE_UNIX_SESSION_ID", "CLAUDE_CODE_SESSION_ID"]
        .into_iter()
        .find_map(|key| env.get(key).filter(|value| !value.is_empty()).cloned())
}

/// Live local Claude instances in this directory that no session id points at.
///
/// These are the plausible earlier identities of a session that exposes no id
/// of its own — the only useful thing to say when hcom cannot recognize it.
fn unbound_claude_candidates(db: &HcomDb, ctx: &HcomContext, exclude: &str) -> Vec<String> {
    let cwd = ctx.cwd.to_string_lossy();
    let mut rows: Vec<InstanceRow> = db
        .iter_instances_full()
        .unwrap_or_default()
        .into_iter()
        .filter(|row| {
            row.tool == "claude"
                && row.status != "stopped"
                && row.name != exclude
                && row.directory == cwd
                && row.session_id.is_none()
                && row.parent_name.is_none()
                && !crate::instances::is_remote_instance(row)
        })
        .collect();
    rows.sort_by(|a, b| b.created_at.total_cmp(&a.created_at));
    rows.truncate(4);
    rows.into_iter().map(|row| row.name).collect()
}

/// Path C: Bare start — detect tool or create adhoc instance.
fn start_bare(
    db: &HcomDb,
    hcom_dir: &std::path::Path,
    ctx: &HcomContext,
    explicit_name: Option<&str>,
) -> Result<i32> {
    let explicit_name = explicit_name
        .map(|name| identity::resolve_display_name(db, name).unwrap_or_else(|| name.to_string()));
    let explicit_name = explicit_name.as_deref();

    // Skip vanilla detection if --name is provided with an existing instance
    let has_valid_identity = explicit_name
        .and_then(|n| db.get_instance_full(n).ok().flatten())
        .is_some();

    // Vanilla tool detection: auto-install hooks for unmanaged AI tools.
    // Identity is already canonical on HcomContext, so route every released
    // hook-bearing integration through the typed Tool hook adapter. This keeps
    // bare `hcom start` aligned with `hcom hooks add` as integrations evolve.
    if !has_valid_identity && ctx.detect_vanilla_tool().is_some() {
        let vanilla_tool = ctx.tool;
        if !vanilla_tool.hooks().is_empty() && !vanilla_tool.verify_hooks_installed(false) {
            println!("Installing {} hooks...", vanilla_tool.as_str());
            let include_perms = crate::config::load_config_snapshot().core.auto_approve;
            match vanilla_tool.try_setup_hooks(include_perms) {
                Ok(()) => {
                    println!(
                        "\nRestart {} to enable automatic message delivery.",
                        vanilla_tool.spec().label
                    );
                    println!("Then run: hcom start");
                }
                Err(error) if error.is_empty() => {
                    eprintln!(
                        "Failed to install hooks. Run: hcom hooks add {}",
                        vanilla_tool.as_str()
                    );
                }
                Err(error) => {
                    eprintln!(
                        "Failed to install {} hooks: {error}\nRun: hcom hooks add {}",
                        vanilla_tool.as_str(),
                        vanilla_tool.as_str()
                    );
                }
            }
            return Ok(1);
        }

        // Gemini: ensure hooksConfig.enabled is set (self-heal for v0.26.0+)
        if vanilla_tool == crate::tool::Tool::Gemini {
            let _ = crate::hooks::gemini::ensure_hooks_enabled();
        }
    }

    let tool = ctx.tool.as_str();
    let claude_session_id = (ctx.tool == crate::tool::Tool::Claude)
        .then(|| resolve_claude_session_id(&ctx.raw_env))
        .flatten();

    if explicit_name.is_none()
        && let Some(ref session_id) = claude_session_id
        && let Some(bound_name) = db.get_session_binding(session_id)?
    {
        // Only hcom writes session bindings, so a row keyed by this session's
        // own id is trusted identity evidence. Heal bindings created by older
        // versions before returning the existing row.
        db.mark_claude_session_validated(session_id, &bound_name)?;
        println!("hcom already started for {bound_name}");
        return Ok(0);
    }

    // Resolve or generate name
    let name = if let Some(n) = explicit_name {
        n.to_string()
    } else {
        instance_names::generate_unique_name(db)?
    };

    // Remote instances are relay mirrors. Starting them remotely is intentionally
    // unsupported because the useful remote lifecycle operations are launch/resume/kill.
    if let Ok(Some(ref existing)) = db.get_instance_full(&name)
        && crate::instances::is_remote_instance(existing)
    {
        bail!("Remote start is not supported for '{name}'. Start it on the owning device instead.");
    }

    // Check if already exists and active (only for explicit names —
    // generate_unique_name creates a placeholder row we must skip past)
    if explicit_name.is_some()
        && let Ok(Some(existing)) = db.get_instance_full(&name)
        && existing.status != "stopped"
    {
        println!("hcom already started for {}", name);
        return Ok(0);
    }

    instance_binding::initialize_instance_in_position_file(
        db,
        &name,
        claude_session_id.as_deref(),
        None, // parent_session_id
        None, // parent_name
        None, // agent_id
        None, // transcript_path
        Some(tool),
        false, // background
        None,  // tag
        None,  // wait_timeout
        None,  // subagent_timeout
        None,  // hints
        None,  // cwd_override
    );

    if let Some(ref session_id) = claude_session_id {
        db.set_session_binding(session_id, &name)?;
        db.mark_claude_session_validated(session_id, &name)?;
    }

    // Bind process if we have a process_id
    if let Some(ref process_id) = ctx.process_id
        && let Err(e) = db.set_process_binding(process_id, "", &name)
    {
        eprintln!("[hcom] warn: set_process_binding failed for {name}: {e}");
    }

    // Claude builds old enough to expose neither session id leave nothing to
    // recognize this session by, so a later `hcom start` here mints another
    // identity. Say what was just created and name the way back instead of
    // letting the duplicate appear silently.
    if explicit_name.is_none()
        && ctx.tool == crate::tool::Tool::Claude
        && claude_session_id.is_none()
    {
        let candidates = unbound_claude_candidates(db, ctx, &name);
        eprintln!(
            "[hcom] warn: this Claude session exposes no session id, so it was registered \
             as a new identity '{name}'. If it already had one{}, reclaim it with \
             `hcom start --as <name>` and drop this one with `hcom kill {name}`.",
            if candidates.is_empty() {
                String::new()
            } else {
                format!(" (unbound here: {})", candidates.join(", "))
            }
        );
    }

    // Print bootstrap
    let hcom_config = HcomConfig::load(None).unwrap_or_else(|e| {
        eprintln!("[hcom] warn: config load failed, using defaults: {e}");
        let mut c = HcomConfig::default();
        c.normalize();
        c
    });

    let bootstrap_text = bootstrap::get_bootstrap(
        db,
        hcom_dir,
        &name,
        tool,
        false,
        ctx.is_launched,
        &ctx.notes,
        &hcom_config.tag,
        relay::is_relay_enabled(&hcom_config),
        None,
    );

    println!("[hcom:{}]", name);
    println!("{}", bootstrap_text);
    // Repeated deliberately: the header above sits on top of a long bootstrap, so
    // `hcom start | tail -n` shows none of it. A caller that cannot see its own
    // name re-runs start, which is one way duplicate identities appear.
    println!("[hcom:{}]", name);

    // Log
    db.log_event(
        "life",
        &name,
        &json!({
            "action": "started",
            "tool": tool,
            "name": name,
        }),
    )
    .ok();

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use rusqlite::params;
    use serde_json::json;
    use serial_test::serial;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn make_ctx(tool_env: &[(&str, &str)], cwd: &str) -> HcomContext {
        let mut env: HashMap<String, String> = std::env::vars().collect();
        for (k, v) in tool_env {
            env.insert((*k).to_string(), (*v).to_string());
        }
        HcomContext::from_env(&env, PathBuf::from(cwd))
    }

    /// Claude context carrying exactly one session-id source, so an ambient
    /// value from the shell running the tests cannot decide the outcome.
    fn make_claude_ctx(session: Option<(&str, &str)>, cwd: &str) -> HcomContext {
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.remove("HCOM_CLAUDE_UNIX_SESSION_ID");
        env.remove("CLAUDE_CODE_SESSION_ID");
        env.insert("CLAUDECODE".to_string(), "1".to_string());
        if let Some((key, value)) = session {
            env.insert(key.to_string(), value.to_string());
        }
        HcomContext::from_env(&env, PathBuf::from(cwd))
    }

    fn log_stopped_snapshot(
        db: &HcomDb,
        name: &str,
        tool: &str,
        directory: &str,
        session_id: &str,
        last_event_id: i64,
    ) {
        db.log_event(
            "life",
            name,
            &json!({
                "action": "stopped",
                "snapshot": {
                    "tool": tool,
                    "directory": directory,
                    "session_id": session_id,
                    "last_event_id": last_event_id
                }
            }),
        )
        .unwrap();
    }
    #[test]
    fn test_start_args_bare() {
        let args = StartArgs::try_parse_from(["start"]).unwrap();
        assert!(args.orphan.is_none());
        assert!(args.as_name.is_none());
    }

    fn insert_claim_target(
        db: &HcomDb,
        name: &str,
        pid: Option<i64>,
        status_time: i64,
        directory: &str,
    ) {
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, status, status_context, status_time, created_at,
                  last_seen, directory, background, pid, last_event_id)
                 VALUES (?1, NULL, 'claude', 'active', 'running', ?2, 1, 0, ?3, 1, ?4, 7)",
                rusqlite::params![name, status_time, directory, pid],
            )
            .unwrap();
    }

    fn spawn_live_pid() -> (std::process::Child, i64) {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep for live-pid test");
        let pid = child.id() as i64;
        (child, pid)
    }

    fn tombstone_count(db: &HcomDb, name: &str) -> i64 {
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM events
                 WHERE type = 'life' AND instance = ? AND data LIKE '%\"action\":\"stopped\"%'",
                rusqlite::params![name],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    #[serial]
    fn test_d3_live_pid_stale_row_refuses_claim() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let (mut child, pid) = spawn_live_pid();
        let cwd = "/tmp/nrm053-d3-live";
        std::fs::create_dir_all(cwd).unwrap();
        insert_claim_target(&db, "nova", Some(pid), crate::shared::time::now_epoch_i64() - 1000, cwd);

        let ctx = make_claude_ctx(
            Some(("CLAUDE_CODE_SESSION_ID", "sess-claim")),
            cwd,
        );
        let result = start_rebind_opts(&db, "nova", &ctx, None, ClaimOptions { adopt_unknown: true, force: false }).unwrap();

        assert_eq!(result, 1, "a stale row with a live pid must refuse the claim");
        assert!(
            db.get_instance_full("nova").unwrap().is_some(),
            "the live row must survive the refused claim"
        );
        assert_eq!(
            tombstone_count(&db, "nova"),
            0,
            "a refused claim must publish no tombstone"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    #[serial]
    fn test_d3_dead_pid_stale_row_claim_succeeds() {
        let (_dir, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let mut dead = std::process::Command::new("true").spawn().unwrap();
        let pid = dead.id() as i64;
        dead.wait().unwrap();
        let cwd = "/tmp/nrm053-d3-dead";
        std::fs::create_dir_all(cwd).unwrap();
        insert_claim_target(&db, "nova", Some(pid), crate::shared::time::now_epoch_i64() - 1000, cwd);

        let ctx = make_claude_ctx(
            Some(("CLAUDE_CODE_SESSION_ID", "sess-claim")),
            cwd,
        );
        let result = start_rebind_opts(&db, "nova", &ctx, None, ClaimOptions { adopt_unknown: true, force: false }).unwrap();

        assert_eq!(result, 0, "a stale row with a dead pid is a valid succession");
        let row = db.get_instance_full("nova").unwrap().unwrap();
        assert_eq!(row.session_id.as_deref(), Some("sess-claim"));
        assert_eq!(
            db.get_session_binding("sess-claim").unwrap().as_deref(),
            Some("nova")
        );
        assert_eq!(
            tombstone_count(&db, "nova"),
            1,
            "the predecessor stop must publish exactly one tombstone"
        );
        assert_eq!(
            row.last_event_id, 7,
            "the succession must carry the predecessor's cursor"
        );
    }

    #[test]
    #[serial]
    fn test_d3_active_row_refuses_claim() {
        let (_dir, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let cwd = "/tmp/nrm053-d3-active";
        std::fs::create_dir_all(cwd).unwrap();
        insert_claim_target(
            &db,
            "nova",
            None,
            crate::shared::time::now_epoch_i64(),
            cwd,
        );

        let ctx = make_claude_ctx(
            Some(("CLAUDE_CODE_SESSION_ID", "sess-claim")),
            cwd,
        );
        let result = start_rebind_opts(&db, "nova", &ctx, None, ClaimOptions { adopt_unknown: true, force: false }).unwrap();

        assert_eq!(result, 1, "a fresh active row must refuse the claim");
        assert!(db.get_instance_full("nova").unwrap().is_some());
    }

    #[test]
    #[serial]
    fn test_d6_unknown_name_claim_refused_without_adopt_unknown() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let cwd = "/tmp/nrm053-d6";
        std::fs::create_dir_all(cwd).unwrap();
        let ctx = make_claude_ctx(
            Some(("CLAUDE_CODE_SESSION_ID", "sess-claim")),
            cwd,
        );

        let refused =
            start_rebind_opts(&db, "novaname", &ctx, None, ClaimOptions::default()).unwrap();
        assert_eq!(refused, 1, "an unknown name must be refused without --adopt-unknown");
        assert!(db.get_instance_full("novaname").unwrap().is_none());

        let adopted = start_rebind_opts(
            &db,
            "novaname",
            &ctx,
            None,
            ClaimOptions { adopt_unknown: true, force: false },
        )
        .unwrap();
        assert_eq!(adopted, 0, "--adopt-unknown allows the provisional claim");
        assert!(db.get_instance_full("novaname").unwrap().is_some());
    }

    #[test]
    #[serial]
    fn test_d4b_force_takes_over_live_row_and_logs_displaced_session() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let (mut child, pid) = spawn_live_pid();
        let cwd = "/tmp/nrm053-d4b";
        std::fs::create_dir_all(cwd).unwrap();
        insert_claim_target(
            &db,
            "nova",
            Some(pid),
            crate::shared::time::now_epoch_i64() - 1000,
            cwd,
        );
        db.conn()
            .execute(
                "UPDATE instances SET session_id = 'sid-victim' WHERE name = 'nova'",
                [],
            )
            .unwrap();

        let ctx = make_claude_ctx(
            Some(("CLAUDE_CODE_SESSION_ID", "sess-claim")),
            cwd,
        );

        let refused = start_rebind_opts(&db, "nova", &ctx, None, ClaimOptions::default()).unwrap();
        assert_eq!(refused, 1, "live takeover must require --force");
        assert!(db.get_instance_full("nova").unwrap().is_some());

        let forced = start_rebind_opts(
            &db,
            "nova",
            &ctx,
            None,
            ClaimOptions { adopt_unknown: false, force: true },
        )
        .unwrap();
        assert_eq!(forced, 0, "--force takes over the live identity");
        let row = db.get_instance_full("nova").unwrap().unwrap();
        assert_eq!(row.session_id.as_deref(), Some("sess-claim"));

        let force_events: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events
                 WHERE type = 'life' AND instance = 'nova'
                   AND data LIKE '%\"action\":\"force_claim\"%'
                   AND data LIKE '%sid-victim%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            force_events, 1,
            "the takeover must log a force_claim event carrying the displaced session ref"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn test_start_args_force_and_adopt_unknown() {
        let args = StartArgs::try_parse_from([
            "start",
            "--as",
            "luna",
            "--force",
            "--adopt-unknown",
        ])
        .unwrap();
        assert_eq!(args.as_name.as_deref(), Some("luna"));
        assert!(args.force);
        assert!(args.adopt_unknown);
    }

    #[test]
    #[serial]
    fn test_succession_event_logged_on_tombstone_reclaim() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let cwd = "/tmp/nrm053-gap1";
        std::fs::create_dir_all(cwd).unwrap();

        log_stopped_snapshot(&db, "nova", "claude", cwd, "sid-old", 77);

        let ctx = make_claude_ctx(
            Some(("CLAUDE_CODE_SESSION_ID", "sess-claim")),
            cwd,
        );
        let result = start_rebind_opts(&db, "nova", &ctx, None, ClaimOptions::default()).unwrap();
        assert_eq!(result, 0);

        let events: Vec<String> = {
            let mut stmt = db
                .conn()
                .prepare(
                    "SELECT data FROM events WHERE type = 'life' AND instance = 'nova' ORDER BY id",
                )
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };
        let succession = events
            .iter()
            .filter(|d| d.contains("\"action\":\"succession\""))
            .count();
        assert_eq!(
            succession, 1,
            "the ordinary succession must be logged as exactly one transfer event: {events:?}"
        );
        assert!(
            !events.iter().any(|d| d.contains("force_claim")),
            "a consented succession must never be logged as force_claim"
        );
        let ev = events
            .iter()
            .find(|d| d.contains("\"action\":\"succession\""))
            .unwrap();
        assert!(
            ev.contains("\"displaced_session_id\":\"sid-old\""),
            "the transfer must name the predecessor session ref: {ev}"
        );
        assert!(
            ev.contains("\"claimer_session_id\":\"sess-claim\""),
            "the transfer must name the claimer session ref: {ev}"
        );
        assert!(
            ev.contains("\"displaced_name\":\"nova\""),
            "the transfer must name the label: {ev}"
        );
    }

    #[test]
    #[serial]
    fn test_succession_event_logged_on_dead_row_reclaim() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let cwd = "/tmp/nrm053-gap1b";
        std::fs::create_dir_all(cwd).unwrap();
        insert_claim_target(
            &db,
            "nova",
            None,
            crate::shared::time::now_epoch_i64() - 1000,
            cwd,
        );
        db.conn()
            .execute(
                "UPDATE instances SET session_id = 'sid-dead' WHERE name = 'nova'",
                [],
            )
            .unwrap();

        let ctx = make_claude_ctx(
            Some(("CLAUDE_CODE_SESSION_ID", "sess-claim")),
            cwd,
        );
        let result = start_rebind_opts(&db, "nova", &ctx, None, ClaimOptions::default()).unwrap();
        assert_eq!(result, 0);

        let ev: String = db
            .conn()
            .query_row(
                "SELECT data FROM events
                 WHERE type = 'life' AND instance = 'nova'
                   AND data LIKE '%\"action\":\"succession\"%'
                 ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(ev.contains("\"displaced_session_id\":\"sid-dead\""), "{ev}");
        assert!(ev.contains("\"claimer_session_id\":\"sess-claim\""), "{ev}");
    }

    #[test]
    #[serial]
    fn test_no_succession_event_for_own_session_or_adopted_name() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let cwd = "/tmp/nrm053-gap1c";
        std::fs::create_dir_all(cwd).unwrap();

        let ctx = make_claude_ctx(
            Some(("CLAUDE_CODE_SESSION_ID", "sess-claim")),
            cwd,
        );
        // adopted fresh name: no predecessor, no transfer
        start_rebind_opts(
            &db,
            "brandnew",
            &ctx,
            None,
            ClaimOptions { adopt_unknown: true, force: false },
        )
        .unwrap();
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events
                 WHERE type = 'life' AND instance = 'brandnew'
                   AND data LIKE '%\"action\":\"succession\"%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "adopting a never-used name is not a succession");
    }

    #[test]
    #[serial]
    fn test_d3_own_session_live_row_updates_in_place() {
        let (_dir, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let cwd = "/tmp/nrm053-d3-own";
        std::fs::create_dir_all(cwd).unwrap();
        insert_claim_target(
            &db,
            "nova",
            None,
            crate::shared::time::now_epoch_i64(),
            cwd,
        );
        db.conn()
            .execute(
                "UPDATE instances SET session_id = 'sess-claim' WHERE name = 'nova'",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO session_bindings (session_id, instance_name, created_at)
                 VALUES ('sess-claim', 'nova', 1)",
                [],
            )
            .unwrap();

        let ctx = make_claude_ctx(
            Some(("CLAUDE_CODE_SESSION_ID", "sess-claim")),
            cwd,
        );
        let result = start_rebind_opts(&db, "nova", &ctx, None, ClaimOptions { adopt_unknown: true, force: false }).unwrap();

        assert_eq!(result, 0, "the compaction re-run reclaims its own live row");
        let row = db.get_instance_full("nova").unwrap().unwrap();
        assert_eq!(row.status, "active", "the own row's status must not reset");
        assert_eq!(
            row.created_at, 1.0,
            "the own row's incarnation must be preserved"
        );
        assert_eq!(tombstone_count(&db, "nova"), 0);
    }

    #[test]
    #[serial]
    fn test_d5_start_as_refuses_reserved_and_invalid_names() {
        let (_dir, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let ctx = make_claude_ctx(
            Some(("CLAUDE_CODE_SESSION_ID", "sess-claim")),
            "/tmp/nrm053-d5",
        );

        for bad in ["bigboss", "hcom", "Bad Name", "has-dash"] {
            let result = start_rebind_opts(&db, bad, &ctx, None, ClaimOptions::default()).unwrap();
            assert_eq!(result, 1, "'{bad}' must be refused by validate_claim_name");
        }
        assert!(
            db.iter_instances_full().unwrap().is_empty(),
            "refused claims must create no rows"
        );
    }

    #[test]
    fn test_start_args_orphan() {
        let args = StartArgs::try_parse_from(["start", "--orphan", "1234"]).unwrap();
        assert_eq!(args.orphan, Some("1234".to_string()));
        assert!(args.as_name.is_none());
    }

    #[test]
    fn test_start_args_rebind() {
        let args = StartArgs::try_parse_from(["start", "--as", "luna"]).unwrap();
        assert!(args.orphan.is_none());
        assert_eq!(args.as_name, Some("luna".to_string()));
    }

    #[test]
    fn test_start_args_bare_as_errors() {
        let err = StartArgs::try_parse_from(["start", "--as"]);
        assert!(err.is_err());
    }

    #[test]
    fn test_start_args_bare_orphan_errors() {
        let err = StartArgs::try_parse_from(["start", "--orphan"]);
        assert!(err.is_err());
    }

    #[test]
    fn test_start_args_unknown_flag_errors() {
        let err = StartArgs::try_parse_from(["start", "--bogus"]);
        assert!(err.is_err());
    }

    #[test]
    #[serial]
    fn test_start_rejects_remote_instances() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, origin_device_id, created_at) VALUES (?1, ?2, ?3)",
                params![
                    "luna:ABCD",
                    "remote-device",
                    crate::shared::time::now_epoch_f64()
                ],
            )
            .unwrap();

        let flags = crate::router::GlobalFlags {
            name: Some("luna:ABCD".to_string()),
            go: false,
        };
        let err = run(&["start".to_string()], &flags).unwrap_err();
        assert!(
            err.to_string().contains("Remote start is not supported"),
            "unexpected error: {err}"
        );
    }

    #[test]
    #[serial]
    fn test_vanilla_claude_start_immediately_binds_exported_session() {
        struct RestoreEnv(Option<std::ffi::OsString>);
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                unsafe {
                    match self.0.take() {
                        Some(value) => std::env::set_var("HCOM_CLAUDE_UNIX_SESSION_ID", value),
                        None => std::env::remove_var("HCOM_CLAUDE_UNIX_SESSION_ID"),
                    }
                }
            }
        }

        let (_dir, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        assert!(crate::hooks::claude::setup_claude_hooks(false));

        let _restore = RestoreEnv(std::env::var_os("HCOM_CLAUDE_UNIX_SESSION_ID"));
        unsafe {
            std::env::set_var("HCOM_CLAUDE_UNIX_SESSION_ID", "sess-vanilla");
        }
        let ctx = make_ctx(&[("CLAUDECODE", "1")], "/tmp/project");

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        let name = db
            .get_session_binding("sess-vanilla")
            .unwrap()
            .expect("bare vanilla start must bind immediately");
        let row = db.get_instance_full(&name).unwrap().unwrap();
        assert_eq!(row.session_id.as_deref(), Some("sess-vanilla"));
        assert_eq!(row.tool, "claude");
        assert_eq!(
            db.get_validated_claude_session_owner("sess-vanilla")
                .unwrap()
                .as_deref(),
            Some(name.as_str()),
            "CLI-created Claude bindings must be immediately trusted by hooks"
        );

        let transcript = hcom_dir.join("vanilla.jsonl");
        std::fs::write(&transcript, "{\"sessionId\":\"sess-vanilla\"}\n").unwrap();
        let mut hook_ctx = ctx.clone();
        hook_ctx.process_id = None;
        let (resolved, _, _) = crate::hooks::common::init_hook_context(
            &db,
            &hook_ctx,
            "sess-vanilla",
            transcript.to_str().unwrap(),
        );
        assert_eq!(resolved.as_deref(), Some(name.as_str()));

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        assert_eq!(
            db.get_session_binding("sess-vanilla").unwrap().as_deref(),
            Some(name.as_str()),
            "repeated bare start must retain the existing vanilla identity"
        );
    }

    #[test]
    fn test_resolve_claude_session_id_sources() {
        let env = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect()
        };

        assert_eq!(
            resolve_claude_session_id(&env(&[
                ("HCOM_CLAUDE_UNIX_SESSION_ID", "hook-sess"),
                ("CLAUDE_CODE_SESSION_ID", "claude-sess"),
            ])),
            Some("hook-sess".to_string()),
            "our own export stays the first source"
        );
        assert_eq!(
            resolve_claude_session_id(&env(&[("CLAUDE_CODE_SESSION_ID", "claude-sess")])),
            Some("claude-sess".to_string()),
            "Claude's own Bash env carries identity when the env file cannot"
        );
        assert_eq!(
            resolve_claude_session_id(&env(&[
                ("HCOM_CLAUDE_UNIX_SESSION_ID", ""),
                ("CLAUDE_CODE_SESSION_ID", "claude-sess"),
            ])),
            Some("claude-sess".to_string()),
            "an empty export is not identity"
        );
        assert_eq!(resolve_claude_session_id(&env(&[])), None);
    }

    #[test]
    #[serial]
    fn test_vanilla_claude_start_reuses_claude_code_session_id() {
        let (_dir, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        assert!(crate::hooks::claude::setup_claude_hooks(false));

        // No CLAUDE_ENV_FILE round trip, so HCOM_CLAUDE_UNIX_SESSION_ID never
        // arrives — the case that used to mint a second identity per start.
        let ctx = make_claude_ctx(
            Some(("CLAUDE_CODE_SESSION_ID", "sess-claude-env")),
            "/tmp/project",
        );

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        let name = db
            .get_session_binding("sess-claude-env")
            .unwrap()
            .expect("CLAUDE_CODE_SESSION_ID must bind identity");
        assert_eq!(
            db.get_validated_claude_session_owner("sess-claude-env")
                .unwrap()
                .as_deref(),
            Some(name.as_str()),
            "hooks must trust the binding the CLI just created"
        );

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        assert_eq!(
            db.get_session_binding("sess-claude-env")
                .unwrap()
                .as_deref(),
            Some(name.as_str()),
            "repeat start must return the first identity, not mint a second"
        );
        let claude_rows: Vec<String> = db
            .iter_instances_full()
            .unwrap()
            .into_iter()
            .filter(|row| row.tool == "claude")
            .map(|row| row.name)
            .collect();
        assert_eq!(claude_rows, vec![name], "exactly one identity per session");
    }

    #[test]
    #[serial]
    fn test_vanilla_claude_rebind_binds_session_and_drops_old_identity() {
        let (_dir, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        assert!(crate::hooks::claude::setup_claude_hooks(false));

        let ctx = make_claude_ctx(
            Some(("CLAUDE_CODE_SESSION_ID", "sess-rebind")),
            "/tmp/project",
        );
        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        let first = db.get_session_binding("sess-rebind").unwrap().unwrap();

        assert_eq!(start_rebind_opts(&db, "nova", &ctx, None, ClaimOptions { adopt_unknown: true, force: false }).unwrap(), 0);
        assert_eq!(
            db.get_session_binding("sess-rebind").unwrap().as_deref(),
            Some("nova"),
            "a reclaimed name must own the session that reclaimed it"
        );
        assert!(
            db.get_instance_full(&first).unwrap().is_none(),
            "the identity being replaced must not be left behind"
        );
        assert_eq!(
            db.get_validated_claude_session_owner("sess-rebind")
                .unwrap()
                .as_deref(),
            Some("nova"),
            "hooks must resolve the reclaimed name, not reject the session"
        );

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        assert_eq!(
            db.get_session_binding("sess-rebind").unwrap().as_deref(),
            Some("nova"),
            "a start after the rebind returns the reclaimed identity"
        );
    }

    #[test]
    #[serial]
    fn test_unidentifiable_claude_start_lists_unbound_candidates() {
        let (_dir, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        assert!(crate::hooks::claude::setup_claude_hooks(false));

        let cwd = std::env::current_dir().unwrap();
        let ctx = make_claude_ctx(None, cwd.to_str().unwrap());

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        let first = db
            .iter_instances_full()
            .unwrap()
            .into_iter()
            .find(|row| row.tool == "claude")
            .expect("first start creates an identity")
            .name;
        assert!(
            db.get_instance_full(&first)
                .unwrap()
                .unwrap()
                .session_id
                .is_none(),
            "a session with no id leaves the row unbound"
        );

        // Without any session id hcom still cannot recognize the session, so the
        // second start mints another identity — the warning names this one back.
        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        let second = db
            .iter_instances_full()
            .unwrap()
            .into_iter()
            .find(|row| row.tool == "claude" && row.name != first)
            .expect("second start mints a second identity")
            .name;
        assert_eq!(
            unbound_claude_candidates(&db, &ctx, &second),
            vec![first],
            "the earlier unbound identity is the reclaim candidate"
        );
    }

    #[test]
    #[serial]
    fn test_root_rebind_preserves_child_hierarchy_and_actor_state() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();

        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, status, status_time, last_seen, created_at)
                 VALUES ('nova', 'sess-1', 'claude', 'active', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, parent_session_id, parent_name, agent_id, tool, status,
                  status_time, last_seen, created_at)
                 VALUES ('nova_task_1', 'sess-1', 'nova', 'agent-1', 'claude',
                         'active', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, parent_session_id, parent_name, agent_id, tool, status,
                  status_time, last_seen, created_at)
                 VALUES ('nova_task_2', 'sess-1', 'nova_task_1', 'agent-2', 'claude',
                         'active', 0, 0, 0)",
                [],
            )
            .unwrap();

        let token = db
            .issue_claude_actor_capability("sess-1", "tool-root", None, "nova")
            .unwrap();

        let links = snapshot_child_links(&db, Some("sess-1")).unwrap();
        assert_eq!(links.len(), 2);
        db.delete_instance("nova").unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, status, status_time, last_seen, created_at)
                 VALUES ('sol', 'sess-1', 'claude', 'active', 0, 0, 0)",
                [],
            )
            .unwrap();

        db.with_immediate_transaction(|tx| {
            restore_child_links_after_root_rebind(tx, &links, "sess-1", "nova", "sol")
        })
        .unwrap();
        db.rebind_claude_root_actor_state("sess-1", "nova", "sol")
            .unwrap();

        let direct = db.get_instance_full("nova_task_1").unwrap().unwrap();
        assert_eq!(direct.parent_session_id.as_deref(), Some("sess-1"));
        assert_eq!(direct.parent_name.as_deref(), Some("sol"));
        let nested = db.get_instance_full("nova_task_2").unwrap().unwrap();
        assert_eq!(nested.parent_session_id.as_deref(), Some("sess-1"));
        assert_eq!(nested.parent_name.as_deref(), Some("nova_task_1"));
        assert_eq!(
            db.resolve_claude_actor_capability(&token, "sess-1")
                .unwrap(),
            Some("sol".to_string())
        );
    }

    #[test]
    #[serial]
    fn test_same_name_root_rebind_restores_child_session_links() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, directory, status, status_time, last_seen, created_at)
                 VALUES ('nova', 'sess-1', 'claude', '/tmp/project', 'active', 0, 0, 1)",
                [],
            )
            .unwrap();
        db.set_session_binding("sess-1", "nova").unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, parent_session_id, parent_name, agent_id, tool, status,
                  status_time, last_seen, created_at)
                 VALUES ('nova_task_1', 'sess-1', 'nova', 'agent-1', 'claude',
                         'active', 0, 0, 2)",
                [],
            )
            .unwrap();
        let token = db
            .issue_claude_actor_capability("sess-1", "tool-child", Some("agent-1"), "nova_task_1")
            .unwrap();

        let ctx = make_ctx(&[("CLAUDECODE", "1")], "/tmp/project");
        assert_eq!(
            start_rebind_opts(&db, "nova", &ctx, Some("nova"), ClaimOptions::default()).unwrap(),
            0
        );

        let child = db.get_instance_full("nova_task_1").unwrap().unwrap();
        assert_eq!(child.parent_session_id.as_deref(), Some("sess-1"));
        assert_eq!(child.parent_name.as_deref(), Some("nova"));
        assert_eq!(
            db.resolve_claude_actor_capability(&token, "sess-1")
                .unwrap(),
            Some("nova_task_1".to_string())
        );
    }

    #[test]
    #[serial]
    fn test_start_rebind_rejects_cross_tool_stopped_snapshot_hijack() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();

        log_stopped_snapshot(
            &db,
            "fama",
            "codex",
            "/tmp/dasha-code/.worktrees/layer1-basic-conversation-fixes",
            "sid-fama",
            42,
        );

        let ctx = make_ctx(
            &[("CLAUDECODE", "1")],
            "/tmp/hcom-gan-harness/.worktrees/bench-infra",
        );

        let err = start_rebind_opts(&db, "fama", &ctx, None, ClaimOptions::default()).unwrap_err();
        assert!(
            err.to_string().contains("Refusing to reclaim 'fama'"),
            "unexpected error: {err}"
        );

        assert!(db.get_instance_full("fama").unwrap().is_none());
        assert_eq!(db.get_session_binding("sid-fama").unwrap(), None);
    }

    #[test]
    #[serial]
    fn test_start_rebind_allows_matching_stopped_snapshot_reclaim() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();

        log_stopped_snapshot(
            &db,
            "nova",
            "claude",
            "/tmp/dasha-code/.worktrees/layer1-basic-conversation-fixes",
            "sid-nova",
            77,
        );

        let ctx = make_ctx(
            &[("CLAUDECODE", "1")],
            "/tmp/dasha-code/.worktrees/layer1-basic-conversation-fixes",
        );

        let exit_code = start_rebind_opts(&db, "nova", &ctx, None, ClaimOptions::default()).unwrap();
        assert_eq!(exit_code, 0);

        let inst = db.get_instance_full("nova").unwrap().unwrap();
        assert_eq!(inst.tool, "claude");
        assert_eq!(
            inst.directory,
            "/tmp/dasha-code/.worktrees/layer1-basic-conversation-fixes"
        );
        assert_eq!(inst.last_event_id, 77);
    }

    #[test]
    #[serial]
    fn test_start_rebind_rejects_cross_directory_stopped_snapshot_hijack() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();

        log_stopped_snapshot(
            &db,
            "mira",
            "claude",
            "/tmp/dasha-code/.worktrees/layer1-basic-conversation-fixes",
            "sid-mira",
            18,
        );

        let ctx = make_ctx(
            &[("CLAUDECODE", "1")],
            "/tmp/hcom-gan-harness/.worktrees/bench-infra",
        );

        let err = start_rebind_opts(&db, "mira", &ctx, None, ClaimOptions::default()).unwrap_err();
        assert!(
            err.to_string().contains("Refusing to reclaim 'mira'"),
            "unexpected error: {err}"
        );

        assert!(db.get_instance_full("mira").unwrap().is_none());
    }

    #[test]
    #[cfg(unix)]
    fn test_same_path_resolves_symlink_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let alias = dir.path().join("alias");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();

        assert!(same_path(
            real.to_string_lossy().as_ref(),
            alias.to_string_lossy().as_ref()
        ));
    }
}
