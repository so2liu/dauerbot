//! Proposal-only provider calls and durable publication into the ordinary outbox.
use super::{PROVIDER_CONFIGURED, llm::{self, ProviderConfig}, read_file};
use crate::{
    App, autonomy,
    db::{self, enqueue_out_request},
    panel,
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};

const SYSTEM: &str = include_str!("autonomy_prompt.md");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Submission {
    proposals: Vec<autonomy::Draft>,
}

struct Request {
    settings: autonomy::Settings,
    provider: ProviderConfig,
    provider_revision: i64,
    workspace: String,
    evidence: Value,
}

fn claim(c: &Connection, now: i64) -> Result<Option<Request>> {
    let Some(settings) = autonomy::claim_due(c, now)? else {
        return Ok(None);
    };
    let (base_url, model, api_key, provider_revision, configured, provider_kind, context_window) = c.query_row(
        &format!("SELECT base_url,model,api_key,revision,({PROVIDER_CONFIGURED}),provider_kind,context_window FROM manager_provider_config g WHERE id=1"),
        [],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get::<_,bool>(4)?,r.get::<_,String>(5)?,r.get(6)?)),
    )?;
    if !configured {
        return Ok(None);
    }
    let (name, workspace): (String, String) = c.query_row(
        "SELECT name,path FROM projects WHERE id=?",
        [&settings.project_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let evidence = panel::proposal_context(c, &settings.project_id, &settings.chat_id, now)?;
    let alerts = c
        .prepare("SELECT reason,last_seen,occurrences FROM work_attention
                  WHERE project_id=? AND last_seen>=? AND to_id=? ORDER BY last_seen DESC LIMIT 8")?
        .query_map(
            params![settings.project_id, now - 30 * 86_400_000, settings.chat_id],
            |r| Ok(json!({"kind":r.get::<_,String>(0)?,"at":r.get::<_,i64>(1)?,"count":r.get::<_,i64>(2)?})),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(Request {
        settings,
        provider: ProviderConfig {
            base_url,
            model,
            api_key,
            supports_images: false,
            provider_kind: serde_json::from_value(json!(provider_kind))?,
            context_window,
        },
        provider_revision,
        workspace,
        evidence: json!({"project":name,"activity":evidence,"alerts":alerts}),
    }))
}

fn publish(
    c: &Connection,
    settings: &autonomy::Settings,
    provider_revision: i64,
    submission: Submission,
    now: i64,
) -> Result<()> {
    let tx = c.unchecked_transaction()?;
    let current: bool = tx.query_row(
        "SELECT revision=? FROM manager_provider_config WHERE id=1",
        [provider_revision],
        |r| r.get(0),
    )?;
    ensure!(current, "Proposal generation provider changed");
    let proposals = autonomy::save_batch(&tx, settings, submission.proposals, now)?;
    if !proposals.is_empty() {
        let card = crate::autonomy_feishu::card(&proposals);
        crate::feishu::ensure_card(&card)?;
        let notice = proposals
            .iter()
            .map(|proposal| {
                format!(
                    "{}. {} · {}",
                    proposal.number, proposal.draft.title, proposal.draft.why_now
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let requested_at = settings
            .last_requested_at
            .expect("validated generation claim");
        let request_id = format!("autonomy-{}-{requested_at}", settings.project_id);
        let outbox = enqueue_out_request(
            &tx,
            &settings.project_id,
            &settings.app_id,
            &request_id,
            &notice,
            json!({
                "to": settings.chat_id,
                "discussion_scope": request_id,
                "discussion_intent": "decision",
                "autonomy_publication_revision": settings.revision,
                "interactive_card": card,
            }),
        )?;
        let ids = proposals
            .into_iter()
            .map(|proposal| proposal.id)
            .collect::<Vec<_>>();
        autonomy::set_outbox(&tx, &settings.project_id, &ids, &outbox)?;
    }
    tx.commit()?;
    Ok(())
}

async fn generate(app: &Arc<App>, mut request: Request) -> Result<()> {
    let workspace = request.workspace;
    request.evidence["project_goals"] = tokio::task::spawn_blocking(move || {
        let instructions = read_file(
        Path::new(&workspace),
        super::FileArgs {
            path: "AGENTS.md".into(),
            offset: None,
            limit: None,
        },
    )
    .ok();
        json!(
            instructions
                .as_ref()
                .and_then(|file| file["content"].as_str())
                .map(|content| content.chars().take(6_000).collect::<String>())
        )
    })
    .await?;
    let redacted = crate::manager_context::clean(&request.evidence.to_string()).0;
    let evidence =
        serde_json::from_str(&redacted).unwrap_or_else(|_| json!({"project_evidence":redacted}));
    let result =
        llm::propose(&request.provider, SYSTEM, &evidence, app.shutdown.clone()).await?;
    let submission = serde_json::from_value(result)?;
    let Some(_permit) = app.service.gate.enter() else {
        return Ok(());
    };
    app.db
        .call(move |c| {
            publish(
                c,
                &request.settings,
                request.provider_revision,
                submission,
                db::now(),
            )
        })
        .await?;
    app.notify.notify_one();
    Ok(())
}

pub(super) async fn run(app: Arc<App>) {
    loop {
        tokio::select! {
            _ = app.shutdown.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
        let Some(_manager_permit) = app.service.manager_gate.enter() else {
            continue;
        };
        let request = {
            let Some(_permit) = app.service.gate.enter() else {
                continue;
            };
            app.db.call(|c| claim(c, db::now())).await
        };
        if let Ok(Some(request)) = request
            && generate(&app, request).await.is_err()
        {
            // No provider body, stored evidence or endpoint belongs in logs.
            tracing::warn!("Autonomy proposal generation did not produce a publishable batch");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_one_card_and_links_all_drafts_before_any_execution() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db = db::Db::open(&dir.path().join("fixture.sqlite"))?;
        let now = db::now();
        db.sync(|c| {
            c.execute(
                "INSERT INTO projects(id,name,path,agent,created_at,mcp_token)
                 VALUES('p','Fixture','/isolated','codex',0,'local-fixture')",
                [],
            )?;
            crate::bot::save(c, "p", "cli_fixture", "local-fixture", false)?;
            c.execute("INSERT INTO conversations(project_id,chat_id,kind) VALUES('p','oc_fixture','group')", [])?;
            c.execute("UPDATE manager_provider_config SET base_url='http://127.0.0.1:1',model='fixture',api_key='fixture-only' WHERE id=1", [])?;
            autonomy::configure(c, "p", serde_json::from_value(json!({
                "enabled":true,"owner_open_id":"ou_fixture","chat_id":"oc_fixture"
            }))?, now)?;
            let request = claim(c, now)?.unwrap();
            let drafts = ["Clarify onboarding", "Document diagnostics"].map(|title| autonomy::Draft {
                title: title.to_owned(),
                why_now: "This step keeps coming up.".into(),
                scope: "Explain the workflow in the local guide.".into(),
                not_doing: "No feature changes.".into(),
                acceptance: "A new reader can follow the steps.".into(),
                risk: "Low.".into(),
                effort: "Small.".into(),
                category: "documentation".into(),
            });
            let oversized = Submission {
                proposals: (0..3).map(|index| autonomy::Draft {
                    title: format!("Oversized proposal {index}"),
                    why_now: "议".repeat(620),
                    scope: "界".repeat(620),
                    not_doing: "外".repeat(620),
                    acceptance: "验".repeat(620),
                    risk: "难".repeat(620),
                    ..drafts[0].clone()
                }).collect(),
            };
            let error = publish(c, &request.settings, request.provider_revision, oversized, now)
                .expect_err("Oversized card must not commit");
            assert_eq!(error.to_string(), "Interactive card exceeds the supported size");
            for table in ["proposals", "messages"] {
                assert_eq!(c.query_row(&format!("SELECT count(*) FROM {table}"),[],|r|r.get::<_,u32>(0))?,0);
            }
            publish(c,&request.settings,request.provider_revision,Submission { proposals: drafts.into() },now)?;
            let (count, batches): (u32, u32) = c.query_row(
                "SELECT count(*),count(DISTINCT outbox_id) FROM proposals WHERE status='pending'",
                [], |r| Ok((r.get(0)?,r.get(1)?)),
            )?;
            assert_eq!((count,batches),(2,1));
            let (content, attachment): (String, String) = c.query_row(
                "SELECT content,attachment FROM messages WHERE direction='out' AND status='pending'",
                [], |r| Ok((r.get(0)?,r.get(1)?)),
            )?;
            assert!(content.contains("1. Clarify onboarding"));
            assert!(content.contains("2. Document diagnostics"));
            let attachment: Value = serde_json::from_str(&attachment)?;
            assert!(attachment["interactive_card"].is_object());
            assert_eq!(attachment["autonomy_publication_revision"], request.settings.revision);
            assert_eq!(c.query_row("SELECT count(*) FROM work_events",[],|r|r.get::<_,u32>(0))?,0);
            assert!(claim(c, now + 1)?.is_none());
            Ok(())
        })
    }
}
