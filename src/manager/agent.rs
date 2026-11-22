use super::*;
use rig_core::{
    completion::{CompletionRequest, FinishReason},
    message::Message,
    transcript::{tool_result_message, validate_canonical},
};

struct DebugReply {
    app_id: String,
    chat: String,
    anchor: String,
    input: String,
}

struct Turn {
    context: Context,
    config: ProviderConfig,
    debug: Option<DebugReply>,
}

fn claim(
    c: &Connection,
    project: &str,
    channel_id: &str,
    snapshot: Option<(String, String)>,
) -> Result<Option<Turn>> {
    let tx = c.unchecked_transaction()?;
    let p = db::project_by_key(&tx, project)?;
    if p.paused || !uses_manager(&tx, project)? || work::has_project_takeover(&tx, project)? {
        return Ok(None);
    }
    let provider = tx.query_row(
        &format!("SELECT g.base_url,g.model,g.api_key,g.supports_images,g.provider_kind,g.context_window,m.revision,m.input_revision
         FROM manager_config m CROSS JOIN manager_provider_config g
         WHERE m.project_id=? AND m.next_attempt_at<=? AND ({PROVIDER_CONFIGURED})"),
        params![project, db::now()],
        |r| Ok((r.get::<_, String>(0)?,r.get::<_, String>(1)?,r.get::<_, String>(2)?,r.get::<_, bool>(3)?,r.get::<_, String>(4)?,r.get::<_, Option<u32>>(5)?,r.get::<_, i64>(6)?,r.get::<_, i64>(7)?)),
    ).optional()?;
    let Some((
        base_url,
        model,
        api_key,
        supports_images,
        kind,
        context_window,
        revision,
        input_revision,
    )) = provider
    else {
        return Ok(None);
    };
    // Take newly prepared steering ahead of a proposal or an already claimed
    // batch, so the same queued input cannot interrupt every retry.
    let mut inputs = tx
        .prepare(&format!(
            "SELECT id,origin_event_id,kind,payload FROM manager_inputs
         WHERE project_id=? AND channel_id=? AND status IN ('queued','processing') AND {}
         ORDER BY (status='queued' AND signal='steer') DESC, rowid LIMIT 32",
            prepared_input(),
        ))?
        .query_map(params![project, channel_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if inputs.is_empty() {
        return Ok(None);
    }
    if inputs[0].2 == "proposal" {
        inputs.truncate(1);
    } else if let Some(index) = inputs.iter().position(|input| input.2 == "proposal") {
        inputs.truncate(index);
    }
    session::open(&tx, project, channel_id)?;
    if let Some((digest, memory)) = snapshot {
        session::sync_memory(&tx, project, channel_id, &digest, &memory)?;
    }
    if !channel::bound(&tx, project, channel_id)? {
        return Ok(None);
    }
    let run = db::id();
    let origin = inputs.iter().rev().find_map(|row| row.1.as_deref());
    let checkpoint = json!({"channel_id":channel_id,"target_generation":p.generation});
    tx.execute(
        "INSERT INTO manager_runs(id,project_id,revision,input_revision,origin_event_id,checkpoint,status,owner,created_at)
         VALUES(?,?,?,?,?,?,'working',?,?)",
        params![run,project,revision,input_revision,origin,checkpoint.to_string(),owner(),db::now()],
    )?;
    let debug = if p.debug_mode
        && let Some(origin) = origin
    {
        tx.query_row(
            "SELECT b.app_id,json_extract(m.attachment,'$.source.chat_id'),m.source_id,i.payload
             FROM manager_inputs i JOIN work_events e ON e.id=i.origin_event_id
             JOIN messages m ON m.id=e.message_id AND m.direction='in'
             JOIN project_bots b ON b.project_id=i.project_id
             WHERE i.origin_event_id=? AND i.project_id=? AND i.channel_id=? AND i.kind='user'
             AND e.source='feishu' AND json_extract(m.attachment,'$.bot_app_id')=b.app_id
             AND json_extract(m.attachment,'$.source.chat_id') GLOB 'oc_*' AND m.source_id GLOB 'om_*'
             AND NOT EXISTS(SELECT 1 FROM manager_ingested_inputs x WHERE x.input_id=i.id)",
            params![origin,project,channel_id], |row| {
                let raw: String = row.get(3)?;
                Ok(DebugReply { app_id:row.get(0)?,chat:row.get(1)?,anchor:row.get(2)?,
                    input:crate::manager_context::clean(&raw).0 })
            },
        ).optional()?
    } else {
        None
    };
    append_inputs(&tx, project, channel_id, &run, inputs)?;
    tx.commit()?;
    Ok(Some(Turn {
        context: Context {
            project: project.into(),
            run,
            revision,
            input_revision,
        },
        debug,
        config: ProviderConfig {
            base_url,
            model,
            api_key,
            supports_images,
            provider_kind: serde_json::from_value(json!(kind))?,
            context_window,
        },
    }))
}

type Input = (String, Option<String>, String, String);

fn append_inputs(
    c: &Connection,
    project: &str,
    channel_id: &str,
    run: &str,
    inputs: Vec<Input>,
) -> Result<()> {
    for (id, origin, kind, raw) in inputs {
        let previously_ingested: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM manager_ingested_inputs WHERE input_id=?)",
            [&id],
            |row| row.get(0),
        )?;
        if !previously_ingested {
            let mut data = prompt_input(&raw, origin.as_deref())?;
            let source = origin.as_deref()
                .map(|source| trusted_source(c, project, source))
                .transpose()?
                .unwrap_or(Value::Null);
            attach_quote(c, project, origin.as_deref(), &mut data, &mut 24000)?;
            let images = prepare_images(c, project, origin.as_deref(), &mut data)?;
            let envelope = event::event_envelope(&id, origin.as_deref(), &kind, data, source);
            session::append(c, project, channel_id, &Message::user(envelope), &images)?;
            let seq = c.last_insert_rowid();
            c.execute(
                "INSERT INTO manager_ingested_inputs(input_id,message_seq) VALUES(?,?)",
                params![id, seq],
            )?;
        }
        own_input(c, run, &id)?;
        c.execute(
            "INSERT INTO manager_run_inputs(run_id,input_id) VALUES(?,?)",
            params![run, id],
        )?;
    }
    Ok(())
}

enum Interrupt {
    Steer,
    Closed,
}

async fn interruption(app: &App, context: &Context, channel: &str) -> Interrupt {
    loop {
        tokio::select! {
            _ = app.shutdown.cancelled() => return Interrupt::Closed,
            _ = tokio::time::sleep(Duration::from_millis(50)) => {},
        }
        if app.service.manager_gate.is_closed() {
            return Interrupt::Closed;
        }
        let channel = channel.to_owned();
        let context = context.clone();
        let status = app.db.call(move |c| {
            if valid(c, &context).is_err() {
                return Ok(Some(Interrupt::Closed));
            }
            Ok(c.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM manager_inputs WHERE project_id=? AND channel_id=?
                 AND status='queued' AND signal='steer' AND {})", prepared_input()),
                params![context.project, channel], |row| row.get::<_, bool>(0),
            )?.then_some(Interrupt::Steer))
        }).await;
        match status {
            Ok(Some(event)) => return event,
            Ok(None) => {}
            Err(_) => return Interrupt::Closed,
        }
    }
}

fn telemetry_inputs(
    c: &Connection,
    run: &str,
    includes_content: bool,
) -> Result<Vec<(String, Option<String>)>> {
    Ok(c.prepare(
        "SELECT i.id,CASE WHEN ? THEN json_extract(m.message_json,'$.content[0].text') END
         FROM manager_run_inputs b JOIN manager_inputs i ON i.id=b.input_id
         JOIN manager_ingested_inputs x ON x.input_id=i.id
         LEFT JOIN manager_session_messages m ON m.seq=x.message_seq
         WHERE b.run_id=? ORDER BY i.rowid",
    )?
    .query_map(params![includes_content, run], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?
    .collect::<rusqlite::Result<Vec<_>>>()?)
}

async fn drive(app: Arc<App>, turn: &Turn, channel: &str, workspace: &Path) -> Result<()> {
    let mut trace = app.manager_telemetry.as_ref().map(|exporter| {
        crate::manager_telemetry::Trace::new(
            exporter.clone(),
            &turn.context.project,
            channel,
            &turn.config.api_key,
            Vec::new(),
        )
    });
    let result = drive_turn(&app, turn, channel, workspace, &mut trace).await;
    if let Some(trace) = &mut trace {
        if result.is_err() {
            trace.finish("failed");
        }
        let run = turn.context.run.clone();
        let includes_content = app
            .manager_telemetry
            .as_ref()
            .is_some_and(|e| e.includes_content());
        match app
            .db
            .call(move |c| telemetry_inputs(c, &run, includes_content))
            .await
        {
            Ok(inputs) => trace.set_inputs(inputs),
            Err(_) => tracing::warn!("Manager telemetry input lookup failed"),
        }
    }
    result
}

async fn drive_turn(
    app: &Arc<App>,
    turn: &Turn,
    channel: &str,
    workspace: &Path,
    trace: &mut Option<crate::manager_telemetry::Trace>,
) -> Result<()> {
    let context = &turn.context;
    if let Some(debug) = &turn.debug {
        let echo = crate::feishu::send_debug_reply(
            app,
            &context.project,
            &debug.app_id,
            &debug.chat,
            &debug.anchor,
            &debug.input,
        );
        tokio::select! {
            biased;
            _ = interruption(app, context, channel) => return Ok(()),
            result = echo => {
                if result.is_err() {
                    tracing::warn!("Manager input debug reply failed");
                }
            }
        }
    }
    let cache_key = format!(
        "dauerbot:{:x}",
        Sha256::digest(format!("{}:{channel}", context.project))
    );
    let mut recover_overflow = false;
    let mut index = 0;
    let mut compactions = 0;
    loop {
        let snapshot = memory::snapshot(workspace).await;
        let run = context.clone();
        let key = channel.to_owned();
        let (history, (last_input, last_estimate, cutoff, has_head_memory)) = app
            .db
            .call(move |c| {
                let tx = c.unchecked_transaction()?;
                valid(&tx, &run)?;
                if let Some((digest, memory)) = snapshot {
                    session::sync_memory(&tx, &run.project, &key, &digest, &memory)?;
                }
                if !proposal_run(&tx, &run.run)? {
                    let wake = tx.prepare(
                        "SELECT id,origin_event_id,kind,payload FROM manager_inputs
                         WHERE project_id=? AND channel_id=? AND status='queued' AND signal='wake' AND kind!='proposal'
                         ORDER BY rowid LIMIT 32",
                    )?.query_map(params![run.project,key], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                    })?.collect::<rusqlite::Result<Vec<_>>>()?;
                    append_inputs(&tx, &run.project, &key, &run.run, wake)?;
                }
                let history = session::load(&tx, &run.project, &key)?;
                let last_usage = tx.query_row(
                    "SELECT last_input_tokens,last_estimate,compaction_seq,head_memory IS NOT NULL FROM manager_channels WHERE project_id=? AND channel_id=?",
                    params![run.project,key], |r| Ok((r.get::<_, Option<i64>>(0)?,r.get::<_, Option<i64>>(1)?,r.get::<_, i64>(2)?,r.get::<_, bool>(3)?)),
                )?;
                tx.commit()?;
                Ok((history, last_usage))
            })
            .await?;
        let limit = turn.config.context_limit();
        let reserve = (limit / 5).min(20_000);
        let estimated = history.iter().map(session::estimated_tokens).sum::<u64>() + 2_000;
        let budget_estimate = match (last_input, last_estimate) {
            (Some(input), Some(previous_estimate)) => {
                (input as u64).saturating_add(estimated.saturating_sub(previous_estimate as u64))
            }
            _ => estimated,
        };
        let keep = if recover_overflow {
            (estimated.saturating_sub(2_000) / 2).min(20_000)
        } else {
            (limit / 8).clamp(256, 20_000)
        };
        let head_memory_len = usize::from(has_head_memory);
        let mut summary_budget = limit / 2;
        let prefix_len = head_memory_len + history[head_memory_len..]
            .iter()
            .take_while(|entry| {
                let cost = session::estimated_tokens(entry);
                if let Some(remaining) = summary_budget.checked_sub(cost) {
                    summary_budget = remaining;
                    true
                } else {
                    false
                }
            })
            .count();
        let cut = if compactions < 4
            && (recover_overflow || budget_estimate > limit.saturating_sub(reserve))
        {
            session::compaction_cut(&history, cutoff, keep).map(|cut| {
                if cut <= prefix_len {
                    cut
                } else {
                    session::compaction_cut(
                        &history[..(prefix_len + 1).min(history.len())],
                        cutoff,
                        0,
                    )
                    .unwrap_or(cut)
                }
            })
        } else {
            None
        };
        if let Some(cut) = cut {
            compactions += 1;
            let max_summary_bytes = ((limit / 2) as usize).saturating_mul(3);
            let mut summary = String::from("[");
            let mut remaining = max_summary_bytes;
            for (i, entry) in history[..cut]
                .iter()
                .skip(head_memory_len)
                .enumerate()
            {
                if i > 0 {
                    summary.push(',');
                }
                let raw = serde_json::to_string(&entry.message)?;
                if raw.len() <= remaining {
                    remaining = remaining.saturating_sub(raw.len());
                    summary.push_str(&raw);
                } else {
                    let cap = remaining.clamp(256, 32 * 1024);
                    let end = raw.floor_char_boundary(cap.min(raw.len()));
                    let excerpt = json!({
                        "truncated_for_compaction": true,
                        "original_bytes": raw.len(),
                        "preview": &raw[..end],
                    })
                    .to_string();
                    remaining = remaining.saturating_sub(excerpt.len());
                    summary.push_str(&excerpt);
                }
            }
            summary.push(']');
            let request = CompletionRequest::new(summary)
                .preamble("Summarize the earlier conversation. Keep requests, decisions, progress, and confirmed or uncertain tool outcomes. Preserve active message_id and event_id values, worker generations, and the user's language.")
                .max_tokens((limit / 8).min(4096));
            let mut span = trace.as_mut().map(|trace| {
                let mut span = trace.span("manager.compaction", "generation");
                span.model(&turn.config.model);
                span
            });
            let response = tokio::select! {
                biased;
                _ = interruption(app, context, channel) => return Ok(()),
                result = llm::call(&turn.config, request, None) => result,
            };
            let summary = match response {
                Ok(response) => {
                    let summary =
                        rig_core::transcript::assistant_text_from_choice(&response.choice);
                    if let Some(span) = &mut span {
                        span.response(response.usage);
                        span.finish(if summary.trim().is_empty() {
                            "failed"
                        } else {
                            "ok"
                        });
                    }
                    summary
                }
                Err(error) => {
                    if let Some(span) = &mut span {
                        span.finish("failed");
                    }
                    return Err(error.into());
                }
            };
            drop(span);
            ensure!(
                !summary.trim().is_empty(),
                Failure::invalid_response()
            );
            let cutoff = history[cut - 1].seq;
            let run = context.clone();
            let key = channel.to_owned();
            app.db
                .call(move |c| {
                    valid(c, &run)?;
                    session::compact(c, &run.project, &key, cutoff, &summary)
                })
                .await?;
            recover_overflow = false;
            continue;
        }
        let messages = session::hydrate(history, turn.config.supports_images).await?;
        let mut request = CompletionRequest::new("").tools(tools::definitions());
        request.chat_history = messages;
        request = request.preamble(SYSTEM);
        let mut observation = app.manager_context.capture(
            &context.project,
            &context.run,
            index,
            Some(channel),
            &request,
        );
        let mut span = trace.as_mut().map(|trace| {
            let mut span = trace.span("manager.generation", "generation");
            span.model(&turn.config.model);
            span.request(&request);
            span
        });
        let response = tokio::select! {
            biased;
            _ = interruption(app, context, channel) => return Ok(()),
            result = llm::call(&turn.config, request, Some(&cache_key)) => match result {
                Err(error) if error.kind == "context_rejected" && !recover_overflow && compactions < 4 => {
                    if let Some(span) = &mut span {
                        span.finish("context_rejected");
                    }
                    recover_overflow = true;
                    continue;
                }
                Err(error) => {
                    if let Some(span) = &mut span {
                        span.finish("failed");
                    }
                    return Err(error.into());
                }
                Ok(response) => response,
            },
        };
        index += 1;
        observation.finish("provider_responded");
        let assistant = response.message();
        let complete = response.error.is_none()
            && response.aborted.is_none()
            && !matches!(
                response.finish_reason(),
                Some(FinishReason::Length | FinishReason::ContentFilter | FinishReason::Other(_))
            )
            && assistant.is_some();
        if let Some(mut span) = span.take() {
            span.response(response.usage);
            span.assistant(response.choice.iter());
            span.finish(if complete { "ok" } else { "failed" });
        }
        drop(span);
        recover_overflow = false;
        let last_estimate = i64::try_from(estimated).ok();
        let input_tokens = response
            .usage
            .input_tokens
            .and_then(|tokens| i64::try_from(tokens).ok());
        let run = context.clone();
        let key = channel.to_owned();
        app.db.call(move |c| {
            valid(c, &run)?;
            c.execute("UPDATE manager_channels SET last_input_tokens=?,last_estimate=? WHERE project_id=? AND channel_id=?",
                params![input_tokens,last_estimate,run.project,key])?;
            Ok(())
        }).await?;
        ensure!(complete, Failure::invalid_response());
        let assistant = assistant.ok_or_else(Failure::invalid_response)?;
        let calls = match &assistant {
            Message::Assistant(message) => message.tool_calls().cloned().collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        let ids = calls
            .iter()
            .map(|call| &call.id)
            .collect::<std::collections::BTreeSet<_>>();
        ensure!(ids.len() == calls.len(), Failure::invalid_response());
        let mut results = Vec::with_capacity(calls.len());
        let mut interrupted = false;
        let mut detached = None;
        let mut effects = Vec::new();
        for (offset, call) in calls.iter().enumerate() {
            let name = call.function.name.as_str();
            let result = if interrupted {
                json!({"status":"skipped_due_to_steering"})
            } else if !tools::NAMES.contains(&name) || call.function.invalid_arguments.is_some() {
                json!({"error":"Invalid Manager tool call"})
            } else {
                let Some(permit) = app.service.manager_gate.enter() else {
                    return Ok(());
                };
                let task_app = app.clone();
                let task_context = context.clone();
                let occurrence = format!("turn:{index}:call:{offset}");
                let call = ToolCall {
                    occurrence,
                    name: name.into(),
                    arguments: Value::Object(call.function.arguments.clone()),
                };
                let task_key = channel.to_owned();
                let task_id = format!("manager-{}", db::id());
                let effect_id = task_id.clone();
                let journal_id = task_id.clone();
                let journal_context = context.clone();
                let journal_call = call.clone();
                app.db.call(move |c| {
                    let tx = c.unchecked_transaction()?;
                    valid(&tx, &journal_context)?;
                    tx.execute("INSERT INTO manager_effects(id,run_id,occurrence,name,arguments,status) VALUES(?,?,?,?,?,'submitting')",
                        params![journal_id,journal_context.run,journal_call.occurrence,journal_call.name,journal_call.arguments.to_string()])?;
                    tx.commit()?;
                    Ok(())
                }).await?;
                effects.push(task_id.clone());
                let mut task = tokio::spawn(async move {
                    let _permit = permit;
                    tools::execute(&task_app, &task_context, call, &effect_id, &task_key).await
                });
                tokio::select! {
                    biased;
                    result = &mut task => result??,
                    interrupt = interruption(app, context, channel) => {
                        if matches!(interrupt, Interrupt::Closed) {
                            // Recovery wakes use the journaled arguments because this
                            // tool batch has no canonical checkpoint.
                            return Ok(());
                        }
                        interrupted = true;
                        detached = Some((offset, task_id));
                        Value::Null
                    }
                }
            };
            if let Some(trace) = trace
                && detached.as_ref().is_none_or(|(index, _)| *index != offset)
            {
                trace_tool(trace, call, &result);
            }
            results.push((call.id.clone(), call.function.name.clone(), result));
        }
        let run = context.clone();
        let key = channel.to_owned();
        let finished = calls.is_empty();
        let detached_result = app
            .db
            .call(move |c| {
                let tx = c.unchecked_transaction()?;
                valid(&tx, &run)?;
                let detached_result = if let Some((index, effect)) = detached {
                    let result = tools::detach(&tx, &run.project, &key, &effect)?;
                    results[index].2 = result.clone();
                    Some((index, result))
                } else {
                    None
                };
                let mut batch = vec![assistant];
                if !results.is_empty() {
                    let content = results
                        .into_iter()
                        .map(|(id, name, result)| {
                            let mut part = tool_result_message(id, name, result.to_string());
                            if let rig_core::message::UserContent::ToolResult(value) = &mut part {
                                value.is_error = tool_result_failed(&result);
                            }
                            part
                        })
                        .collect();
                    batch.push(Message::User { content });
                }
                validate_canonical(&batch)?;
                for message in batch {
                    session::append(&tx, &run.project, &key, &message, &[])?;
                }
                for id in effects {
                    tx.execute(
                        "UPDATE manager_effects SET transcript_recorded=1 WHERE id=?",
                        [&id],
                    )?;
                }
                if finished {
                    close_turn(&tx, &run)?;
                    recovered(&tx, &run)?;
                }
                tx.commit()?;
                Ok(detached_result)
            })
            .await?;
        if let (Some(trace), Some((index, result))) = (trace.as_mut(), detached_result) {
            trace_tool(trace, &calls[index], &result);
        }
        if finished || interrupted {
            if let Some(trace) = trace {
                trace.finish(if interrupted { "interrupted" } else { "ok" });
            }
            return Ok(());
        }
    }
}

fn trace_tool(
    trace: &mut crate::manager_telemetry::Trace,
    call: &rig_core::message::ToolCall,
    result: &Value,
) {
    let outcome = match result["status"].as_str() {
        Some("running_in_background") => "running_in_background",
        Some("queued") => "queued",
        Some("held") => "held",
        Some("submitted") => "submitted",
        Some("refused") => "refused",
        Some("worker_unavailable") => "worker_unavailable",
        Some("skipped_due_to_steering") => "skipped_due_to_steering",
        Some("uncertain") => "uncertain",
        _ if tool_result_failed(result) => "failed",
        _ => "ok",
    };
    trace.span("manager.tool", "tool").tool(
        call.function.name.as_str(),
        &Value::Object(call.function.arguments.clone()),
        result,
        outcome,
    );
}

pub(super) async fn run(app: Arc<App>) {
    tokio::spawn(autonomy::run(app.clone()));
    let mut tasks = tokio::task::JoinSet::new();
    let mut running = HashSet::new();
    loop {
        tokio::select! {
            _ = app.shutdown.cancelled() => break,
            joined = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Ok(key)) = joined {
                    running.remove(&key);
                }
            },
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if app.service.manager_gate.is_closed() {
                    continue;
                }
                let channels = app.db.call(|c| {
                    Ok(c.prepare("SELECT DISTINCT i.project_id,i.channel_id FROM manager_inputs i
                      JOIN projects p ON p.id=i.project_id JOIN manager_config m ON m.project_id=p.id
                      LEFT JOIN manager_channels ch ON ch.project_id=i.project_id AND ch.channel_id=i.channel_id
                      WHERE p.paused=0 AND m.next_attempt_at<=?1 AND COALESCE(ch.retry_at,0)<=?1 AND i.status IN ('queued','processing')")?
                      .query_map([db::now()], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?
                      .collect::<rusqlite::Result<Vec<_>>>()?)
                }).await.unwrap_or_default();
                for (project, channel) in channels {
                    if !running.insert((project.clone(), channel.clone())) {
                        continue;
                    }
                    let app = app.clone();
                    tasks.spawn(async move {
                        if let Some(_permit) = app.service.manager_gate.enter() {
                            let _ = run_channel(app, &project, &channel).await;
                        }
                        (project,channel)
                    });
                }
            }
        }
    }
    while tasks.join_next().await.is_some() {}
}

async fn run_channel(app: Arc<App>, project: &str, channel: &str) -> Result<()> {
    let p = app.db.project(project).await?;
    let workspace = Path::new(&p.path);
    prepare_group_context(&app, project, channel).await;
    let snapshot = memory::snapshot(workspace).await;
    let (project, key) = (project.to_owned(), channel.to_owned());
    let Some(turn) = app
        .db
        .call(move |c| {
            if provider_unavailable(c, &project)? {
                return Ok(None);
            }
            claim(c, &project, &key, snapshot)
        })
        .await?
    else {
        return Ok(());
    };
    let result = drive(app.clone(), &turn, channel, workspace).await;
    let context = turn.context;
    let key = channel.to_owned();
    app.db.call(move |c| {
        {
            let tx = c.unchecked_transaction()?;
            tools::recover_unattached_effects(&tx, &context, &key)?;
            tx.commit()?;
        }
        if valid(c, &context).is_err() {
            c.execute("UPDATE manager_runs SET status='superseded' WHERE id=? AND status='working'", [&context.run])?;
            return Ok(());
        }
        {
            if let Err(error) = result {
                let error = error.downcast::<Failure>().unwrap_or_else(|_| Failure::internal());
                failure(c, &context, &error)?;
            } else {
                c.execute("UPDATE manager_runs SET checkpoint=json_set(checkpoint,'$.actor_done',1) WHERE id=? AND status='working'", [&context.run])?;
                tools::settle_detached_run(c, &context.run)?;
            }
        }
        Ok(())
    }).await
}

fn tool_result_failed(result: &Value) -> bool {
    result
        .get("error")
        .is_some_and(|error| error.as_str() != Some(""))
        || result["dispatch_error"].is_string()
        || matches!(
            result["status"].as_str(),
            Some("failed" | "uncertain" | "skipped_due_to_steering")
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
    use tokio::sync::{mpsc, oneshot};

    type Request = (Value, oneshot::Sender<Value>);

    async fn receive(rx: &mut mpsc::UnboundedReceiver<Request>) -> Request {
        tokio::time::timeout(Duration::from_secs(8), rx.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn fixture() -> (
        tempfile::TempDir,
        Arc<App>,
        Project,
        mpsc::UnboundedReceiver<Request>,
        tokio::task::JoinHandle<()>,
    ) {
        let (dir, app, project) = super::super::tests::fixture().await;
        let (tx, rx) = mpsc::unbounded_channel::<Request>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let router = Router::new()
            .route(
                "/v1/chat/completions",
                post(
                    |State(tx): State<mpsc::UnboundedSender<Request>>,
                     Json(request): Json<Value>| async move {
                        let (respond, response) = oneshot::channel();
                        tx.send((request, respond)).unwrap();
                        let response = response.await.unwrap_or(Value::Null);
                        let status = if response.get("error").is_some() {
                            StatusCode::BAD_REQUEST
                        } else {
                            StatusCode::OK
                        };
                        (status, Json(response))
                    },
                ),
            )
            .with_state(tx);
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        app.db
            .sync(|c| {
                c.execute(
                    "UPDATE manager_provider_config SET base_url=? WHERE id=1",
                    [&url],
                )?;
                Ok(())
            })
            .unwrap();
        (dir, app, project, rx, server)
    }

    fn reply(content: Value, calls: Value) -> Value {
        let reason = if calls.is_array() {
            "tool_calls"
        } else {
            "stop"
        };
        json!({"id":"fixture","object":"chat.completion","created":1,"model":"fixture-manager",
            "choices":[{"index":0,"message":{"role":"assistant","content":content,
            "tool_calls":calls},"finish_reason":reason}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}})
    }

    fn ingest(app: &App, project: &Project, chat: &str, id: &str, body: &str) {
        app.db
            .ingest_bot(
                &project.id,
                "cli_fixture",
                chat,
                id,
                body,
                &json!({"source":{"chat_id":chat,"chat_type":"p2p","message_type":"text"}}),
            )
            .unwrap();
        app.db.sync(|c| work::sync_inbound(c).map(|_| ())).unwrap();
    }

    #[tokio::test]
    async fn prepared_steer_interrupts_even_when_queued_before_claim() {
        let (_dir, app, project, _requests, _server) = fixture().await;
        ingest(&app, &project, "oc_a", "om_first", "first");
        ingest(&app, &project, "oc_a", "om_attachment", "attachment");
        let channel = "feishu:cli_fixture:oc_a";
        let turn = app.db.sync(|c| {
            c.execute("UPDATE messages SET attachment=json_set(attachment,'$.file_key','file') WHERE source_id='om_attachment'", [])?;
            Ok(claim(c, &project.id, channel, None)?.unwrap())
        }).unwrap();
        let mut interrupt = Box::pin(interruption(&app, &turn.context, channel));
        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut interrupt)
                .await
                .is_err()
        );
        app.db.sync(|c| {
            c.execute("UPDATE messages SET attachment=json_set(attachment,'$.resources_ready',1) WHERE source_id='om_attachment'", [])?;
            Ok(())
        }).unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), interrupt)
                .await
                .unwrap(),
            Interrupt::Steer
        ));
    }

    #[tokio::test]
    async fn automatic_worker_takeover_fences_admission_until_release_or_retirement() {
        let (_dir, app, project, _requests, _server) = fixture().await;
        ingest(&app, &project, "oc_a", "om_first", "first");
        let channel = "feishu:cli_fixture:oc_a";
        app.db.sync(|c| {
            c.execute("UPDATE projects SET agent='auto',generation='',terminal=NULL WHERE id=?", [&project.id])?;
            c.execute("INSERT INTO manager_workers(generation,project_id,agent,state,created_at,token) VALUES('worker',?,'codex','ready',0,'fixture')", [&project.id])?;
            work::begin_execution(c, &project.id, "worker", 1)?;
            let turn = claim(c, &project.id, channel, None)?.unwrap();
            work::set_takeover(c, &project.id, "worker", true, 2)?;
            assert!(valid(c, &turn.context).is_err());
            assert!(claim(c, &project.id, channel, None)?.is_none());
            work::set_takeover(c, &project.id, "worker", false, 3)?;
            valid(c, &turn.context)?;
            work::set_takeover(c, &project.id, "worker", true, 4)?;
            work::finish_execution(c, &project.id, "worker", "exited", "", 5)?;
            assert!(!work::has_takeover(c, &project.id, "worker")?);
            assert!(claim(c, &project.id, channel, None)?.is_some());
            // A historical binding must not fence its replacement even if an
            // old client left an active lease behind.
            crate::manager_workers::closed(c, "worker")?;
            work::set_takeover(c, &project.id, "worker", true, 6)?;
            crate::manager_workers::controls(c, &db::project_by_key(c, &project.id)?)?;
            Ok(())
        }).unwrap();
    }

    #[tokio::test]
    async fn steer_cancels_unary_request_without_interrupting_another_channel() {
        let (_dir, app, project, mut rx, server) = fixture().await;
        let manager = tokio::spawn(run(app.clone()));
        ingest(&app, &project, "oc_a", "om_a", "first A");
        let (a, original) = receive(&mut rx).await;
        ingest(&app, &project, "oc_b", "om_b", "only B");
        let (b, other) = receive(&mut rx).await;
        assert!(b.to_string().contains("only B"));
        assert!(!b.to_string().contains("first A"));
        ingest(&app, &project, "oc_a", "om_a_steer", "second A");
        let (steered, next) = receive(&mut rx).await;
        assert!(steered.to_string().contains("second A"));
        assert!(!steered.to_string().contains("only B"));
        let blocks = |payload: &Value| {
            payload["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|message| message["content"].as_array().unwrap().iter().cloned())
                .collect::<Vec<_>>()
        };
        let prefix = blocks(&a);
        assert_eq!(&blocks(&steered)[..prefix.len()], prefix);
        assert!(other.send(reply(json!("B answer"), Value::Null)).is_ok());
        assert!(next.send(reply(json!("A answer"), Value::Null)).is_ok());
        let _ = original.send(reply(json!("cancelled answer"), Value::Null));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let count = app
                    .db
                    .sync(|c| {
                        Ok(c.query_row(
                            "SELECT count(*) FROM manager_inputs WHERE status='consumed'",
                            [],
                            |r| r.get::<_, i64>(0),
                        )?)
                    })
                    .unwrap();
                if count == 3 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        app.shutdown.cancel();
        manager.await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn completed_tool_batch_and_latest_idle_memory_are_appended_once() {
        let (_dir, app, project, mut rx, server) = fixture().await;
        let workspace = Path::new(&project.path);
        fs::write(workspace.join("AGENTS.md"), "original memory").unwrap();
        fs::write(workspace.join("file.txt"), "tool evidence").unwrap();
        let manager = tokio::spawn(run(app.clone()));
        ingest(&app, &project, "oc_a", "om_start", "read the fixture");
        let (initial, respond) = receive(&mut rx).await;
        respond
            .send(reply(
                Value::Null,
                json!([{"id":"fixture-call","type":"function",
            "function":{"name":"read_file","arguments":"{\"path\":\"file.txt\"}"}}]),
            ))
            .unwrap();
        let (with_result, respond) = receive(&mut rx).await;
        let original = initial["messages"].as_array().unwrap();
        assert_eq!(
            &with_result["messages"].as_array().unwrap()[..original.len()],
            original
        );
        assert!(with_result.to_string().contains("tool evidence"));
        respond.send(reply(json!("finished"), Value::Null)).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if app
                    .db
                    .sync(|c| {
                        Ok(c.query_row(
                            "SELECT status='consumed' FROM manager_inputs WHERE kind='user'",
                            [],
                            |r| r.get::<_, bool>(0),
                        )?)
                    })
                    .unwrap()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        fs::write(workspace.join("AGENTS.md"), "intermediate idle memory").unwrap();
        fs::write(workspace.join("AGENTS.md"), "latest idle memory").unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(260), rx.recv())
                .await
                .is_err()
        );
        ingest(&app, &project, "oc_a", "om_continue", "continue");
        let (updated, respond) = receive(&mut rx).await;
        let previous = with_result["messages"].as_array().unwrap();
        assert_eq!(
            &updated["messages"].as_array().unwrap()[..previous.len()],
            previous
        );
        let serialized = updated["messages"].to_string();
        assert_eq!(serialized.matches("latest idle memory").count(), 1);
        assert!(!serialized.contains("intermediate idle memory"));
        respond.send(reply(json!("done"), Value::Null)).unwrap();
        app.shutdown.cancel();
        manager.await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn feishu_envelope_is_shared_by_transcript_and_telemetry() -> Result<()> {
        let (_dir, app, project) = super::super::tests::fixture().await;
        let body = "Review <code> & \"quotes\" provider<&credential";
        let id = app.db.ingest_bot(
            &project.id, "cli_fixture", "oc_fixture", "om_0123456789abcdef0123456789abcdef", body,
            &json!({
                "source": {
                    "chat_id": "oc_fixture", "chat_type": "p2p", "message_type": "text",
                    "sender": {"open_id": "ou_fixture", "name": "A & B"},
                    "root_id": "om_root", "thread_id": "omt_fixture", "parent_id": "om_parent",
                    "mention_all": false
                },
                "quoted_message": {"status": "ok", "message_id": "om_parent", "content": "<parent>"},
            }),
        )?.unwrap();
        app.db.sync(|c| {
            work::sync_inbound(c)?;
            route_event(c, &id)?;
            let channel = "feishu:cli_fixture:oc_fixture";
            let Turn { context, .. } = claim(c, &project.id, channel, None)?.unwrap();
            let transcript = session::load(c, &project.id, channel)?;
            let Message::User { content } = &transcript.last().unwrap().message else {
                panic!("expected a user message");
            };
            let Some(rig_core::message::UserContent::Text(text)) = content.iter().next() else {
                panic!("expected an input envelope");
            };
            let xml = &text.text;
            assert!(xml.starts_with(&format!(
                "<dauerbot_event event_id=\"{id}\" kind=\"user\" source=\"feishu\">"
            )));
            for field in [
                "<bot_app_id>cli_fixture</bot_app_id>", "<chat_id>oc_fixture</chat_id>",
                "<message_id>om_0123456789abcdef0123456789abcdef</message_id>", "<root_id>om_root</root_id>",
                "<thread_id>omt_fixture</thread_id>", "<open_id>ou_fixture</open_id>",
                "<name>A &amp; B</name>", "<quoted_message>",
                "<content>&lt;parent&gt;</content>",
                "<body>Review &lt;code&gt; &amp; &quot;quotes&quot; provider&lt;&amp;credential</body>",
            ] {
                assert_eq!(xml.matches(field).count(), 1, "{xml}");
            }
            assert!(!xml.contains("trusted_source") && !xml.contains("platform"));
            let inputs = telemetry_inputs(c, &context.run, true)?;
            assert_eq!(inputs, [(id.clone(), Some(xml.clone()))]);
            assert_eq!(telemetry_inputs(c, &context.run, false)?, [(id.clone(), None)]);
            crate::manager_telemetry::assert_input_export(
                inputs,
                &xml.replace("provider&lt;&amp;credential", "[redacted]"),
            );
            Ok(())
        })
    }

    #[tokio::test]
    async fn detachment_and_recovery_emit_one_durable_wake() {
        let (_dir, app, project) = super::super::tests::fixture().await;
        app.db.sync(|c| {
            let tx = c.unchecked_transaction()?;
            tx.execute("INSERT INTO manager_runs(id,project_id,revision,input_revision,checkpoint,status,owner,created_at)
                VALUES('run',?,1,1,'{\"channel_id\":\"local:default\"}','working','old-host',1)", [&project.id])?;
            tx.execute("INSERT INTO manager_effects(id,run_id,occurrence,name,arguments,status)
                VALUES('effect','run','0','send_md',?,'submitting')",
                [json!({"to":"oc_fixture","text":"fixture message", "api_key":"fixture-secret"}).to_string()])?;
            assert_eq!(tools::detach(&tx, &project.id, channel::LOCAL, "effect")?["status"], "running_in_background");
            tx.commit()?;
            migrate(c, true)?;
            migrate(c, true)?;
            let result: String = c.query_row("SELECT payload FROM manager_inputs WHERE id='bg-tool:effect' AND signal='wake'", [], |r|r.get(0))?;
            let payload: Value = serde_json::from_str(&result)?;
            assert_eq!(payload["result"]["status"], "uncertain");
            let arguments = payload["arguments"].as_str().unwrap();
            assert!(arguments.contains("fixture message"));
            assert!(!arguments.contains("fixture-secret"));
            Ok(())
        }).unwrap();
    }
    #[tokio::test]
    async fn running_send_detaches_and_holds_handover_until_its_single_wake() {
        let (_dir, app, project, mut model_requests, provider) = fixture().await;
        fs::write(Path::new(&project.path).join("result.txt"), "fixture file").unwrap();
        let (tx, mut sends) = mpsc::unbounded_channel::<Request>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        app.config.write().await.feishu_base = format!("http://{}", listener.local_addr().unwrap());
        let send = post(
            |State(tx): State<mpsc::UnboundedSender<Request>>, Json(body): Json<Value>| async move {
                let (respond, response) = oneshot::channel();
                tx.send((body, respond)).unwrap();
                Json(response.await.unwrap())
            },
        );
        let router = Router::new()
            .route(
                "/open-apis/auth/v3/tenant_access_token/internal",
                post(|| async {
                    Json(json!({"code":0,"tenant_access_token":"fixture-token","expire":3600}))
                }),
            )
            .route(
                "/open-apis/im/v1/files",
                post(|| async { Json(json!({"code":0,"data":{"file_key":"fixture-file"}})) }),
            )
            .route("/open-apis/im/v1/messages", send.clone())
            .route("/open-apis/im/v1/messages/om_first/reply", send)
            .with_state(tx);
        let transport = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let manager = tokio::spawn(run(app.clone()));
        ingest(&app, &project, "oc_a", "om_first", "first");
        let (_, respond) = receive(&mut model_requests).await;
        respond.send(reply(Value::Null, json!([
            {"id":"send","type":"function","function":{"name":"send_md","arguments":json!({"to":"oc_a","text":"fixture result"}).to_string()}},
            {"id":"skip","type":"function","function":{"name":"read_file","arguments":json!({"path":"skipped.txt"}).to_string()}}
        ]))).unwrap();
        let (sent, complete_send) = receive(&mut sends).await;
        assert_eq!(sent["receive_id"], "oc_a");
        ingest(&app, &project, "oc_a", "om_steer", "steer");
        let (steered, respond) = receive(&mut model_requests).await;
        let serialized = steered["messages"].to_string();
        assert!(serialized.contains("running_in_background"));
        assert!(serialized.contains("skipped_due_to_steering"));
        respond.send(reply(json!("steered"), Value::Null)).unwrap();
        let gate = app.service.manager_gate.clone();
        let mut drain = tokio::spawn(async move { gate.close_for_test().await.unwrap() });
        assert!(
            tokio::time::timeout(Duration::from_millis(200), &mut drain)
                .await
                .is_err()
        );
        complete_send
            .send(json!({"code":0,"data":{"message_id":"om_sent","chat_id":"oc_a"}}))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), &mut drain)
            .await
            .unwrap()
            .unwrap();
        app.db.sync(|c| {
            let (count, result): (i64,String) = c.query_row(
                "SELECT count(*),COALESCE(MAX(payload),'') FROM manager_inputs WHERE id LIKE 'bg-tool:%'",
                [], |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert_eq!(count, 1);
            assert_eq!(serde_json::from_str::<Value>(&result)?["result"]["message_id"], "om_sent");
            assert_eq!(c.query_row("SELECT count(*) FROM manager_effects WHERE name='read_file'", [], |row| row.get::<_, i64>(0))?, 0);
            Ok(())
        }).unwrap();
        app.service.manager_gate.reopen_for_test();
        let (wake, respond) = receive(&mut model_requests).await;
        assert!(
            wake["messages"]
                .to_string()
                .contains("<background_tool_result>")
        );
        respond.send(reply(Value::Null, json!([
            {"id":"file","type":"function","function":{"name":"send_file","arguments":json!({"to":"oc_a","path":"result.txt","reply_to_message_id":"om_first"}).to_string()}}
        ]))).unwrap();
        let (file, respond) = receive(&mut sends).await;
        assert_eq!(file["reply_in_thread"], true);
        assert_eq!(file["msg_type"], "file");
        assert!(file["content"].as_str().unwrap().contains("fixture-file"));
        respond
            .send(json!({"code":0,"data":{"message_id":"om_file","chat_id":"oc_a","root_id":"om_first","thread_id":"omt_fixture"}}))
            .unwrap();
        let (_, respond) = receive(&mut model_requests).await;
        respond
            .send(reply(json!("completed"), Value::Null))
            .unwrap();
        app.shutdown.cancel();
        manager.await.unwrap();
        app.db
            .sync(|c| {
                let result: String = c.query_row(
                    "SELECT result FROM manager_effects WHERE name='send_file'",
                    [],
                    |row| row.get(0),
                )?;
                let result: Value = serde_json::from_str(&result)?;
                assert_eq!(result["status"], "sent");
                assert_eq!(result["reply_to_message_id"], "om_first");
                assert_eq!(result["root_id"], "om_first");
                assert_eq!(result["thread_id"], "omt_fixture");
                assert!(result.get("error").is_none());
                assert!(!tool_result_failed(&result));
                Ok(())
            })
            .unwrap();
        provider.abort();
        transport.abort();
    }
    #[tokio::test]
    async fn provider_overflow_compacts_preserved_history_then_retries() {
        let (_dir, app, project, mut requests, server) = fixture().await;
        let key = "feishu:cli_fixture:oc_a";
        app.db
            .sync(|c| {
                session::open(c, &project.id, key)?;
                session::append(c, &project.id, key, &Message::user("early question"), &[])?;
                session::append(
                    c,
                    &project.id,
                    key,
                    &Message::assistant("early answer"),
                    &[],
                )?;
                session::append(c, &project.id, key, &Message::user("later question"), &[])?;
                session::append(
                    c,
                    &project.id,
                    key,
                    &Message::assistant("later answer"),
                    &[],
                )?;
                Ok(())
            })
            .unwrap();
        let manager = tokio::spawn(run(app.clone()));
        ingest(&app, &project, "oc_a", "om_overflow", "latest request");
        let (full, respond) = receive(&mut requests).await;
        assert!(full["messages"].to_string().contains("early question"));
        respond.send(json!({"error":{"code":"context_length_exceeded","type":"invalid_request_error","message":"fixture context limit"}})).unwrap();
        let (summary, respond) = receive(&mut requests).await;
        assert!(
            summary["messages"]
                .to_string()
                .contains("Summarize the earlier conversation")
        );
        assert!(summary["messages"].to_string().contains("early question"));
        respond
            .send(reply(json!("earlier fixture summary"), Value::Null))
            .unwrap();
        let (retry, respond) = receive(&mut requests).await;
        let future = retry["messages"].to_string();
        assert!(future.contains("earlier fixture summary"));
        assert!(future.contains("latest request"));
        assert!(!future.contains("early question"));
        respond
            .send(reply(json!("recovered"), Value::Null))
            .unwrap();
        app.shutdown.cancel();
        manager.await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn schema_seven_backfills_old_channels_and_keeps_processing_claims() {
        let (_dir, app, project) = super::super::tests::fixture().await;
        ingest(&app, &project, "oc_a", "om_a", "claimed");
        ingest(&app, &project, "oc_b", "om_b", "queued");
        app.db.sync(|c| {
            let a: String = c.query_row("SELECT id FROM work_events WHERE source_id='om_a'", [], |r| r.get(0))?;
            c.execute("INSERT INTO manager_runs(id,project_id,revision,input_revision,origin_event_id,checkpoint,status,owner,created_at)
                VALUES('legacy',?,1,1,?,json_object('format',1),'working','legacy-host',1)", params![project.id,a])?;
            own_input(c, "legacy", &a)?;
            c.execute("INSERT INTO manager_run_inputs(run_id,input_id) VALUES('legacy',?)", [&a])?;
            c.execute("INSERT INTO manager_effects(id,run_id,occurrence,name,arguments,status)
                VALUES('legacy-effect','legacy','0','send_md','{}','submitting')", [])?;
            c.execute_batch("DROP INDEX manager_inputs_channel;
                ALTER TABLE manager_inputs DROP COLUMN channel_id;
                ALTER TABLE manager_inputs DROP COLUMN signal;
                ALTER TABLE manager_effects DROP COLUMN transcript_recorded;")?;
            let tx = c.unchecked_transaction()?;
            migrate(&tx, true)?;
            tx.commit()?;
            let rows = c.prepare("SELECT status,channel_id,signal FROM manager_inputs WHERE kind='user' ORDER BY rowid")?
                .query_map([], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            assert_eq!(rows, vec![("processing".into(),"feishu:cli_fixture:oc_a".into(),"steer".into()),
                ("queued".into(),"feishu:cli_fixture:oc_b".into(),"steer".into())]);
            let (channel, effect): (String,String) = c.query_row(
                "SELECT i.channel_id,e.status FROM manager_inputs i,manager_effects e
                 WHERE i.id='bg-tool:legacy-effect' AND e.id='legacy-effect'", [], |r| Ok((r.get(0)?,r.get(1)?)),
            )?;
            assert_eq!(channel, "feishu:cli_fixture:oc_a");
            assert_eq!(effect, "uncertain");
            assert_eq!(c.query_row("PRAGMA user_version", [], |r|r.get::<_,i64>(0))?, 7);
            Ok(())
        }).unwrap();
    }

    #[tokio::test]
    async fn delegate_can_select_earlier_channel_source_and_rejects_foreign_source() {
        let (_dir, app, project, mut requests, server) = fixture().await;
        ingest(&app, &project, "oc_a", "om_first", "first topic");
        ingest(&app, &project, "oc_a", "om_second", "second topic");
        ingest(&app, &project, "oc_b", "om_other", "other channel");
        let (first, other): (String, String) = app
            .db
            .sync(|c| {
                let first: String = c.query_row(
                    "SELECT id FROM work_events WHERE source_id='om_first'",
                    [],
                    |r| r.get(0),
                )?;
                let other: String = c.query_row(
                    "SELECT id FROM work_events WHERE source_id='om_other'",
                    [],
                    |r| r.get(0),
                )?;
                c.execute(
                    "UPDATE manager_inputs SET status='consumed' WHERE origin_event_id=?",
                    [&other],
                )?;
                Ok((first, other))
            })
            .unwrap();
        let manager = tokio::spawn(run(app.clone()));
        let (_, respond) = receive(&mut requests).await;
        respond
            .send(reply(
                Value::Null,
                json!([
                    {"id":"bad","type":"function","function":{"name":"delegate","arguments":json!({"instructions":"foreign","source_event_id":other}).to_string()}},
                    {"id":"good","type":"function","function":{"name":"delegate","arguments":json!({"instructions":"for first","source_event_id":first}).to_string()}}
                ]),
            ))
            .unwrap();
        let (follow, respond) = receive(&mut requests).await;
        let payload = follow["messages"].to_string();
        assert!(payload.contains("source_event_id must reference a trusted request in this channel"));
        respond.send(reply(json!("done"), Value::Null)).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let done: i64 = app
                    .db
                    .sync(|c| {
                        Ok(c.query_row(
                            "SELECT count(*) FROM manager_inputs WHERE channel_id='feishu:cli_fixture:oc_a' AND status='consumed'",
                            [],
                            |r| r.get(0),
                        )?)
                    })
                    .unwrap();
                if done == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let origins: Vec<Option<String>> = app
            .db
            .sync(|c| {
                Ok(c.prepare("SELECT json_extract(payload,'$.origin_event_id') FROM work_events WHERE source IN ('management','manager_worker')")?
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .unwrap();
        assert_eq!(origins, vec![Some(first)]);
        app.shutdown.cancel();
        manager.await.unwrap();
        server.abort();
    }
}
