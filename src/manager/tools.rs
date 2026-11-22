use super::*;
use rig_core::completion::ToolDefinition;

#[derive(Clone)]
pub(super) struct ToolCall {
    pub occurrence: String,
    pub name: String,
    pub arguments: Value,
}

pub(super) const NAMES: [&str; 11] = [
    "read_file",
    "write_file",
    "delegate",
    "worker_status",
    "steer_worker",
    "send_md",
    "send_file",
    "close_agent",
    "cli_send_keys",
    "cli_view",
    "schedule_run",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendText {
    to: String,
    text: String,
    reply_to_message_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendFile {
    to: String,
    path: String,
    caption: Option<String>,
    reply_to_message_id: Option<String>,
}

pub(super) async fn execute(
    app: &Arc<App>,
    ctx: &Context,
    call: ToolCall,
    effect_id: &str,
    channel: &str,
) -> Result<Value> {
    let Some(_permit) = app.service.gate.enter() else {
        bail!("Service ownership changed")
    };
    let effect = effect_id.to_owned();
    let result = perform(app, ctx, call, &effect, channel).await;
    // Result persistence belongs to the admitted effect even if steering arrived
    // meanwhile. Only the actor checkpoint has the stricter current-revision CAS.
    let value = match result {
        Ok(value) => value,
        Err(error) => json!({"error":error.to_string()}),
    };
    let id = effect.clone();
    let run = ctx.run.clone();
    let saved = value.clone();
    app.db
        .call(move |c| {
            let status = if saved["status"] == "uncertain" {
                "uncertain"
            } else {
                "complete"
            };
            let tx = c.unchecked_transaction()?;
            tx.execute(
                "UPDATE manager_effects SET status=?,result=? WHERE id=? AND status='submitting'",
                params![status, saved.to_string(), id],
            )?;
            wake_detached(&tx, &id)?;
            settle_detached_run(&tx, &run)?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    app.notify.notify_one();
    Ok(value)
}

async fn perform(
    app: &Arc<App>,
    ctx: &Context,
    call: ToolCall,
    effect: &str,
    channel: &str,
) -> Result<Value> {
    let lock = app.project_lock(&ctx.project).await;
    let guard = lock.lock().await;
    let context = ctx.clone();
    let approved_proposal = app
        .db
        .call(move |c| {
            valid(c, &context)?;
            proposal_run(c, &context.run)
        })
        .await?;
    if approved_proposal
        && !matches!(
            call.name.as_str(),
            "delegate" | "worker_status" | "send_md" | "read_file"
        )
    {
        bail!("Tool is unavailable for an approved proposal");
    }
    let p = app.db.project(&ctx.project).await?;
    match call.name.as_str() {
        "read_file" => {
            let a: FileArgs = serde_json::from_value(call.arguments)?;
            read_file(Path::new(&p.path), &a.path)
        }
        "write_file" => {
            let a: WriteArgs = serde_json::from_value(call.arguments)?;
            write_file(Path::new(&p.path), a)
        }
        "delegate" | "steer_worker" => {
            let a: DelegateArgs = serde_json::from_value(call.arguments)?;
            let steer = call.name == "steer_worker";
            ensure!(
                !a.instructions.trim().is_empty() && a.instructions.len() <= 1048576,
                "Worker instructions must contain 1 to 1048576 bytes"
            );
            if steer && (p.agent == "auto" || a.generation.is_some()) {
                ensure!(
                    a.target_cli.is_none(),
                    "Choose an existing generation for steering, not a new CLI"
                );
                let generation = a
                    .generation
                    .context("Choose the running worker generation from worker_status")?;
                let context = ctx.clone();
                let target = generation.clone();
                let task_run = app
                    .db
                    .call(move |c| {
                        let parent = valid(c, &context)?;
                        crate::manager_workers::authorize(c, &parent, &target, true)?;
                        c.query_row(
                            "SELECT id FROM work_runs
                WHERE project_id=? AND generation=? AND status IN ('submitted','running')
                ORDER BY rowid DESC LIMIT 1",
                            params![parent.id, target],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()?
                        .context("No active task is running on the selected worker")
                    })
                    .await?;
                drop(guard);
                return crate::manager_terminal::send(
                    app,
                    ctx,
                    crate::manager_terminal::SendArgs {
                        generation,
                        task_run: Some(task_run),
                        segments: vec![
                            crate::herdr::terminal_control::Segment::Text {
                                text: a.instructions,
                            },
                            crate::herdr::terminal_control::Segment::Key {
                                key: "enter".into(),
                            },
                        ],
                    },
                    effect,
                )
                .await;
            }
            ensure!(
                a.generation.is_none(),
                "Choose a CLI with target_cli when delegating a new task"
            );
            let target_cli = a.target_cli.as_deref();
            let selected = if a.target_cli.is_some() || (!steer && p.agent == "auto") {
                ensure!(
                    !steer,
                    "Choose target_cli on delegate; steer_worker is scoped to the legacy primary conversation"
                );
                match crate::manager_workers::select(app, &p, target_cli.unwrap_or("")).await? {
                    crate::manager_workers::Selection::Target(agent) => Some(agent),
                    crate::manager_workers::Selection::Unavailable(mut result) => {
                        if result["catalog"].is_null() {
                            result["catalog"] = crate::manager_workers::catalog(&p).await;
                        }
                        return Ok(result);
                    }
                }
            } else {
                None
            };
            let context = ctx.clone();
            let request = effect.to_owned();
            let origin_channel = channel.to_owned();
            app.db.call(move|c|{
      let tx=c.unchecked_transaction()?;let p=valid(&tx,&context)?;
      let run_origin=tx.query_row("SELECT origin_event_id FROM manager_runs WHERE id=?", [&context.run], |row| row.get::<_,Option<String>>(0))?;
      let origin = if let Some(requested) = a.source_event_id.as_deref() {
          ensure!(
              !approved_proposal || run_origin.as_deref() == Some(requested),
              "Approved proposal must use its own source_event_id"
          );
          let valid_source: bool = tx.query_row(
              "SELECT EXISTS(SELECT 1 FROM manager_inputs i
               JOIN work_events e ON e.id=i.origin_event_id AND e.project_id=i.project_id
               WHERE i.project_id=? AND i.channel_id=? AND i.origin_event_id=?
               AND i.kind!='worker' AND (i.kind!='proposal' OR ?))",
              params![p.id, origin_channel, requested, approved_proposal],
              |row| row.get(0),
          )?;
          ensure!(
              valid_source,
              "source_event_id must reference a trusted request in this channel"
          );
          Some(requested.to_owned())
      } else {
          run_origin
      };
      let proposal = origin.as_deref().map(|id| work::event(&tx, &p.id, id)).transpose()?
        .filter(|event| event.source == "autonomy");
      let instructions = if let Some(proposal) = &proposal {
          ensure!(!steer && a.target_cli.is_none(), "Approved proposal must use the current worker");
          ensure!(approved_origin(&tx, &p.id, &proposal.id)?, "Approved proposal authority changed");
          if let Some(existing) = tx.query_row(
              &format!("SELECT e.id FROM work_events e WHERE e.project_id=? AND e.source IN ('management','manager_worker')
               AND json_extract(e.payload,'$.origin_event_id')=? AND NOT ({FAILED_BEFORE_SUBMISSION})
               ORDER BY e.rowid LIMIT 1"),
              params![p.id,proposal.id], |row| row.get::<_,String>(0)
          ).optional()? {
              let event = work::event(&tx, &p.id, &existing)?;
              return Ok(json!({"event_id":existing,"status":event.status,"proposal_id":proposal.payload["proposal_id"]}));
          }
          proposal.payload["instructions"].as_str().context("Approved proposal instructions are missing")?
      } else {
          &a.instructions
      };
      if selected.is_some() {
          crate::manager_workers::controls(&tx,&p)?;
      }
      let held=selected.is_none() && crate::cli_switch::latest(&tx,&p.id)?.as_ref().is_some_and(crate::cli_switch::blocks_worker);
      ensure!(!steer||(p.terminal.is_some()&&p.ready),"Worker is not ready; delegate to wake it");
      ensure!(!steer||!held,"Worker steering is held by a CLI transition");
      let id=intent_for_worker(&tx,&p,&request,instructions,origin.as_deref(),steer,selected.as_deref().map(|agent| (agent,None)))?;
      tx.execute("UPDATE work_events SET payload=json_set(payload,'$.origin_channel_id',?) WHERE id=?",params![origin_channel,id])?;
      let event=work::event(&tx,&p.id,&id)?;tx.commit()?;
      Ok(json!({"effect_id":request,"event_id":id,"status":if held{"held"}else{"queued"},"worker_agent":event.payload["expected_agent"],"generation":event.payload["target_generation"],"transition_id":event.payload["transition_id"],"work_queued":true,"assignment_pending":selected.is_some() && event.payload["target_generation"].is_null()}))
    }).await
        }
        "worker_status" => {
            drop(guard);
            ensure!(
                call.arguments
                    .as_object()
                    .is_some_and(|arguments| arguments.is_empty()),
                "worker_status takes no arguments"
            );
            let obsolete = crate::manager_close::inspect(app, &p).await?;
            let authority = ctx.clone();
            let mut snapshot = app
                .db
                .call(move |c| {
                    let current = valid(c, &authority)?;
                    worker_snapshot(c, &current.id, &current.generation)
                })
                .await?;
            snapshot["agent"] = json!(p.agent);
            snapshot["generation"] = json!(p.generation);
            snapshot["native_session"] = json!(p.native_session);
            snapshot["ready"] = json!(p.ready);
            snapshot["paused"] = json!(p.paused);
            snapshot["mcp_registered"] = json!(p.execution["mcp_registered"] == true);
            snapshot["available_clis"] = crate::manager_workers::catalog(&p).await;
            snapshot["obsolete_executions"] = json!(obsolete);
            Ok(snapshot)
        }
        "cli_send_keys" => {
            drop(guard);
            let args: crate::manager_terminal::SendArgs = serde_json::from_value(call.arguments)?;
            crate::manager_terminal::send(app, ctx, args, effect).await
        }
        "cli_view" => {
            drop(guard);
            let args: crate::manager_terminal::ViewArgs = serde_json::from_value(call.arguments)?;
            crate::manager_terminal::view(app, ctx, args, effect).await
        }
        "close_agent" => {
            let args: crate::manager_close::CloseArgs = serde_json::from_value(call.arguments)?;
            crate::manager_close::close(app, &p, ctx, effect, args).await
        }
        "schedule_run" => {
            let mut input: work::TimerInput = serde_json::from_value(call.arguments)?;
            ensure!(
                matches!(input.purpose.as_str(), "user" | "internal"),
                "Choose user or internal schedule purpose"
            );
            if let Some(to) = input.to.as_deref() {
                crate::feishu::recipient_type(to)?;
            }
            input.request_id = Some(effect.to_owned());
            let context = ctx.clone();
            app.db
                .call(move |c| {
                    let tx = c.unchecked_transaction()?;
                    valid(&tx, &context)?;
                    let timer = work::create_timer(&tx, &context.project, input, db::now())?;
                    tx.commit()?;
                    Ok(json!(timer))
                })
                .await
        }
        "send_md" | "send_file" => {
            let (to, text, anchor, path) = if call.name == "send_md" {
                let a: SendText = serde_json::from_value(call.arguments)?;
                (a.to, a.text, a.reply_to_message_id, None)
            } else {
                let a: SendFile = serde_json::from_value(call.arguments)?;
                let path = crate::feishu::allowed_file(&p, Path::new(&a.path), &app.dir)?;
                let root = fs::canonicalize(&p.path)?;
                if let Ok(relative) = path.strip_prefix(&root) {
                    file_parent(&root, &relative.to_string_lossy())?;
                } else {
                    ensure!(
                        path.starts_with(app.dir.join("files").join(&p.id)),
                        "Only workspace files and downloaded attachments are available"
                    );
                }
                (
                    a.to,
                    a.caption.unwrap_or_default(),
                    a.reply_to_message_id,
                    Some(path),
                )
            };
            crate::feishu::recipient_type(&to)?;
            ensure!(
                !text.trim().is_empty() || path.is_some(),
                "Message content is empty"
            );
            ensure!(text.len() <= 131072, "Message is too large");
            let context = ctx.clone();
            let request = effect.to_owned();
            let message = app
                .db
                .call(move |c| {
                    let tx = c.unchecked_transaction()?;
                    let p = valid(&tx, &context)?;
                    if approved_proposal {
                        let origin: String = tx.query_row(
                            "SELECT origin_event_id FROM manager_runs WHERE id=?",
                            [&context.run],
                            |row| row.get(0),
                        )?;
                        let proposal = work::event(&tx, &p.id, &origin)?;
                        ensure!(
                            approved_origin(&tx, &p.id, &origin)?
                                && proposal.payload["to"].as_str() == Some(&to)
                                && proposal.payload["anchor"].as_str() == anchor.as_deref(),
                            "Reply to the approved proposal's destination and anchor"
                        );
                    }
                    let binding =
                        crate::bot::get(&tx, &p.id)?.context("Connect this project's bot first")?;
                    let mut attachment =
                        json!({"to":to,"manager_effect":request,"reply_to_message_id":anchor});
                    if let Some(path) = path {
                        attachment["path"] = json!(path);
                    }
                    let id = db::enqueue_out_request(
                        &tx,
                        &p.id,
                        &binding.app_id,
                        &request,
                        &text,
                        attachment,
                    )?;
                    tx.commit()?;
                    Ok(id)
                })
                .await?;
            drop(guard);
            let send_error = app.send_out(&message).await.err().map(|e| e.to_string());
            let m = app.db.message(&message).await?;
            Ok(
                json!({"message_id":m.attachment["sent_message_id"],"status":m.status,"outbox_id":m.id,"error":m.error,"dispatch_error":send_error}),
            )
        }
        _ => bail!("This management tool is not available"),
    }
}
pub(super) fn detach(c: &Connection, project: &str, channel: &str, id: &str) -> Result<Value> {
    let result: Option<String> = c
        .query_row(
            "SELECT result FROM manager_effects WHERE id=? AND status IN ('complete','uncertain')",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(result) = result {
        return Ok(serde_json::from_str(&result)?);
    }
    c.execute(
        "INSERT OR IGNORE INTO manager_detached_effects(effect_id,project_id,channel_id) VALUES(?,?,?)",
        params![id,project,channel],
    )?;
    Ok(json!({"status":"running_in_background","task_id":id}))
}

pub(super) fn wake_detached(c: &Connection, effect: &str) -> Result<()> {
    c.execute(
        "INSERT OR IGNORE INTO manager_inputs(id,project_id,kind,payload,created_at,channel_id,signal)
         SELECT 'bg-tool:'||e.id,d.project_id,'worker',
            json_object('kind','background_tool_result','task_id',e.id,'tool',e.name,'result',json(e.result)),
            ?,d.channel_id,'wake'
         FROM manager_detached_effects d JOIN manager_effects e ON e.id=d.effect_id
         WHERE e.id=? AND e.status IN ('complete','uncertain')",
        params![db::now(),effect],
    )?;
    Ok(())
}
pub(super) fn recover_unattached_effects(
    c: &Connection,
    context: &Context,
    channel: &str,
) -> Result<()> {
    let effects = c
        .prepare("SELECT id FROM manager_effects WHERE run_id=? AND transcript_recorded=0")?
        .query_map([&context.run], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for effect in effects {
        c.execute(
            "INSERT OR IGNORE INTO manager_detached_effects(effect_id,project_id,channel_id) VALUES(?,?,?)",
            params![effect,context.project,channel],
        )?;
        wake_detached(c, &effect)?;
    }
    Ok(())
}

pub(super) fn settle_detached_run(c: &Connection, run: &str) -> Result<()> {
    c.execute(
        "UPDATE manager_runs SET status='complete'
         WHERE id=? AND status='working' AND json_extract(checkpoint,'$.actor_done')=1
         AND NOT EXISTS(SELECT 1 FROM manager_effects e JOIN manager_detached_effects d ON d.effect_id=e.id
             WHERE e.run_id=manager_runs.id AND e.status NOT IN ('complete','uncertain'))",
        [run],
    )?;
    Ok(())
}

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    let schemas = [
        (
            "Read a bounded UTF-8 project file; returns content and sha256.",
            json!({"path":{"type":"string"}}),
            vec!["path"],
        ),
        (
            "Write a project file. Existing files require their last-read expected_sha256.",
            json!({"path":{"type":"string"},"content":{"type":"string"},"expected_sha256":{"type":"string"}}),
            vec!["path", "content"],
        ),
        (
            "Delegate coding to a native CLI worker in this project; returns a durable receipt. Optional target_cli selects a worker for this task only; optional source_event_id selects a specific incoming platform.event_id from this channel.",
            json!({"instructions":{"type":"string"},"target_cli":{"type":"string","enum":["codex","claude","pi","agy","custom"]},"source_event_id":{"type":"string"}}),
            vec!["instructions"],
        ),
        (
            "Inspect available CLIs and worker status.",
            json!({}),
            vec![],
        ),
        (
            "Steer an active task on its exact worker generation from worker_status. Legacy primary-conversation steering can omit generation. Use delegate with target_cli for separate new work.",
            json!({"instructions":{"type":"string"},"generation":{"type":"string"}}),
            vec!["instructions"],
        ),
        (
            "Send Markdown to a recipient, optionally as a reply.",
            json!({"to":{"type":"string"},"text":{"type":"string"},"reply_to_message_id":{"type":"string"}}),
            vec!["to", "text"],
        ),
        (
            "Send a workspace file or downloaded attachment, optionally as a reply.",
            json!({"to":{"type":"string"},"path":{"type":"string"},"caption":{"type":"string"},"reply_to_message_id":{"type":"string"}}),
            vec!["to", "path"],
        ),
        (
            "Explicitly close an obsolete owned execution selected from worker_status. Never close automatically: verify task completion, give a reason, and retain unknown/current/live/referenced targets. Returns an audited refusal, verified closure or uncertain receipt.",
            json!({"generation":{"type":"string"},"terminal_id":{"type":"string"},"reason":{"type":"string"},"task_finished":{"type":"boolean"}}),
            vec!["generation", "terminal_id", "reason", "task_finished"],
        ),
        (
            "Send an ordered, compact text/key sequence, with each segment exactly {text: string} or {key: string} to the current project's owned CLI. Use generation from worker_status. Text is UTF-8 with LF unchanged; keys are separate segments. Text never implies a key. Keys: enter, tab, esc, backspace, delete, up/down/left/right, home/end, pageup/pagedown, space; ctrl/alt/shift modifiers and modified letters, for example alt+m. Unknown keys fail before sending. Use cli_view to inspect results; a submitted sequence does not prove task completion. Do not repeat uncertain input.",
            json!({"generation":{"type":"string"},"segments":{"type":"array","minItems":1,"maxItems":32,"items":{"oneOf":[{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false},{"type":"object","properties":{"key":{"type":"string"}},"required":["key"],"additionalProperties":false}]}}}),
            vec!["segments"],
        ),
        (
            "Read the current project's owned CLI visible rendered text. This does not send keys, resize, or take over its terminal. Known credential-like lines are redacted. Missing cursor/viewport metadata is explicit; internal execution identifiers are diagnostic, not user-facing task results.",
            json!({"generation":{"type":"string"}}),
            vec![],
        ),
        (
            "Create a project schedule. Use user for work explicitly requested by the user; internal for your own follow-up checks.",
            json!({"name":{"type":"string"},"prompt":{"type":"string"},"kind":{"type":"string","enum":["once","interval","cron"]},"at":{"type":"string"},"interval_seconds":{"type":"integer"},"cron":{"type":"string"},"timezone":{"type":"string"},"to":{"type":"string"},"purpose":{"type":"string","enum":["user","internal"]}}),
            vec!["name", "prompt", "kind", "purpose"],
        ),
    ];
    NAMES.into_iter().zip(schemas).map(|(name,(description,properties,required))|ToolDefinition::new(name.try_into().expect("static tool name"),description,json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}))).collect()
}
