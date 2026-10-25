//! Read-only, bounded outbox snapshots. Called by the diagnostics worker only.
use super::*;
use serde_json::Value;

pub const OUTBOX_PAGE_SIZE: usize = 50;

#[derive(Debug)]
pub struct OutboxPage {
    pub total: usize,
    pub rejected: usize,
    pub offset: usize,
    pub retry_not_before: Option<String>,
    pub entries: Vec<OutboxDiagnostic>,
}

#[derive(Debug)]
pub struct OutboxDiagnostic {
    pub seq: i64,
    pub title: String,
    pub operation: String,
    pub state: &'static str,
    pub details: String,
}

impl Store {
    pub fn outbox_diagnostics(&self, offset: usize) -> Result<OutboxPage> {
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        let (total, rejected): (i64, i64) = tx.query_row(
            "SELECT COUNT(*),COALESCE(SUM(error IS NOT NULL),0) FROM outbox",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let total = usize::try_from(total)?;
        let rejected = usize::try_from(rejected)?;
        let offset = offset.min(total.saturating_sub(1) / OUTBOX_PAGE_SIZE * OUTBOX_PAGE_SIZE);
        let rows = tx.prepare("SELECT seq,entity_id,payload,claimed,error,checkpoint FROM outbox ORDER BY seq LIMIT ?1 OFFSET ?2")?
            .query_map(params![OUTBOX_PAGE_SIZE as i64, i64::try_from(offset)?], |row| Ok((
                row.get::<_,i64>(0)?, row.get::<_,String>(1)?, row.get::<_,String>(2)?,
                row.get::<_,bool>(3)?, row.get::<_,Option<String>>(4)?, row.get::<_,String>(5)?
            )))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut entries = Vec::with_capacity(rows.len());
        for (seq, id, payload, claimed, error, checkpoint) in rows {
            let value: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
            let kind = value["kind"].as_str().unwrap_or("unrecognized_operation");
            let operation = kind.replace('_', " ");
            let title: Option<String> = tx
                .query_row(
                    "SELECT title FROM (
                   SELECT content AS title,0 AS priority FROM tasks WHERE id=?1
                   UNION ALL SELECT name,1 FROM projects WHERE id=?1
                   UNION ALL SELECT name,2 FROM filters WHERE id=?1
                 ) ORDER BY priority LIMIT 1",
                    [&id],
                    |row| row.get(0),
                )
                .optional()?;
            let title = title.unwrap_or_else(|| id.clone());
            let blockers = if value.is_object() {
                // Match the rejection and temporary-resource rules used by
                // claim_next_outbox. Never infer blockers from queue position:
                // unrelated operations can pass a quarantined rejection.
                tx.prepare("SELECT b.seq,CASE WHEN b.error IS NOT NULL THEN 'rejected dependency' ELSE 'awaiting resource creation' END
                    FROM outbox b WHERE b.seq<>?2 AND (
                      (b.error IS NOT NULL AND (b.entity_id=?1 OR instr(?3,json_quote(b.entity_id))>0))
                      OR (json_extract(b.payload,'$.kind') IN ('quick_add','add_project','add_filter') AND (
                        (b.entity_id=?1 AND ?1 LIKE 'local:%' AND json_extract(?3,'$.kind') NOT IN ('quick_add','add_project','add_filter'))
                        OR EXISTS(SELECT 1 FROM json_tree(?3) reference WHERE reference.key IN ('id','parent_id','project_id')
                          AND reference.type='text' AND reference.value LIKE 'local:%' AND reference.value=b.entity_id)
                      ))) ORDER BY b.seq")?
                    .query_map(params![id,seq,payload], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            } else {
                Vec::new()
            };
            let state = if error.is_some() {
                "Rejected"
            } else if !blockers.is_empty() {
                "Blocked"
            } else {
                "Pending"
            };
            let mut details = format!(
                "Operation #{seq}: {operation}\n\nEntity: {title}\nID: {id}\nState: {state}\nClaimed / may have been sent: {}\n",
                if claimed { "yes" } else { "no" }
            );
            for (blocked_seq, reason) in blockers {
                details.push_str(&format!("\nBlocked by #{blocked_seq}: {reason}"));
            }
            // Expose progress flags, never raw HTTP/parser checkpoints or
            // arbitrary payload fields. Credentials are not part of this view.
            let checkpoint: Value = serde_json::from_str(&checkpoint).unwrap_or(Value::Null);
            if let Some(fields) = checkpoint.as_object() {
                let flags = fields
                    .iter()
                    .filter(|(key, _)| {
                        matches!(
                            key.as_str(),
                            "fallback"
                                | "parent"
                                | "order"
                                | "description"
                                | "update"
                                | "move"
                                | "cleanup"
                                | "rejected"
                        )
                    })
                    .filter_map(|(key, value)| {
                        value.as_bool().map(|value| format!("{key}={value}"))
                    })
                    .collect::<Vec<_>>();
                if !flags.is_empty() {
                    details.push_str(&format!("\n\nProgress: {}", flags.join(", ")));
                }
                for field in ["keys_done", "order-keys_done"] {
                    if let Some(done) = fields.get(field).and_then(Value::as_array) {
                        details.push_str(&format!(
                            "\nOrdering commands acknowledged: {}/{}",
                            done.iter().filter(|value| **value == true).count(),
                            done.len()
                        ));
                    }
                }
            }
            let mut affected = Vec::new();
            for field in ["project_id", "parent_id", "section_id"] {
                if let Some(id) = value
                    .get(field)
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                {
                    details.push_str(&format!("\n{field}: {id}"));
                }
            }
            if let Some(plans) = value.get("plans").and_then(Value::as_array) {
                for plan in plans {
                    if let Some(positions) = plan.get("positions").and_then(Value::as_array) {
                        affected.extend(
                            positions
                                .iter()
                                .filter_map(|position| position["id"].as_str())
                                .map(str::to_owned),
                        );
                    }
                }
            }
            for field in ["tasks", "projects"] {
                if let Some(positions) = value.get(field).and_then(Value::as_array) {
                    affected.extend(
                        positions
                            .iter()
                            .filter_map(|position| position["id"].as_str())
                            .map(str::to_owned),
                    );
                }
            }
            if !affected.is_empty() {
                details.push_str(&format!(
                    "\n\nAffected resource IDs: {}",
                    affected.join(", ")
                ));
            }
            if let Some(error) = error {
                details.push_str(&format!("\n\nComplete rejection error:\n{error}"));
            }
            if value.is_null() {
                details.push_str("\n\nThe queued operation could not be decoded.");
            }
            entries.push(OutboxDiagnostic {
                seq,
                title,
                operation,
                state,
                details,
            });
        }
        let retry_not_before =
            meta_from(&tx, "retry_not_before")?.filter(|value| !value.is_empty());
        tx.commit()?;
        Ok(OutboxPage {
            total,
            rejected,
            offset,
            retry_not_before,
            entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_report_real_blockers_complete_errors_and_checkpoint_progress_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("diagnostics.db")).unwrap();
        let inbox = store.project("inbox").unwrap().unwrap();
        let id = store.add_task("Offline task", &inbox).unwrap();
        store.edit_task(&id, "Updated title", "").unwrap();
        let first = store.claim_next_outbox().unwrap().unwrap();
        let error = "full rejection details\n".repeat(200);
        store.fail_outbox(first.seq, &error).unwrap();
        store.save_checkpoint(first.seq,&serde_json::json!({"parsed":{"private":"not shown"},"update":true,"keys_done":[true,false]})).unwrap();
        let other = store.add_task("Independent task", &inbox).unwrap();
        store
            .set_meta("retry_not_before", "2027-01-01T00:00:00Z")
            .unwrap();
        let before = store
            .outbox()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry.uuid.clone(),
                    serde_json::to_string(&entry.operation).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        let snapshot = store.outbox_diagnostics(0).unwrap();
        assert_eq!(snapshot.total, 3);
        assert_eq!(snapshot.rejected, 1);
        assert_eq!(
            snapshot
                .entries
                .iter()
                .map(|entry| entry.state)
                .collect::<Vec<_>>(),
            ["Rejected", "Blocked", "Pending"]
        );
        assert!(snapshot.entries[0].details.contains(&error));
        assert!(snapshot.entries[0].details.contains("1/2"));
        assert!(!snapshot.entries[0].details.contains("not shown"));
        assert!(
            snapshot.entries[1]
                .details
                .contains(&format!("Blocked by #{}", first.seq))
        );
        assert_eq!(store.claim_next_outbox().unwrap().unwrap().entity_id, other);
        assert_eq!(
            snapshot.retry_not_before.as_deref(),
            Some("2027-01-01T00:00:00Z")
        );
        assert_eq!(
            before,
            store
                .outbox()
                .unwrap()
                .iter()
                .map(|entry| (
                    entry.uuid.clone(),
                    serde_json::to_string(&entry.operation).unwrap()
                ))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn diagnostic_pages_are_bounded_and_adjust_when_the_queue_shrinks() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("diagnostics.db")).unwrap();
        let conn = store.connect().unwrap();
        for index in 0..125 {
            conn.execute("INSERT INTO outbox(uuid,entity_id,payload) VALUES(?1,?2,'{\"kind\":\"delete_task\"}')",params![index.to_string(),format!("task-{index}")]).unwrap();
        }
        assert_eq!(store.outbox_diagnostics(0).unwrap().entries.len(), 50);
        let last = store.outbox_diagnostics(100).unwrap();
        assert_eq!(last.entries.len(), 25);
        assert!(
            last.entries[0].seq
                > store
                    .outbox_diagnostics(50)
                    .unwrap()
                    .entries
                    .last()
                    .unwrap()
                    .seq
        );
        conn.execute("DELETE FROM outbox WHERE seq>10", []).unwrap();
        assert_eq!(store.outbox_diagnostics(100).unwrap().offset, 0);
    }
}
