use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use super::topic_decisions;

pub(super) const TRUNCATION_RESERVE: usize = 256;

fn retain(entry: Value, entries: &mut Vec<Value>, remaining: &mut usize, omitted: &mut usize) {
    if *omitted == 0 {
        let cost = entry.to_string().len() + usize::from(!entries.is_empty());
        if cost <= *remaining {
            *remaining -= cost;
            entries.push(entry);
            return;
        }
    }
    *omitted += 1;
}

pub(super) fn attach(
    db: &Connection,
    project: &str,
    anchor: &str,
    context: &mut Value,
    limit: usize,
) -> Result<()> {
    let Some(scope) = topic_decisions::discussion(db, project, anchor)? else {
        return Ok(());
    };
    let omission = |inputs: usize, notes: usize| {
        json!({
            "older_inputs_omitted": inputs,
            "older_turn_notes_omitted": notes,
            "reason": "Earlier entries in this discussion did not fit the request byte budget."
        })
    };
    let mut remaining = limit
        .saturating_sub(context.to_string().len())
        .saturating_sub(TRUNCATION_RESERVE);
    let mut prior = Vec::new();
    let mut omitted_inputs = 0;
    let mut statement = db.prepare(
        "SELECT origin_event_id,payload FROM manager_inputs
         WHERE project_id=? AND status='consumed' AND kind='user' AND origin_event_id IS NOT NULL
         ORDER BY created_at DESC,rowid DESC",
    )?;
    let mut rows = statement.query([project])?;
    while let Some(row) = rows.next()? {
        let origin: String = row.get(0)?;
        if context["latest_user_steering"]["source_event_id"].as_str() == Some(&origin)
            || topic_decisions::discussion(db, project, &origin)?.as_deref() != Some(&scope)
        {
            continue;
        }
        if omitted_inputs != 0 {
            omitted_inputs += 1;
            continue;
        }
        let raw: String = row.get(1)?;
        let historical: Value = serde_json::from_str(&raw)?;
        let in_ledger = context["topic_decisions"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|topic| topic["decisions"].as_array().into_iter().flatten())
            .any(|decision| {
                decision["source_event_id"].as_str() == Some(&origin)
                    && decision["user_evidence"] == historical["user_input"]["body"]
            });
        let entry = if in_ledger {
            json!({"source_event_id":origin,"body_in_topic_decisions":true})
        } else {
            json!({"source_event_id":origin,"data":historical})
        };
        retain(entry, &mut prior, &mut remaining, &mut omitted_inputs);
    }
    prior.reverse();
    context["prior_conversation"] = json!(prior);

    let mut statement = db.prepare(
        "SELECT n.run_id,n.note FROM manager_turn_notes n JOIN manager_runs r ON r.id=n.run_id
         WHERE r.project_id=? AND n.note!='' ORDER BY r.created_at DESC,r.rowid DESC",
    )?;
    let mut sources = db.prepare(
        "SELECT i.origin_event_id FROM manager_run_inputs b JOIN manager_inputs i ON i.id=b.input_id
         WHERE b.run_id=? AND i.project_id=?",
    )?;
    let mut rows = statement.query([project])?;
    let mut notes = Vec::new();
    let mut omitted_notes = 0;
    while let Some(row) = rows.next()? {
        let run: String = row.get(0)?;
        let mut linked = sources.query(params![run, project])?;
        let mut matches = false;
        while let Some(input) = linked.next()? {
            let origin: Option<String> = input.get(0)?;
            matches = if let Some(origin) = origin {
                topic_decisions::discussion(db, project, &origin)?.as_deref() == Some(&scope)
            } else {
                false
            };
            if !matches {
                break;
            }
        }
        if matches {
            retain(
                json!(row.get::<_, String>(1)?),
                &mut notes,
                &mut remaining,
                &mut omitted_notes,
            );
        }
    }
    notes.reverse();
    context["manager_decisions"] = json!(notes);
    if omitted_inputs != 0 || omitted_notes != 0 {
        context["topic_history_truncation"] = omission(omitted_inputs, omitted_notes);
    }
    Ok(())
}
