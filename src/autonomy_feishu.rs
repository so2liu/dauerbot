//! Feishu proposal decisions are bound to one confirmed card and bot.
use crate::{
    App,
    autonomy::{self, Authority, Decision},
    bot::Binding,
    db,
};
use anyhow::{Context, Result, ensure};
use open_lark::ws_client::{CardActionTrigger, CardActionTriggerResponse, CardToast};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const ACTION: &str = "dauerbot_proposal_decision";

pub(crate) fn card(items: &[autonomy::Proposal]) -> Value {
    let mut elements = Vec::new();
    for proposal in items {
        let autonomy::Proposal {
            number, id, draft, ..
        } = proposal;
        for text in [
            format!("{number}. {}", draft.title),
            format!("Why now: {}", draft.why_now),
            format!("Scope: {}", draft.scope),
            format!("Out of scope: {}", draft.not_doing),
            format!("Acceptance: {}", draft.acceptance),
            format!("Risk: {}", draft.risk),
        ] {
            elements.push(json!({"tag":"div","text":{"tag":"plain_text","content":text}}));
        }
        for (label, decision, style) in [
            ("Approve", "approve", "primary_filled"),
            ("Reject", "reject", "default"),
        ] {
            elements.push(json!({
                "tag":"button","text":{"tag":"plain_text","content":format!("{label} {number}")},
                "type":style,
                "behaviors":[{"type":"callback","value":{"action":ACTION,"proposal_id":id,"decision":decision}}],
            }));
        }
    }
    elements.push(json!({"tag":"div","text":{"tag":"plain_text","content":
        "The configured owner can also reply in this topic with Approve 2, Approve all, or Reject 1,3."}}));
    json!({
        "schema":"2.0", "config":{"enable_forward":false},
        "header":{"title":{"tag":"plain_text","content":"Proposals awaiting approval"},"template":"blue"},
        "body":{"elements":elements}
    })
}

// Validate the published request before the outbox claims an external send.
pub(crate) fn validate_publication(
    c: &Connection,
    project: &str,
    attachment: &Value,
) -> Result<()> {
    let Some(revision) = attachment.get("autonomy_publication_revision") else {
        return Ok(());
    };
    let current: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM autonomy_settings s JOIN project_bots b
         ON b.project_id=s.project_id AND b.app_id=s.app_id
         WHERE s.project_id=? AND s.enabled=1 AND s.revision=? AND s.app_id=? AND s.chat_id=?)",
        params![
            project,
            revision.as_i64(),
            attachment["bot_app_id"].as_str(),
            attachment["to"].as_str()
        ],
        |r| r.get(0),
    )?;
    ensure!(
        current,
        "Project autonomy decision destination changed before delivery"
    );
    Ok(())
}

pub(crate) fn callback(
    app: &App,
    binding: &Binding,
    event: &CardActionTrigger,
) -> Option<CardActionTriggerResponse> {
    let action = event.event.action.as_ref()?;
    let value = action.value.as_ref()?;
    if value["action"] != ACTION {
        return None;
    }
    let result = app.db.sync(|c| {
        ensure!(action.tag == "button", "Invalid proposal control");
        let tx = c.unchecked_transaction()?;
        let message = accept_card(&tx, binding, event, value, db::now())?;
        tx.commit()?;
        Ok(message)
    });
    let (kind, content) = match result {
        Ok(state) => {
            app.notify.notify_one();
            ("success", state)
        }
        Err(_) => (
            "error",
            "This action is not authorized, or the original proposal card is unavailable.".into(),
        ),
    };
    Some(CardActionTriggerResponse {
        toast: Some(CardToast {
            r#type: kind.into(),
            title: None,
            content: Some(content),
        }),
        card: None,
    })
}

fn accept_card(
    c: &Connection,
    binding: &Binding,
    event: &CardActionTrigger,
    value: &Value,
    now: i64,
) -> Result<String> {
    ensure!(
        event.header.event_type == "card.action.trigger"
            && event.header.app_id == binding.app_id
            && !event.header.event_id.is_empty()
            && event.event.host == "im_message",
        "Invalid proposal callback context"
    );
    let operator = event.event.operator.as_ref().context("Missing operator")?;
    let context = event
        .event
        .context
        .as_ref()
        .context("Missing card context")?;
    let id = value["proposal_id"]
        .as_str()
        .context("Missing proposal ID")?;
    let decision = match value["decision"].as_str() {
        Some("approve") => Decision::Approve,
        Some("reject") => Decision::Reject,
        _ => anyhow::bail!("Invalid proposal decision"),
    };
    let proposal = autonomy::decide_inside(
        c,
        id,
        decision,
        Authority::Feishu {
            project_id: &binding.project_id,
            app_id: &binding.app_id,
            chat_id: &context.open_chat_id,
            owner_open_id: &operator.open_id,
            anchor: &context.open_message_id,
        },
        now,
    )?;
    Ok(format!(
        "Proposal {} · {}",
        proposal.number,
        state(&proposal.status)
    ))
}

enum Selection {
    All,
    Numbered(BTreeSet<i64>),
}

fn parse(text: &str) -> Option<(Decision, Selection)> {
    let text = text.trim();
    if text == "全部批准" {
        return Some((Decision::Approve, Selection::All));
    }
    let (verb, rest) = text.split_once(' ')?;
    let decision = if verb.eq_ignore_ascii_case("approve") || verb == "批准" {
        Decision::Approve
    } else if verb.eq_ignore_ascii_case("reject") || verb == "驳回" {
        Decision::Reject
    } else {
        return None;
    };
    if verb.eq_ignore_ascii_case("approve") && rest.trim().eq_ignore_ascii_case("all") {
        return Some((Decision::Approve, Selection::All));
    }
    let numbers: BTreeSet<i64> = rest
        .split([',', '，'])
        .map(|part| {
            let number = part.trim();
            match number {
                "1" => Some(1),
                "2" => Some(2),
                "3" => Some(3),
                _ => None,
            }
        })
        .collect::<Option<_>>()?;
    Some((decision, Selection::Numbered(numbers)))
}

fn state(status: &str) -> &'static str {
    match status {
        "approved" => "Approved",
        "rejected" => "Rejected",
        "expired" => "Expired",
        _ => "Pending",
    }
}

/// A recognized decision inside a verified proposal topic is daemon control.
/// An unauthorized editor or unrelated message cannot enter the decision path.
pub(crate) fn text(
    c: &Connection,
    binding: &Binding,
    incoming_id: &str,
    text: &str,
    source: &Value,
    now: i64,
) -> Result<bool> {
    if source["message_type"] != "text" {
        return Ok(false);
    }
    let Some((decision, selection)) = parse(crate::cli_control::command_text(text, source)) else {
        return Ok(false);
    };
    let chat = source["chat_id"].as_str().unwrap_or_default();
    let owner = source["sender"]["open_id"].as_str().unwrap_or_default();
    let root = source["root_id"].as_str().unwrap_or_default();
    let parent = source["parent_id"].as_str().unwrap_or_default();
    if root.is_empty() && parent.is_empty() {
        return Ok(false);
    }
    if incoming_id.is_empty() || incoming_id.len() > 150 {
        return Ok(false);
    }

    let tx = c.unchecked_transaction()?;
    let proposals = tx
        .prepare(
            "SELECT p.id,p.number,p.source_message_id FROM proposals p
         JOIN messages m ON m.id=p.outbox_id AND m.project_id=p.project_id
         WHERE p.project_id=?1 AND p.source_message_id IN (?2,?3)
         AND p.source_message_id!='' AND m.status='sent' AND m.direction='out'
         AND json_extract(m.attachment,'$.bot_app_id')=?4
         AND json_extract(m.attachment,'$.sent_chat_id')=?5
         AND json_extract(m.attachment,'$.sent_message_id')=p.source_message_id
         ORDER BY p.number",
        )?
        .query_map(
            params![binding.project_id, root, parent, binding.app_id, chat],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let Some((_, _, anchor)) = proposals.first() else {
        return Ok(false);
    };
    if proposals.iter().any(|(_, _, current)| current != anchor) {
        return Ok(true);
    }
    let authorized: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM autonomy_settings s JOIN project_bots b
         ON b.project_id=s.project_id AND b.app_id=s.app_id
         WHERE s.project_id=? AND s.app_id=? AND s.chat_id=? AND s.owner_open_id=?)",
        params![binding.project_id, binding.app_id, chat, owner],
        |r| r.get(0),
    )?;
    if !authorized {
        return Ok(true);
    }

    let requested: Vec<_> = proposals
        .iter()
        .filter(|(_, number, _)| match &selection {
            Selection::All => true,
            Selection::Numbered(numbers) => numbers.contains(number),
        })
        .collect();
    let request_id = format!("autonomy-decision-{incoming_id}");
    let saved: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM messages WHERE project_id=? AND direction='out' AND source_id=?)",
        params![binding.project_id,request_id], |r|r.get(0)
    )?;
    if saved {
        return Ok(true);
    }

    let mut summary = Vec::new();
    if let Selection::Numbered(numbers) = &selection
        && requested.len() != numbers.len()
    {
        summary.push("The requested proposal numbers are not on this card.".into());
    } else {
        for (id, number, _) in requested {
            let outcome = autonomy::decide_inside(
                &tx,
                id,
                decision,
                Authority::Feishu {
                    project_id: &binding.project_id,
                    app_id: &binding.app_id,
                    chat_id: chat,
                    owner_open_id: owner,
                    anchor,
                },
                now,
            )?;
            summary.push(format!("Proposal {number} · {}", state(&outcome.status)));
        }
    }
    db::enqueue_out_request(
        &tx,
        &binding.project_id,
        &binding.app_id,
        &request_id,
        &summary.join("\n"),
        json!({"to":chat,"reply_to_message_id":anchor,"discussion_intent":"receipt"}),
    )?;
    tx.commit()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_commands_keep_scope_numbers_bounded() {
        assert!(matches!(
            parse("全部批准"),
            Some((Decision::Approve, Selection::All))
        ));
        assert!(matches!(
            parse("驳回 1，3,1"),
            Some((Decision::Reject, Selection::Numbered(ids))) if ids == BTreeSet::from([1,3])
        ));
        assert!(matches!(
            parse("Approve all"),
            Some((Decision::Approve, Selection::All))
        ));
        assert!(matches!(
            parse("Approve 2"),
            Some((Decision::Approve, Selection::Numbered(ids))) if ids == BTreeSet::from([2])
        ));
        assert!(matches!(
            parse("Reject 1,3"),
            Some((Decision::Reject, Selection::Numbered(ids))) if ids == BTreeSet::from([1,3])
        ));
        for no_command in [
            "Reject all",
            "Approve 2\nRun this",
            "批准 4",
            "批准 2,foo",
            "可否批准 2",
            "全部批准。",
            "批准 01",
            "批准 2\n开始执行",
        ] {
            assert!(parse(no_command).is_none(), "{no_command}");
        }
    }

    #[test]
    fn card_payload_contains_only_structured_decisions_and_plain_text() {
        let proposal = autonomy::Proposal {
            number: 2,
            id: "fixture-id".into(),
            project_id: "fixture".into(),
            status: "pending".into(),
            created_at: 0,
            decided_at: None,
            decided_by: None,
            execution_event_id: None,
            source_message_id: None,
            draft: autonomy::Draft {
                title: "<b>warning</b>".into(),
                why_now: "Reason".into(),
                scope: "Scope marker".into(),
                not_doing: "Excluded marker".into(),
                acceptance: "Acceptance marker".into(),
                risk: "Risk marker".into(),
                effort: "small".into(),
                category: "documentation".into(),
            },
        };
        let value = card(&[proposal]);
        assert_eq!(
            value["header"]["title"]["content"],
            "Proposals awaiting approval"
        );
        let elements = value["body"]["elements"].as_array().unwrap();
        assert_eq!(elements[6]["text"]["content"], "Approve 2");
        assert_eq!(elements[7]["text"]["content"], "Reject 2");
        for (index, expected) in [
            "2. <b>warning</b>",
            "Why now: Reason",
            "Scope: Scope marker",
            "Out of scope: Excluded marker",
            "Acceptance: Acceptance marker",
            "Risk: Risk marker",
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(elements[index]["text"]["tag"], "plain_text");
            assert_eq!(elements[index]["text"]["content"], expected);
        }
        for (index, decision) in [(6, "approve"), (7, "reject")] {
            assert_eq!(
                elements[index]["behaviors"][0]["value"],
                json!({
                    "action":ACTION,"proposal_id":"fixture-id","decision":decision
                })
            );
        }
    }
    #[test]
    fn ingress_binds_decisions_to_trusted_operator_and_sent_card() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db = db::Db::open(&dir.path().join("fixture.sqlite"))?;
        db.sync(|c| {
            c.execute_batch("INSERT INTO projects(id,name,path,agent,created_at,mcp_token)
                  VALUES('project','Fixture','/tmp','codex',0,'local-fixture');
                INSERT INTO project_bots(project_id,app_id,app_secret,updated_at)
                  VALUES('project','cli_fixture','local-fixture',0);
                INSERT INTO autonomy_settings(project_id,enabled,owner_open_id,app_id,chat_id)
                  VALUES('project',1,'ou_owner','cli_fixture','oc_fixture');
                INSERT INTO messages(id,project_id,direction,source_id,content,status,attachment,created_at,updated_at)
                  VALUES('outbox','project','out','fixture-approval','test','sent',
                  '{\"bot_app_id\":\"cli_fixture\",\"sent_chat_id\":\"oc_fixture\",\"sent_message_id\":\"om_card\"}',0,0);")?;
            let now = db::now();
            for number in 1..=3 {
                c.execute("INSERT INTO proposals(id,project_id,number,title,why_now,scope,not_doing,acceptance,risk,effort,category,dedupe_hash,status,created_at,source_message_id,outbox_id)
                  VALUES(?1,'project',?2,'Test improvement','Reduce confusion','Improve guidance','No other changes','Review guidance','Low','small','documentation',?1,'pending',?3,'om_card','outbox')",
                  params![format!("proposal-{number}"),number,now])?;
            }
            let binding = Binding { project_id: "project".into(),app_id:"cli_fixture".into(),app_secret:String::new() };
            let card_value = |proposal, decision| json!({"action":ACTION,"proposal_id":proposal,"decision":decision});
            let card_event = |app, operator, anchor| serde_json::from_value::<CardActionTrigger>(json!({
                "schema":"2.0", "header":{"event_id":"evt","event_type":"card.action.trigger","app_id":app},
                "event":{"host":"im_message","operator":{"open_id":operator},
                    "context":{"open_message_id":anchor,"open_chat_id":"oc_fixture"},
                    "action":{"tag":"button","value":card_value("proposal-1","approve")}}
            })).unwrap();
            let approve = card_value("proposal-1", "approve");
            for event in [
                card_event("cli_other","ou_owner","om_card"),
                card_event("cli_fixture","ou_other","om_card"),
                card_event("cli_fixture","ou_owner","om_other"),
            ] {
                let tx = c.unchecked_transaction()?;
                assert!(accept_card(&tx,&binding,&event,&approve,now).is_err());
            }
            let right = card_event("cli_fixture","ou_owner","om_card");
            for action in ["approve","reject"] {
                let tx = c.unchecked_transaction()?;
                assert_eq!(accept_card(&tx,&binding,&right,&card_value("proposal-1",action),now)?,
                           "Proposal 1 · Approved");
                tx.commit()?;
            }
            let source = |owner, root| json!({"message_type":"text","chat_type":"group","chat_id":"oc_fixture",
                "sender":{"open_id":owner},"root_id":root});
            assert!(!text(c,&binding,"om_outside","批准 2",&source("ou_owner","om_other"),now)?);
            assert!(text(c,&binding,"om_other_user","批准 2",&source("ou_other","om_card"),now)?);
            let mut owner_source = source("ou_owner","om_card");
            assert!(!text(c,&binding,"om_unverified","@_user_1 Reject 2",&owner_source,now)?);
            owner_source["mentions"] = json!([{"key":"@_user_1"}]);
            assert!(text(c,&binding,"om_reject","@_user_1 Reject 2",&owner_source,now)?);
            assert!(text(c,&binding,"om_reject","@_user_1 Reject 2",&owner_source,now)?);
            assert!(text(c,&binding,"om_approve","@_user_1 全部批准",&owner_source,now)?);
            let states = c.prepare("SELECT status FROM proposals ORDER BY number")?
                          .query_map([], |r| r.get::<_, String>(0))?
                          .collect::<rusqlite::Result<Vec<_>>>()?;
            assert_eq!(states, ["approved","rejected","approved"]);
            let executions: i64 = c.query_row("SELECT COUNT(*) FROM work_events WHERE source='autonomy'",[],|r|r.get(0))?;
            assert_eq!(executions,2);
            let replies: i64 = c.query_row("SELECT COUNT(*) FROM messages WHERE source_id LIKE 'autonomy-decision-%' AND
                json_extract(attachment,'$.reply_to_message_id')='om_card'",[],|r|r.get(0))?;
            assert_eq!(replies,2);
            Ok(())
        })
    }
}
