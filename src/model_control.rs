//! Durable Feishu model controls, independent of the Agent message queue.
use crate::{
    App,
    bot::Binding,
    db,
    models::{self, Catalog, Selection},
};
use anyhow::{Context, Result, ensure};
use open_lark::ws_client::{CardActionTrigger, CardActionTriggerResponse, CardToast};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

const EXPIRES_MS: i64 = 30 * 60 * 1000;
const MAX_OPTIONS: usize = 60;

pub fn migrate(c: &Connection, recover: bool) -> Result<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS model_control_commands (
      id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
      source_id TEXT NOT NULL, app_id TEXT NOT NULL, chat_id TEXT NOT NULL, operator_id TEXT NOT NULL,
      agent TEXT NOT NULL, generation TEXT NOT NULL, token TEXT NOT NULL UNIQUE,
      phase TEXT NOT NULL DEFAULT 'pending', options TEXT NOT NULL DEFAULT '[]',
      selection TEXT, action_id TEXT, outbox_id TEXT, error TEXT NOT NULL DEFAULT '',
      created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
      UNIQUE(project_id,source_id));
      CREATE TABLE IF NOT EXISTS model_control_actions (
      event_id TEXT PRIMARY KEY, command_id TEXT NOT NULL REFERENCES model_control_commands(id) ON DELETE CASCADE,
      selection TEXT NOT NULL, created_at INTEGER NOT NULL);")?;
    if recover {
        c.execute(
            "UPDATE model_control_commands SET phase='pending' WHERE phase='building'",
            [],
        )?;
    }
    Ok(())
}

pub fn is_command(text: &str) -> bool {
    matches!(text.trim(), "/model" | "/models")
}

/// Called inside the bot-source INSERT transaction, only for a newly inserted
/// trusted source. No source can commit without its corresponding control job.
pub fn journal_source(
    c: &Connection,
    project: &str,
    app_id: &str,
    chat: &str,
    source: &str,
    text: &str,
    attachment: &Value,
) -> Result<()> {
    ensure!(
        is_command(crate::cli_control::command_text(text, &attachment["source"])) && attachment["daemon_control"] == "model",
        "Invalid model command"
    );
    let operator = attachment["source"]["sender"]["open_id"]
        .as_str()
        .filter(|s| s.starts_with("ou_") && s.len() <= 200)
        .context("Model command is missing its trusted sender")?;
    ensure!(
        chat.starts_with("oc_") && attachment["source"]["chat_id"] == chat,
        "Model command is missing its trusted conversation"
    );
    let known: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE project_id=? AND direction='in' AND source_id=? AND json_extract(attachment,'$.bot_app_id')=? AND json_extract(attachment,'$.daemon_control')='model' AND json_extract(attachment,'$.source.sender.open_id')=? AND json_extract(attachment,'$.source.chat_id')=?)",
        params![project,source,app_id,operator,chat], |r| r.get(0))?;
    ensure!(
        known,
        "Model command must have a trusted daemon-control source record"
    );
    let (agent,generation): (String,String) = c.query_row("SELECT agent,generation FROM projects WHERE id=? AND EXISTS(SELECT 1 FROM project_bots WHERE project_id=projects.id AND app_id=?)",
        params![project,app_id],|r| Ok((r.get(0)?,r.get(1)?)))?;
    c.execute("INSERT INTO model_control_commands(id,project_id,source_id,app_id,chat_id,operator_id,agent,generation,token,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
        params![db::id(),project,source,app_id,chat,operator,agent,generation,db::id(),db::now(),db::now()])?;
    c.execute("UPDATE messages SET status='daemon_control',updated_at=? WHERE project_id=? AND direction='in' AND source_id=? AND status='queued'",params![db::now(),project,source])?;
    Ok(())
}

fn toast(kind: &str, content: &str) -> CardActionTriggerResponse {
    CardActionTriggerResponse {
        toast: Some(CardToast {
            r#type: kind.into(),
            title: None,
            content: Some(content.into()),
        }),
        card: None,
    }
}

/// The native callback ACK contains no network/model work. Only a validated,
/// deduplicated action journal entry is accepted before returning success.
pub fn ingest_callback(
    app: &Arc<App>,
    binding: &Binding,
    event: &CardActionTrigger,
) -> Result<CardActionTriggerResponse> {
    let result = app.db.sync(|c| accept(c, binding, event));
    match result {
        Ok(true) => {
            app.notify.notify_one();
            Ok(toast(
                "info",
                "Model change accepted. Its result will appear in this topic.",
            ))
        }
        Ok(false) => Ok(toast(
            "info",
            "This action has already been accepted. Its saved result will be used.",
        )),
        Err(error) => Ok(toast("error", &error.to_string())),
    }
}

fn accept(c: &Connection, binding: &Binding, event: &CardActionTrigger) -> Result<bool> {
    ensure!(
        event.header.event_type == "card.action.trigger" && event.header.app_id == binding.app_id,
        "This card belongs to a different bot"
    );
    ensure!(
        !event.header.event_id.is_empty(),
        "The callback is missing its event identity"
    );
    let action = event
        .event
        .action
        .as_ref()
        .context("No model action was supplied")?;
    ensure!(
        action.tag == "button",
        "Submit the model form to apply a selection"
    );
    let token = action
        .name
        .strip_prefix("apply_")
        .context("This is not a Dauerbot model control")?;
    let operator = event
        .event
        .operator
        .as_ref()
        .context("The callback is missing its operator")?;
    let context = event
        .event
        .context
        .as_ref()
        .context("The callback is missing its original message context")?;
    ensure!(
        event.event.host == "im_message" && !context.open_message_id.is_empty(),
        "Open the original model card in its conversation"
    );
    let tx = c.unchecked_transaction()?;
    let command: Option<Command> = tx
        .query_row(
            &format!("SELECT {COLUMNS} FROM model_control_commands WHERE token=?"),
            [token],
            row,
        )
        .optional()?;
    let command = command.context("This model card is no longer available; send /models again")?;
    ensure!(
        command.project == binding.project_id && command.app_id == binding.app_id,
        "This model card belongs to a different project or bot"
    );
    let bound: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM project_bots WHERE project_id=? AND app_id=?)",
        params![command.project, command.app_id],
        |r| r.get(0),
    )?;
    ensure!(
        bound,
        "This project's bot changed; send /models to the current bot"
    );
    ensure!(
        command.operator == operator.open_id && command.chat == context.open_chat_id,
        "Only the requesting user can use this card in its original conversation"
    );
    let sent: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE id=? AND project_id=? AND status='sent' AND json_extract(attachment,'$.bot_app_id')=? AND json_extract(attachment,'$.sent_message_id')=? AND json_extract(attachment,'$.sent_chat_id')=?)",
        params![command.outbox,command.project,command.app_id,context.open_message_id,command.chat],|r|r.get(0))?;
    ensure!(
        sent,
        "The original model-card delivery has not been verified; send /models again"
    );
    let values = action
        .form_value
        .as_ref()
        .context("Submit the complete model form")?;
    let explicit = values["model_id"].as_str().unwrap_or("").trim();
    let selection = if !explicit.is_empty() {
        Some(models::validate(
            &command.agent,
            &Selection {
                model: explicit.into(),
                provider: values["provider_id"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_owned),
            },
        )?)
    } else {
        let chosen = values["catalog_model"]
            .as_str()
            .context("Choose a model or enter its exact model ID")?;
        if chosen == "native" {
            None
        } else {
            let index: usize = chosen
                .strip_prefix('m')
                .context("Unknown model option; send /models again")?
                .parse()
                .context("Unknown model option")?;
            let options: Vec<Selection> = serde_json::from_str(&command.options)?;
            Some(models::validate(
                &command.agent,
                options
                    .get(index)
                    .context("Unknown model option; send /models again")?,
            )?)
        }
    };
    let encoded = serde_json::to_string(&selection)?;
    if let Some((id, saved)) = tx
        .query_row(
            "SELECT command_id,selection FROM model_control_actions WHERE event_id=?",
            [&event.header.event_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
    {
        ensure!(
            id == command.id && saved == encoded,
            "The callback identity was reused for a different action"
        );
        return Ok(false);
    }
    if command.phase != "card_ready" {
        ensure!(
            matches!(command.phase.as_str(), "accepted" | "applying" | "done")
                && command.selection.as_deref() == Some(&encoded),
            "This model card has already been used; send /models for a new selection"
        );
        return Ok(false);
    }
    ensure!(
        db::now() - command.created_at <= EXPIRES_MS,
        "This model card expired; send /models again"
    );
    let current: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id=? AND agent=? AND generation=?)",
        params![command.project, command.agent, command.generation],
        |r| r.get(0),
    )?;
    ensure!(
        current,
        "This project's CLI or session changed; send /models again"
    );
    tx.execute("INSERT INTO model_control_actions(event_id,command_id,selection,created_at) VALUES(?,?,?,?)",
        params![event.header.event_id,command.id,encoded,db::now()])?;
    ensure!(tx.execute("UPDATE model_control_commands SET phase='accepted',selection=?,action_id=?,updated_at=? WHERE id=? AND phase='card_ready'",
        params![encoded,command.id,db::now(),command.id])? == 1, "This model action was already accepted");
    tx.commit()?;
    Ok(true)
}

#[derive(Clone)]
struct Command {
    id: String,
    project: String,
    source: String,
    app_id: String,
    chat: String,
    operator: String,
    agent: String,
    generation: String,
    token: String,
    phase: String,
    options: String,
    selection: Option<String>,
    outbox: Option<String>,
    created_at: i64,
}
const COLUMNS: &str = "id,project_id,source_id,app_id,chat_id,operator_id,agent,generation,token,phase,options,selection,outbox_id,created_at";
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Command> {
    Ok(Command {
        id: r.get(0)?,
        project: r.get(1)?,
        source: r.get(2)?,
        app_id: r.get(3)?,
        chat: r.get(4)?,
        operator: r.get(5)?,
        agent: r.get(6)?,
        generation: r.get(7)?,
        token: r.get(8)?,
        phase: r.get(9)?,
        options: r.get(10)?,
        selection: r.get(11)?,
        outbox: r.get(12)?,
        created_at: r.get(13)?,
    })
}

/// The startup owner should spawn exactly one worker. Read-only native discovery
/// never delays the Agent delivery loop or the synchronous card callback.
pub async fn run(app: Arc<App>) {
    loop {
        if app.shutdown.is_cancelled() {
            break;
        }
        if let Some(_permit) = app.service.gate.enter()
            && let Err(error) = reconcile(&app).await
        {
            tracing::warn!(error=%error, "Model-control work could not complete");
        }
        tokio::select! { _ = app.shutdown.cancelled() => break, _ = tokio::time::sleep(Duration::from_millis(500)) => {} }
    }
}

pub async fn reconcile(app: &Arc<App>) -> Result<()> {
    let command = app.db.call(|c| {
        Ok(c.query_row(&format!("SELECT {COLUMNS} FROM model_control_commands WHERE phase IN ('pending','accepted','applying') ORDER BY rowid LIMIT 1"), [], row).optional()?)
    }).await?;
    let Some(command) = command else {
        return Ok(());
    };
    if command.phase == "pending" {
        let saved = command.clone();
        return match build(app, command).await {
            Ok(()) => Ok(()),
            Err(error) => finish(app, &saved, "failed", &format!("The native model catalogue could not be loaded: {error}. Send /models to try again; the Agent can still receive normal instructions.")).await,
        };
    }
    let selection: Option<Selection> = serde_json::from_str(
        command
            .selection
            .as_deref()
            .context("Accepted model action has no selection")?,
    )?;
    let id = command.id.clone();
    app.db.call(move |c| {
        c.execute("UPDATE model_control_commands SET phase='applying',updated_at=? WHERE id=? AND phase='accepted'",params![db::now(),id])?;
        Ok(())
    }).await?;
    // This persisted request ID makes a restart between acceptance and the
    // response safe. The core method owns CLI/generation fencing and dedup.
    match app
        .request_model_switch_with_id(
            &command.project,
            selection,
            &command.id,
            Some(models::Scope {
                agent: command.agent.clone(),
                generation: command.generation.clone(),
            }),
        )
        .await
    {
        Ok(_) => {
            finish(
                app,
                &command,
                "done",
                "Model change requested. The CLI's effective model is confirmed separately.",
            )
            .await
        }
        Err(error) => {
            let (id, project) = (command.id.clone(), command.project.clone());
            // A response can fail after the transition transaction committed.
            // Inspect the same request before describing it as rejected.
            let saved = app
                .db
                .call(move |c| {
                    Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM cli_switch_requests WHERE id=? AND project_id=?)",
                params![id,project], |r| r.get::<_,bool>(0),
            )?)
                })
                .await?;
            if saved {
                finish(app, &command, "done", "The model-change request is saved. Supervised transition or repair will continue; no duplicate request was created.").await
            } else {
                finish(app, &command, "failed", &format!("The model change could not be accepted: {error}. Your instructions remain available; send /models to choose again.")).await
            }
        }
    }
}

async fn build(app: &Arc<App>, command: Command) -> Result<()> {
    let id = command.id.clone();
    let claimed = app.db.call(move |c| Ok(c.execute("UPDATE model_control_commands SET phase='building',updated_at=? WHERE id=? AND phase='pending'",params![db::now(),id])?==1)).await?;
    if !claimed {
        return Ok(());
    }
    let project = app.db.project(&command.project).await?;
    if project.agent != command.agent || project.generation != command.generation {
        return finish(
            app,
            &command,
            "failed",
            "The project session changed while loading models. Send /models to open a new picker.",
        )
        .await;
    }
    let (project_id, agent) = (command.project.clone(), command.agent.clone());
    let configured = app
        .db
        .call(move |c| models::load(c, &project_id, &agent))
        .await?;
    let catalog = models::discover(&project, configured).await?;
    let selections: Vec<Selection> = catalog
        .options
        .iter()
        .take(MAX_OPTIONS)
        .map(|option| Selection {
            model: option.id.clone(),
            provider: option.provider.clone(),
        })
        .collect();
    let card = picker_card(&command, &catalog);
    app.db.call(move |c| {
        let tx = c.unchecked_transaction()?;
        let output = db::enqueue_out_request(&tx,&command.project,&command.app_id,&format!("model-card-{}",command.id),
            "Choose this project's model",json!({"to":command.chat,"reply_to_message_id":command.source,"interactive_card":card}))?;
        tx.execute("UPDATE model_control_commands SET phase='card_ready',options=?,outbox_id=?,updated_at=? WHERE id=? AND phase='building'",
            params![serde_json::to_string(&selections)?,output,db::now(),command.id])?;
        tx.commit()?;
        Ok(())
    }).await?;
    app.notify.notify_one();
    Ok(())
}

async fn finish(app: &Arc<App>, command: &Command, phase: &str, message: &str) -> Result<()> {
    let command = command.clone();
    let (phase, message) = (phase.to_owned(), message.to_owned());
    app.db
        .call(move |c| {
            let tx = c.unchecked_transaction()?;
            db::enqueue_out_request(
                &tx,
                &command.project,
                &command.app_id,
                &format!("model-result-{}", command.id),
                &message,
                json!({"to":command.chat,"reply_to_message_id":command.source}),
            )?;
            tx.execute(
                "UPDATE model_control_commands SET phase=?,error=?,updated_at=? WHERE id=?",
                params![
                    phase,
                    if phase == "failed" { &message } else { "" },
                    db::now(),
                    command.id
                ],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    app.notify.notify_one();
    Ok(())
}

fn plain(text: &str) -> Value {
    json!({"tag":"plain_text","content":text})
}
fn clean(text: &str) -> String {
    text.chars().take(90).collect()
}

/// Card2 schema workflow: one status block, one form block, one guide block;
/// a single primary action and a bounded native catalogue, never guessed IDs.
fn picker_card(command: &Command, catalog: &Catalog) -> Value {
    let mut options =
        vec![json!({"text":plain("Follow native CLI/session settings"),"value":"native"})];
    for (index, option) in catalog.options.iter().take(MAX_OPTIONS).enumerate() {
        let label = option
            .provider
            .as_ref()
            .map(|p| format!("{p} / {}", option.label))
            .unwrap_or_else(|| option.label.clone());
        options.push(json!({"text":plain(&clean(&label)),"value":format!("m{index}")}));
    }
    let mut fields = vec![
        json!({"tag":"select_static","name":"catalog_model","placeholder":plain("Choose a native model"),"width":"fill","options":options}),
        json!({"tag":"input","name":"model_id","placeholder":plain("Or enter an exact model ID"),"max_length":256}),
    ];
    if matches!(command.agent.as_str(), "pi" | "codex") {
        fields.push(json!({"tag":"input","name":"provider_id","placeholder":plain("Provider for an explicit ID (optional)"),"max_length":256}));
    }
    fields.push(json!({"tag":"button","name":format!("apply_{}",command.token),"text":plain("Use this model"),"type":"primary_filled","width":"fill","form_action_type":"submit"}));
    let configured = catalog
        .configured
        .as_ref()
        .map(|s| match &s.provider {
            Some(provider) => format!("{provider} / {}", s.model),
            None => s.model.clone(),
        })
        .unwrap_or_else(|| "Native CLI/session settings".into());
    let status = format!(
        "**Choose a model for this project**\nCLI: {} · Requested selection: {}\nObserved running model: unverified\n<font color='grey'>Selection is not proof of the running model. Native login and global settings are preserved.</font>",
        command.agent, configured
    );
    let mut guide="Only the requesting user can submit this card. It expires in 30 minutes.\n\n**Shortcut:** Add a Feishu bot-menu item named `/models` with **Send message** behavior. Enable **Card action trigger** in Events & Callbacks → Callback configuration.\n\nSend `/models` again to refresh the catalogue.".to_owned();
    if catalog.options.len() > MAX_OPTIONS {
        guide.push_str("\nThe list is shortened; use an exact model ID for other native models.");
    }
    if !catalog.warnings.is_empty() {
        guide.push_str("\nNative catalogue discovery may be incomplete; exact IDs are accepted and validated by the CLI.");
    }
    json!({"schema":"2.0","config":{"update_multi":true,"enable_forward":false,"width_mode":"default"},
    "header":{"title":plain("Project model"),"subtitle":plain(&command.agent),"template":"blue"},
    "body":{"direction":"vertical","padding":"12px 12px 20px 12px","vertical_spacing":"12px","elements":[
        {"tag":"column_set","flex_mode":"none","background_style":"blue-50","columns":[{"tag":"column","width":"weighted","weight":1,"padding":"12px","elements":[{"tag":"markdown","content":status}]}]},
        {"tag":"form","name":"model_form","vertical_spacing":"8px","elements":fields},
        {"tag":"markdown","text_size":"notation","content":guide}
    ]}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Connection, Binding, CardActionTrigger) {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE projects(id TEXT PRIMARY KEY,agent TEXT,generation TEXT);
          CREATE TABLE project_bots(project_id TEXT,app_id TEXT);
          CREATE TABLE messages(id TEXT PRIMARY KEY,project_id TEXT,status TEXT,attachment TEXT);
          INSERT INTO projects VALUES('project','codex','generation');
          INSERT INTO project_bots VALUES('project','cli_test');
          INSERT INTO messages VALUES('outbox','project','sent','{\"bot_app_id\":\"cli_test\",\"sent_message_id\":\"om_card\",\"sent_chat_id\":\"oc_chat\"}');").unwrap();
        migrate(&c, false).unwrap();
        c.execute("INSERT INTO model_control_commands(id,project_id,source_id,app_id,chat_id,operator_id,agent,generation,token,phase,options,outbox_id,created_at,updated_at)
          VALUES('command','project','om_request','cli_test','oc_chat','ou_owner','codex','generation','token','card_ready','[]','outbox',?,?)",params![db::now(),db::now()]).unwrap();
        let binding = Binding {
            project_id: "project".into(),
            app_id: "cli_test".into(),
            app_secret: String::new(),
        };
        let event = serde_json::from_value(json!({"schema":"2.0","header":{"event_id":"event","event_type":"card.action.trigger","app_id":"cli_test"},
          "event":{"host":"im_message","operator":{"open_id":"ou_owner"},"context":{"open_message_id":"om_card","open_chat_id":"oc_chat"},
          "action":{"tag":"button","name":"apply_token","form_value":{"catalog_model":"native"}}}})).unwrap();
        (c, binding, event)
    }

    #[test]
    fn card_actions_are_scoped_to_requester_original_receipt_and_live_cli() {
        let (c, binding, event) = fixture();
        let mut wrong = event.clone();
        wrong.event.operator.as_mut().unwrap().open_id = "ou_other".into();
        assert!(accept(&c, &binding, &wrong).is_err());
        let mut forwarded = event.clone();
        forwarded.event.context.as_mut().unwrap().open_message_id = "om_forwarded".into();
        assert!(accept(&c, &binding, &forwarded).is_err());
        let mut other_chat = event.clone();
        other_chat.event.context.as_mut().unwrap().open_chat_id = "oc_other".into();
        assert!(accept(&c, &binding, &other_chat).is_err());
        c.execute("UPDATE projects SET generation='new'", [])
            .unwrap();
        assert!(accept(&c, &binding, &event).is_err());
        c.execute("UPDATE projects SET generation='generation'", [])
            .unwrap();
        c.execute("UPDATE project_bots SET app_id='cli_changed'", [])
            .unwrap();
        assert!(accept(&c, &binding, &event).is_err());
        assert_eq!(
            c.query_row("SELECT phase FROM model_control_commands", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "card_ready"
        );
    }

    #[test]
    fn accepted_action_deduplicates_retries_but_rejects_changed_or_unknown_choices() {
        let (c, binding, event) = fixture();
        let mut invalid = event.clone();
        invalid.event.action.as_mut().unwrap().form_value = Some(json!({"catalog_model":"m999"}));
        assert!(accept(&c, &binding, &invalid).is_err());
        assert!(accept(&c, &binding, &event).unwrap());
        assert!(!accept(&c, &binding, &event).unwrap());
        // A response retry stays a dedup hit even if the accepted operation
        // already replaced the generation; it must not create another effect.
        c.execute("UPDATE projects SET generation='after-accepted-switch'", [])
            .unwrap();
        assert!(!accept(&c, &binding, &event).unwrap());
        let mut changed = event.clone();
        changed.event.action.as_mut().unwrap().form_value =
            Some(json!({"model_id":"different-model"}));
        assert!(accept(&c, &binding, &changed).is_err());
        assert_eq!(
            c.query_row("SELECT COUNT(*) FROM model_control_actions", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn control_source_and_scope_snapshot_rollback_together() {
        let (c, _, _) = fixture();
        c.execute_batch(
            "ALTER TABLE messages ADD COLUMN direction TEXT;
          ALTER TABLE messages ADD COLUMN source_id TEXT;
          ALTER TABLE messages ADD COLUMN updated_at INTEGER;",
        )
        .unwrap();
        let attachment = json!({"bot_app_id":"cli_test","daemon_control":"model",
            "source":{"chat_id":"oc_chat","sender":{"open_id":"ou_owner"}}});
        {
            let tx = c.unchecked_transaction().unwrap();
            tx.execute("INSERT INTO messages(id,project_id,direction,source_id,status,attachment) VALUES('input','project','in','om_new','queued',?)",[attachment.to_string()]).unwrap();
            journal_source(
                &tx,
                "project",
                "cli_test",
                "oc_chat",
                "om_new",
                "/models",
                &attachment,
            )
            .unwrap();
            // A crash/error before the source transaction commits loses both
            // rows, so a replay can never leave an excluded, ownerless command.
        }
        assert_eq!(
            c.query_row("SELECT COUNT(*) FROM messages WHERE id='input'", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap(),
            0
        );
        assert_eq!(
            c.query_row(
                "SELECT COUNT(*) FROM model_control_commands WHERE source_id='om_new'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        let tx = c.unchecked_transaction().unwrap();
        tx.execute("INSERT INTO messages(id,project_id,direction,source_id,status,attachment) VALUES('input','project','in','om_new','queued',?)",[attachment.to_string()]).unwrap();
        journal_source(
            &tx,
            "project",
            "cli_test",
            "oc_chat",
            "om_new",
            "/model",
            &attachment,
        )
        .unwrap();
        tx.commit().unwrap();
        c.execute("UPDATE projects SET agent='agy',generation='later'", [])
            .unwrap();
        let scope: (String, String) = c
            .query_row(
                "SELECT agent,generation FROM model_control_commands WHERE source_id='om_new'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(scope, ("codex".into(), "generation".into()));
        assert_eq!(
            c.query_row("SELECT status FROM messages WHERE id='input'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "daemon_control"
        );
    }
}
