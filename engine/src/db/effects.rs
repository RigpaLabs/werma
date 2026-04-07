use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use crate::models::{Effect, EffectStatus, EffectType};

/// Exponential backoff delays in seconds: 5s, 30s, 120s, 300s, 600s.
const BACKOFF_SECS: &[i64] = &[5, 30, 120, 300, 600];

fn backoff_secs(attempt: i32) -> i64 {
    let idx = (attempt as usize)
        .saturating_sub(1)
        .min(BACKOFF_SECS.len() - 1);
    BACKOFF_SECS[idx]
}

/// Thin wrapper so we can turn a String into a boxed StdError for rusqlite.
#[derive(Debug)]
struct ConversionError(String);
impl std::fmt::Display for ConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ConversionError {}

fn parse_error(col: usize, msg: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        col,
        rusqlite::types::Type::Text,
        Box::new(ConversionError(msg)),
    )
}

fn effect_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Effect> {
    let effect_type_str: String = row.get(4)?;
    let effect_type: EffectType = effect_type_str
        .parse()
        .map_err(|e: anyhow::Error| parse_error(4, e.to_string()))?;
    let payload_str: String = row.get(5)?;
    let payload: serde_json::Value =
        serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Object(Default::default()));
    let blocking_int: i32 = row.get(6)?;
    let status_str: String = row.get(7)?;
    let status: EffectStatus = status_str
        .parse()
        .map_err(|e: anyhow::Error| parse_error(7, e.to_string()))?;

    Ok(Effect {
        id: row.get(0)?,
        dedup_key: row.get(1)?,
        task_id: row.get(2)?,
        issue_id: row.get(3)?,
        effect_type,
        payload,
        blocking: blocking_int != 0,
        status,
        attempts: row.get(8)?,
        max_attempts: row.get(9)?,
        created_at: row.get(10)?,
        next_retry_at: row.get(11)?,
        executed_at: row.get(12)?,
        error: row.get(13)?,
    })
}

impl super::Db {
    /// Insert a batch of effects, ignoring duplicates by dedup_key.
    ///
    /// Takes a `&Connection` so it can be called inside a `transaction()` closure.
    pub fn insert_effects_with_conn(conn: &Connection, effects: &[Effect]) -> Result<()> {
        for effect in effects {
            let payload = serde_json::to_string(&effect.payload)?;
            let blocking: i32 = if effect.blocking { 1 } else { 0 };
            conn.execute(
                "INSERT OR IGNORE INTO effects (
                    dedup_key, task_id, issue_id, effect_type, payload,
                    blocking, status, attempts, max_attempts, created_at,
                    next_retry_at, executed_at, error
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5,
                    ?6, ?7, ?8, ?9, ?10,
                    ?11, ?12, ?13
                )",
                params![
                    effect.dedup_key,
                    effect.task_id,
                    effect.issue_id,
                    effect.effect_type.to_string(),
                    payload,
                    blocking,
                    effect.status.to_string(),
                    effect.attempts,
                    effect.max_attempts,
                    effect.created_at,
                    effect.next_retry_at,
                    effect.executed_at,
                    effect.error,
                ],
            )
            .context("insert effect")?;
        }
        Ok(())
    }

    /// Convenience wrapper that takes &self — for use outside transactions.
    pub fn insert_effects(&self, effects: &[Effect]) -> Result<()> {
        Self::insert_effects_with_conn(&self.conn, effects)
    }

    /// Fetch pending and retryable effects up to `limit`, ordered by id ASC.
    ///
    /// Only returns effects whose next_retry_at is in the past (or NULL).
    pub fn pending_effects(&self, limit: i32) -> Result<Vec<Effect>> {
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let mut stmt = self.conn.prepare(
            "SELECT id, dedup_key, task_id, issue_id, effect_type, payload,
                    blocking, status, attempts, max_attempts, created_at,
                    next_retry_at, executed_at, error
             FROM effects
             WHERE status IN ('pending', 'failed')
               AND (next_retry_at IS NULL OR next_retry_at <= ?1)
             ORDER BY id ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![now, limit], effect_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Mark an effect as done, increment attempts, and record the execution timestamp.
    ///
    /// RIG-321: `attempts` is incremented on success too, so `attempts > 0` proves the
    /// effect was actually executed (not silently skipped).
    pub fn mark_effect_done(&self, id: i64) -> Result<()> {
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        self.conn.execute(
            "UPDATE effects SET status = 'done', attempts = attempts + 1, executed_at = ?1, error = NULL WHERE id = ?2",
            params![now, id],
        )?;
        Ok(())
    }

    /// Mark an effect as failed, increment attempts, apply exponential backoff.
    ///
    /// If attempts reaches max_attempts after this failure, status becomes 'dead'.
    pub fn mark_effect_failed(&self, id: i64, error: &str) -> Result<()> {
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

        // Fetch current attempts and max_attempts to decide final status.
        let (attempts, max_attempts): (i32, i32) = self.conn.query_row(
            "SELECT attempts, max_attempts FROM effects WHERE id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        let new_attempts = attempts + 1;
        let exhausted = new_attempts >= max_attempts;
        let new_status = if exhausted { "dead" } else { "failed" };

        let next_retry_at = if exhausted {
            None
        } else {
            let delay = backoff_secs(new_attempts);
            let ts = (chrono::Local::now() + chrono::Duration::seconds(delay))
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string();
            Some(ts)
        };

        self.conn.execute(
            "UPDATE effects SET status = ?1, attempts = ?2, error = ?3,
                              executed_at = ?4, next_retry_at = ?5
             WHERE id = ?6",
            params![new_status, new_attempts, error, now, next_retry_at, id],
        )?;
        Ok(())
    }

    /// Fetch all dead-lettered (permanently failed) effects, ordered by id ASC.
    pub fn dead_effects(&self) -> Result<Vec<Effect>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, dedup_key, task_id, issue_id, effect_type, payload,
                    blocking, status, attempts, max_attempts, created_at,
                    next_retry_at, executed_at, error
             FROM effects
             WHERE status = 'dead'
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], effect_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Fetch pending and failed effects (visible queue), ordered by id ASC.
    pub fn pending_and_failed_effects(&self) -> Result<Vec<Effect>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, dedup_key, task_id, issue_id, effect_type, payload,
                    blocking, status, attempts, max_attempts, created_at,
                    next_retry_at, executed_at, error
             FROM effects
             WHERE status IN ('pending', 'failed')
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], effect_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Reset a dead or failed effect back to pending for retry.
    ///
    /// Clears attempts, status, next_retry_at, executed_at, and error so it will be picked
    /// up on the next processor run.
    pub fn retry_effect(&self, id: i64) -> Result<bool> {
        let rows_changed = self.conn.execute(
            "UPDATE effects
             SET status = 'pending', attempts = 0, next_retry_at = NULL, error = NULL, executed_at = NULL
             WHERE id = ?1 AND status IN ('dead', 'failed')",
            params![id],
        )?;
        Ok(rows_changed > 0)
    }

    /// Fetch all effects for a given task_id, ordered by id ASC.
    pub fn effects_for_task(&self, task_id: &str) -> Result<Vec<Effect>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, dedup_key, task_id, issue_id, effect_type, payload,
                    blocking, status, attempts, max_attempts, created_at,
                    next_retry_at, executed_at, error
             FROM effects
             WHERE task_id = ?1
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![task_id], effect_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Returns true if all blocking effects for a task are resolved (done or dead).
    ///
    /// Both `done` and `dead` are treated as terminal states — a `dead` blocking effect
    /// (permanently failed after max retries) is resolved: the effect won't be retried,
    /// so the task should not be held back indefinitely. Without this, a dead effect would
    /// leave `linear_pushed=0` forever, causing the daemon to re-process the task every tick.
    ///
    /// A task with no blocking effects at all is considered "done" (returns true).
    pub fn blocking_effects_done(&self, task_id: &str) -> Result<bool> {
        let pending_blocking: i32 = self.conn.query_row(
            "SELECT COUNT(*) FROM effects
             WHERE task_id = ?1
               AND blocking = 1
               AND status NOT IN ('done', 'dead')",
            params![task_id],
            |row| row.get(0),
        )?;
        Ok(pending_blocking == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, make_test_task};
    use crate::models::*;

    fn make_effect(task_id: &str, dedup_key: &str, effect_type: EffectType) -> Effect {
        Effect {
            id: 0,
            dedup_key: dedup_key.to_string(),
            task_id: task_id.to_string(),
            issue_id: "FAT-42".to_string(),
            effect_type,
            payload: serde_json::json!({"target_status": "review"}),
            blocking: true,
            status: EffectStatus::Pending,
            attempts: 0,
            max_attempts: 5,
            created_at: "2026-03-26T10:00:00".to_string(),
            next_retry_at: None,
            executed_at: None,
            error: None,
        }
    }

    fn setup_db_with_task(task_id: &str) -> Db {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task(task_id);
        db.insert_task(&task).unwrap();
        db
    }

    #[test]
    fn insert_and_retrieve_effects() {
        let db = setup_db_with_task("t1");
        let e1 = make_effect("t1", "t1:move", EffectType::MoveIssue);
        let e2 = make_effect("t1", "t1:comment", EffectType::PostComment);
        db.insert_effects(&[e1, e2]).unwrap();

        let pending = db.pending_effects(100).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].effect_type, EffectType::MoveIssue);
        assert_eq!(pending[1].effect_type, EffectType::PostComment);
    }

    #[test]
    fn dedup_key_prevents_duplicates() {
        let db = setup_db_with_task("t2");
        let e1 = make_effect("t2", "t2:move", EffectType::MoveIssue);
        let e2 = make_effect("t2", "t2:move", EffectType::MoveIssue); // same dedup_key
        db.insert_effects(&[e1, e2]).unwrap();

        let count: i32 = db
            .conn
            .query_row("SELECT COUNT(*) FROM effects", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn mark_effect_done() {
        let db = setup_db_with_task("t3");
        let e = make_effect("t3", "t3:move", EffectType::MoveIssue);
        db.insert_effects(&[e]).unwrap();

        let effect = db.pending_effects(1).unwrap().into_iter().next().unwrap();
        db.mark_effect_done(effect.id).unwrap();

        let pending = db.pending_effects(100).unwrap();
        assert!(pending.is_empty());

        let done: i32 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM effects WHERE status = 'done'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(done, 1);
    }

    #[test]
    fn mark_effect_failed_increments_attempts() {
        let db = setup_db_with_task("t4");
        let e = make_effect("t4", "t4:move", EffectType::MoveIssue);
        db.insert_effects(&[e]).unwrap();

        let effect = db.pending_effects(1).unwrap().into_iter().next().unwrap();
        db.mark_effect_failed(effect.id, "network error").unwrap();

        let (attempts, status, next_retry_at): (i32, String, Option<String>) = db
            .conn
            .query_row(
                "SELECT attempts, status, next_retry_at FROM effects WHERE id = ?1",
                params![effect.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

        assert_eq!(attempts, 1);
        assert_eq!(status, "failed");
        assert!(next_retry_at.is_some()); // backoff timestamp set
    }

    #[test]
    fn mark_effect_dead_after_max_attempts() {
        let db = setup_db_with_task("t5");
        let mut e = make_effect("t5", "t5:move", EffectType::MoveIssue);
        e.max_attempts = 1;
        db.insert_effects(&[e]).unwrap();

        let effect = db.pending_effects(1).unwrap().into_iter().next().unwrap();
        db.mark_effect_failed(effect.id, "fatal error").unwrap();

        let status: String = db
            .conn
            .query_row(
                "SELECT status FROM effects WHERE id = ?1",
                params![effect.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "dead");
    }

    #[test]
    fn blocking_effects_done_checks_only_blocking() {
        let db = setup_db_with_task("t6");

        // One blocking effect marked done
        let mut e_blocking = make_effect("t6", "t6:move", EffectType::MoveIssue);
        e_blocking.blocking = true;
        db.insert_effects(&[e_blocking]).unwrap();
        let blocking_id = db.pending_effects(1).unwrap()[0].id;
        db.mark_effect_done(blocking_id).unwrap();

        // One non-blocking effect still pending
        let mut e_nonblocking = make_effect("t6", "t6:comment", EffectType::PostComment);
        e_nonblocking.blocking = false;
        db.insert_effects(&[e_nonblocking]).unwrap();

        // Should be true: blocking is done, non-blocking doesn't count
        assert!(db.blocking_effects_done("t6").unwrap());
    }

    #[test]
    fn dead_blocking_effects_count_as_terminal() {
        // A dead blocking effect (permanently failed after max retries) is a terminal state.
        // blocking_effects_done() must return true so the task can be marked linear_pushed.
        // Without this, dead effects leave linear_pushed=0 forever → daemon spam every tick.
        let db = setup_db_with_task("t6d");

        let mut e = make_effect("t6d", "t6d:move", EffectType::MoveIssue);
        e.max_attempts = 1; // one attempt → immediately dead on first failure
        db.insert_effects(&[e]).unwrap();

        let effect_id = db.pending_effects(1).unwrap()[0].id;
        db.mark_effect_failed(effect_id, "permanent API error")
            .unwrap();

        // Verify the effect is now dead
        let status: String = db
            .conn
            .query_row(
                "SELECT status FROM effects WHERE id = ?1",
                params![effect_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "dead");

        // blocking_effects_done must return true — dead is terminal, same as done
        assert!(
            db.blocking_effects_done("t6d").unwrap(),
            "dead blocking effect should count as resolved in blocking_effects_done()"
        );
    }

    #[test]
    fn blocking_effects_done_false_when_pending() {
        // Sanity check: a pending blocking effect keeps blocking_effects_done() returning false.
        let db = setup_db_with_task("t6e");

        let e = make_effect("t6e", "t6e:move", EffectType::MoveIssue);
        db.insert_effects(&[e]).unwrap();

        assert!(
            !db.blocking_effects_done("t6e").unwrap(),
            "pending blocking effect should keep blocking_effects_done() returning false"
        );
    }

    #[test]
    fn pending_effects_respects_retry_after() {
        let db = setup_db_with_task("t7");
        let e = make_effect("t7", "t7:move", EffectType::MoveIssue);
        db.insert_effects(&[e]).unwrap();

        // Set next_retry_at far in the future
        db.conn
            .execute(
                "UPDATE effects SET status = 'failed', next_retry_at = '2099-01-01T00:00:00'",
                [],
            )
            .unwrap();

        let pending = db.pending_effects(100).unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn blocking_effects_done_true_when_no_effects() {
        let db = setup_db_with_task("t8");
        assert!(db.blocking_effects_done("t8").unwrap());
    }

    #[test]
    fn insert_effects_with_conn_inside_transaction() {
        let db = setup_db_with_task("t9");
        let e = make_effect("t9", "t9:move", EffectType::MoveIssue);

        db.transaction(|conn| {
            Db::insert_effects_with_conn(conn, &[e.clone()])?;
            Ok(())
        })
        .unwrap();

        let pending = db.pending_effects(100).unwrap();
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn dead_effects_returns_only_dead() {
        let db = setup_db_with_task("t10");

        let mut e_dead = make_effect("t10", "t10:dead", EffectType::MoveIssue);
        e_dead.max_attempts = 1;
        let mut e_pending = make_effect("t10", "t10:pending", EffectType::PostComment);
        e_pending.max_attempts = 5;

        db.insert_effects(&[e_dead, e_pending]).unwrap();

        // Kill the first effect by failing it once (max_attempts=1 → dead immediately)
        let dead_id = db.pending_effects(1).unwrap()[0].id;
        db.mark_effect_failed(dead_id, "fatal").unwrap();

        let dead = db.dead_effects().unwrap();
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].effect_type, EffectType::MoveIssue);
    }

    #[test]
    fn retry_effect_resets_dead_to_pending() {
        let db = setup_db_with_task("t11");

        let mut e = make_effect("t11", "t11:move", EffectType::MoveIssue);
        e.max_attempts = 1;
        db.insert_effects(&[e]).unwrap();

        let effect_id = db.pending_effects(1).unwrap()[0].id;
        db.mark_effect_failed(effect_id, "fatal").unwrap();

        // Verify it's dead
        let status: String = db
            .conn
            .query_row(
                "SELECT status FROM effects WHERE id = ?1",
                params![effect_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "dead");

        // Reset via retry
        let changed = db.retry_effect(effect_id).unwrap();
        assert!(changed, "retry_effect should return true for a dead effect");

        // Should be pending again with attempts=0
        let (attempts, new_status, next_retry): (i32, String, Option<String>) = db
            .conn
            .query_row(
                "SELECT attempts, status, next_retry_at FROM effects WHERE id = ?1",
                params![effect_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(new_status, "pending");
        assert_eq!(attempts, 0);
        assert!(next_retry.is_none());
    }

    #[test]
    fn retry_effect_returns_false_for_done_effect() {
        let db = setup_db_with_task("t12");
        let e = make_effect("t12", "t12:move", EffectType::MoveIssue);
        db.insert_effects(&[e]).unwrap();

        let effect_id = db.pending_effects(1).unwrap()[0].id;
        db.mark_effect_done(effect_id).unwrap();

        // retry_effect should NOT reset a done effect
        let changed = db.retry_effect(effect_id).unwrap();
        assert!(
            !changed,
            "retry_effect should return false for a done effect"
        );
    }

    #[test]
    fn effects_for_task_returns_all_statuses() {
        let db = setup_db_with_task("t13");

        let e1 = make_effect("t13", "t13:move", EffectType::MoveIssue);
        let e2 = make_effect("t13", "t13:comment", EffectType::PostComment);
        db.insert_effects(&[e1, e2]).unwrap();

        // Mark e1 done
        let effects = db.pending_effects(10).unwrap();
        db.mark_effect_done(effects[0].id).unwrap();

        // effects_for_task should return both (done + pending)
        let all = db.effects_for_task("t13").unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn pending_and_failed_effects_excludes_done_and_dead() {
        let db = setup_db_with_task("t14");

        let e1 = make_effect("t14", "t14:move", EffectType::MoveIssue);
        let mut e2 = make_effect("t14", "t14:dead", EffectType::PostComment);
        e2.max_attempts = 1;
        let e3 = make_effect("t14", "t14:comment", EffectType::AddLabel);
        db.insert_effects(&[e1, e2, e3]).unwrap();

        let pending = db.pending_effects(100).unwrap();
        // Mark e1 done
        db.mark_effect_done(pending[0].id).unwrap();
        // Kill e2 (max_attempts=1 → dead on first failure)
        db.mark_effect_failed(pending[1].id, "fatal").unwrap();

        // Only e3 (pending AddLabel) remains
        let visible = db.pending_and_failed_effects().unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].effect_type, EffectType::AddLabel);
    }
}
