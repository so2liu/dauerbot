//! Admission uses native mentions and existing receipts, never a discussion map.
use crate::{App, bot::Binding, db, feishu};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Accept,
    OwnMention,
    MentionAll,
    Ignore,
    AwaitIdentity,
}
pub(crate) fn migrate(c: &Connection) -> Result<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS feishu_bot_identity(project_id TEXT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,app_id TEXT NOT NULL,open_id TEXT NOT NULL DEFAULT '',updated_at INTEGER NOT NULL DEFAULT 0,next_attempt_at INTEGER NOT NULL DEFAULT 0,failures INTEGER NOT NULL DEFAULT 0,error TEXT NOT NULL DEFAULT ''); CREATE TABLE IF NOT EXISTS feishu_group_intake(project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,app_id TEXT NOT NULL,chat_id TEXT NOT NULL,source_id TEXT NOT NULL,content TEXT NOT NULL,attachment TEXT NOT NULL,created_at INTEGER NOT NULL,status TEXT NOT NULL DEFAULT 'pending',reason TEXT NOT NULL DEFAULT 'identity',PRIMARY KEY(project_id,app_id,source_id));")?;
    Ok(())
}
fn own_id(c: &Connection, project: &str, app: &str) -> Result<Option<String>> {
    Ok(c.query_row(
        "SELECT open_id FROM feishu_bot_identity WHERE project_id=? AND app_id=? AND open_id!=''",
        params![project, app],
        |r| r.get(0),
    )
    .optional()?)
}
fn mention_id(mention: &Value) -> &str {
    mention["id"]["open_id"]
        .as_str()
        .or_else(|| mention["id"].as_str())
        .unwrap_or("")
}
pub(crate) fn mention_all(kind: &str, content: &Value, mentions: &Value) -> bool {
    if mentions.as_array().is_some_and(|all| {
        all.iter()
            .any(|m| m["key"] == "@_all" || mention_id(m) == "all" || m["id"]["user_id"] == "all")
    }) {
        return true;
    }
    // Official larksuite/node-sdk b188be60134d409b61259546bcf69c731d2b0b7c,
    // channel/normalize/index.ts: native @all may have no mentions entry;
    // normalize the protocol's reserved @_all placeholder, not a display name.
    fn reserved_all(s: &str) -> bool {
        s.match_indices("@_all").any(|(at, _)| {
            s.as_bytes()
                .get(at + 5)
                .is_none_or(|next| !next.is_ascii_alphanumeric() && *next != b'_')
        })
    }
    if kind == "text" {
        return content["text"].as_str().is_some_and(reserved_all);
    }
    fn post(v: &Value) -> bool {
        match v {
            Value::Array(a) => a.iter().any(post),
            Value::Object(o) => {
                (v["tag"] == "at" && v["user_id"] == "all")
                    || (v["tag"] == "text" && v["text"].as_str().is_some_and(reserved_all))
                    || o.values().any(post)
            }
            _ => false,
        }
    }
    kind == "post" && post(content)
}
fn associations(c: &Connection, sql: &str, chat: &str, key: &str) -> Result<Vec<String>> {
    if key.is_empty() {
        return Ok(vec![]);
    }
    Ok(c.prepare(sql)?
        .query_map(params![chat, key], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}
pub(crate) fn classify(
    c: &Connection,
    project: &str,
    app: &str,
    chat: &str,
    attachment: &Value,
) -> Result<Decision> {
    let source = &attachment["source"];
    if source["chat_type"] != "group" {
        return Ok(Decision::Accept);
    }
    ensure!(source["chat_id"] == chat, "Native group metadata changed");
    if source["mention_all"] == true {
        return Ok(Decision::MentionAll);
    }
    let mentions = source["mentions"].as_array();
    let own = own_id(c, project, app)?;
    if own
        .as_ref()
        .is_some_and(|id| mentions.is_some_and(|all| all.iter().any(|m| mention_id(m) == id)))
    {
        return Ok(Decision::OwnMention);
    }
    // Open IDs are app-scoped. The receiving app can compare only its own ID;
    // native MentionEvent.mentioned_type identifies other bots vs. people.
    // Official larksuite/oapi-sdk-go v3.13.0 service/im/v1/model.go:5247.
    if mentions.is_some_and(|all| all.iter().any(|m| m["mentioned_type"] != "user")) {
        return Ok(if own.is_none() {
            Decision::AwaitIdentity
        } else {
            Decision::Ignore
        });
    }
    Ok(if owns_topic(c, project, chat, source)? {
        Decision::Accept
    } else {
        Decision::Ignore
    })
}

fn owns_topic(c: &Connection, project: &str, chat: &str, source: &Value) -> Result<bool> {
    let parent = source["parent_id"].as_str().unwrap_or("");
    let strongest = associations(
        c,
        "SELECT DISTINCT m.project_id FROM messages m JOIN project_bots b ON b.project_id=m.project_id AND b.app_id=json_extract(m.attachment,'$.bot_app_id') WHERE m.direction='out' AND m.status='sent' AND (json_extract(m.attachment,'$.sent_chat_id')=?1 OR json_extract(m.attachment,'$.to')=?1) AND json_extract(m.attachment,'$.sent_message_id')=?2 LIMIT 2",
        chat,
        parent,
    )?;
    if !strongest.is_empty() {
        return Ok(strongest.len() == 1 && strongest[0] == project);
    }
    let thread = source["thread_id"].as_str().unwrap_or("");
    let root = source["root_id"].as_str().unwrap_or("");
    if thread.is_empty() && root.is_empty() && parent.is_empty() {
        return Ok(false);
    }
    // A broadcast is not first-responder ownership, even if only one bot has
    // answered so far. Exact sent-message parent proof above can disambiguate it.
    let broadcast:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM messages m JOIN project_bots b ON b.project_id=m.project_id AND b.app_id=json_extract(m.attachment,'$.bot_app_id') WHERE m.direction='in' AND json_extract(m.attachment,'$.source.chat_id')=?1 AND ((?2!='' AND m.source_id=?2) OR (?3!='' AND json_extract(m.attachment,'$.source.thread_id')=?3)) AND (json_extract(m.attachment,'$.source.mention_all')=1 OR json_extract(m.attachment,'$.source.group_admission')='mention_all' OR (json_type(m.attachment,'$.source.mention_all') IS NULL AND (m.content || ' ') GLOB '*@_all[^a-zA-Z0-9_]*')))",params![chat,root,thread],|r|r.get(0))?;
    if broadcast {
        return Ok(false);
    }
    let rows=c.prepare("SELECT DISTINCT m.project_id FROM messages m JOIN project_bots b ON b.project_id=m.project_id AND b.app_id=json_extract(m.attachment,'$.bot_app_id') WHERE (m.direction='out' AND m.status='sent' AND (json_extract(m.attachment,'$.sent_chat_id')=?1 OR json_extract(m.attachment,'$.to')=?1) AND ((?2!='' AND json_extract(m.attachment,'$.sent_thread_id')=?2) OR (?3!='' AND (json_extract(m.attachment,'$.sent_root_id')=?3 OR json_extract(m.attachment,'$.sent_message_id')=?3)))) OR (m.direction='in' AND json_extract(m.attachment,'$.source.chat_id')=?1 AND (json_extract(m.attachment,'$.source.group_admission')='own_mention' OR EXISTS(SELECT 1 FROM feishu_bot_identity i,json_each(m.attachment,'$.source.mentions') mention WHERE i.project_id=b.project_id AND i.app_id=b.app_id AND i.open_id!='' AND json_extract(mention.value,'$.id.open_id')=i.open_id)) AND ((?2!='' AND json_extract(m.attachment,'$.source.thread_id')=?2) OR (?3!='' AND m.source_id=?3) OR (?4!='' AND m.source_id=?4))) LIMIT 2")?
 .query_map(params![chat,thread,root,parent],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.len() == 1 && rows[0] == project)
}
async fn resolve(app: Arc<App>, binding: Binding) -> Result<()> {
    let cfg = app.config.read().await.clone();
    let token = feishu::token(&app, &binding).await?;
    let v = feishu::api_json(
        app.http
            .get(format!("{}/open-apis/bot/v3/info", cfg.feishu_base))
            .bearer_auth(token)
            .timeout(Duration::from_secs(3))
            .send()
            .await?,
    )
    .await?;
    let open = v["bot"]["open_id"]
        .as_str()
        .filter(|id| id.starts_with("ou_") && id.len() <= 200)
        .context("Feishu did not return this bot's Open ID")?
        .to_owned();
    let Some(_permit) = app.service.gate.enter() else {
        return Ok(());
    };
    app.db.call(move|c|{
  let current=crate::bot::get(c,&binding.project_id)?.context("Bot was disconnected")?;
  if current.app_id!=binding.app_id || current.app_secret!=binding.app_secret {return Ok(());}
  c.execute("INSERT INTO feishu_bot_identity(project_id,app_id,open_id,updated_at,next_attempt_at,failures,error) VALUES(?,?,?,?,?,0,'') ON CONFLICT(project_id) DO UPDATE SET app_id=excluded.app_id,open_id=excluded.open_id,updated_at=excluded.updated_at,next_attempt_at=excluded.next_attempt_at,failures=0,error=''",params![binding.project_id,binding.app_id,open,db::now(),db::now()+86_400_000])?;Ok(())
 }).await
}
async fn retry_waiting(app: &App) -> Result<()> {
    let rows=app.db.call(|c|Ok(c.prepare("SELECT m.project_id,m.app_id,m.chat_id,m.source_id,m.content,m.attachment FROM feishu_group_intake m JOIN project_bots b ON b.project_id=m.project_id AND b.app_id=m.app_id JOIN feishu_bot_identity i ON i.project_id=b.project_id AND i.app_id=b.app_id AND i.open_id!='' WHERE m.status='pending' ORDER BY m.created_at,m.rowid LIMIT 128")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)).await?;
    let mut changed = false;
    for (project, app_id, chat, source, content, attachment) in rows {
        let value: Value = serde_json::from_str(&attachment)?;
        changed |= app
            .db
            .ingest_routed_bot(&project, &app_id, &chat, &source, &content, &value)?
            .is_some();
    }
    if changed {
        app.quote_notify.notify_one();
        app.notify.notify_one();
    }
    Ok(())
}
pub(crate) async fn run(app: Arc<App>) {
    loop {
        if app.shutdown.is_cancelled() {
            return;
        }
        let Some(_permit) = app.service.gate.enter() else {
            tokio::select! {_=app.shutdown.cancelled()=>return,_=tokio::time::sleep(Duration::from_millis(100))=>continue}
        };
        drop(_permit);
        if let Ok(bindings) = app.db.bots().await {
            let mut tasks = tokio::task::JoinSet::new();
            for binding in bindings {
                let b = binding.clone();
                let due=app.db.call(move|c|Ok(c.query_row("SELECT next_attempt_at FROM feishu_bot_identity WHERE project_id=? AND app_id=?",params![b.project_id,b.app_id],|r|r.get::<_,i64>(0)).optional()?.unwrap_or(0)<=db::now())).await.unwrap_or(false);
                if due {
                    let worker = app.clone();
                    tasks.spawn(async move {
     let result=tokio::time::timeout(Duration::from_secs(5),resolve(worker.clone(),binding.clone())).await;
     if !matches!(result,Ok(Ok(()))) {
      let error=match result {Ok(Err(error))=>error.to_string(),_=>"Bot identity lookup timed out".into()};
      let Some(_permit)=worker.service.gate.enter() else{return;};
      let _=worker.db.call(move|c|{
       if crate::bot::get(c,&binding.project_id)?.is_none_or(|b|b.app_id!=binding.app_id || b.app_secret!=binding.app_secret){return Ok(());}
       c.execute("INSERT INTO feishu_bot_identity(project_id,app_id,failures,error,next_attempt_at) VALUES(?,?,1,?,?) ON CONFLICT(project_id) DO UPDATE SET app_id=excluded.app_id,open_id=CASE WHEN app_id=excluded.app_id THEN open_id ELSE '' END,failures=CASE WHEN app_id=excluded.app_id THEN failures+1 ELSE 1 END,error=excluded.error,next_attempt_at=?+(min(60,5*(1 << min(failures,4)))*1000)",params![binding.project_id,binding.app_id,error,db::now()+5000,db::now()])?;Ok(())
      }).await;
     }
    });
                    if tasks.len() >= 4 {
                        let _ = tasks.join_next().await;
                    }
                }
            }
            while tasks.join_next().await.is_some() {}
        }
        if let Some(_permit) = app.service.gate.enter() {
            if let Err(error) = retry_waiting(&app).await {
                tracing::warn!("Group routing retry: {error}");
            }
            if let Ok(states) = app
                .db
                .call(|c| {
                    Ok(
                        c.prepare("SELECT project_id,open_id,error FROM feishu_bot_identity")?
                            .query_map([], |r| {
                                Ok((
                                    r.get::<_, String>(0)?,
                                    r.get::<_, String>(1)?,
                                    r.get::<_, String>(2)?,
                                ))
                            })?
                            .collect::<rusqlite::Result<Vec<_>>>()?,
                    )
                })
                .await
            {
                let mut runtime = app.runtime.write().await;
                for (project, id, error) in states {
                    runtime.bots.entry(project).or_insert(json!({}))["group_routing"] = json!({"status":if id.is_empty(){"retrying_identity"}else{"ready"},"error":error});
                }
            }
        }
        tokio::select! {_=app.shutdown.cancelled()=>return,_=tokio::time::sleep(Duration::from_secs(1))=>{}}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, db::ProjectInput, work};
    async fn app() -> (tempfile::TempDir, Arc<App>) {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(dir.path().join("store"), Config::default()).unwrap();
        (dir, app)
    }
    async fn project(app: &App, name: &str, openid: Option<&str>) -> Binding {
        let p = app
            .db
            .create(ProjectInput {
                name: name.into(),
                path: app.dir.to_string_lossy().into(),
                agent: "codex".into(),
                chat_id: String::new(),
                native_session: String::new(),
                extra_args: vec![],
            })
            .await
            .unwrap();
        let app_id = format!("cli_{name}");
        app.db.sync(|c|{crate::bot::save(c,&p.id,&app_id,"fixture-secret",false)?;if let Some(id)=openid{c.execute("INSERT INTO feishu_bot_identity(project_id,app_id,open_id,updated_at,next_attempt_at) VALUES(?,?,?,?,?)",params![p.id,app_id,id,db::now(),db::now()+86_400_000])?;}Ok(())}).unwrap();
        app.db.bot(&p.id).await.unwrap()
    }
    #[allow(clippy::too_many_arguments)]
    fn event(
        bot: &Binding,
        id: &str,
        chat_type: &str,
        mentions: Value,
        thread: &str,
        root: &str,
        parent: &str,
        text: &str,
    ) -> Value {
        json!({"header":{"app_id":bot.app_id,"event_type":"im.message.receive_v1"},"event":{"sender":{"sender_type":"user","sender_id":{"open_id":"ou_user"}},"message":{"message_id":id,"chat_id":"oc_group","chat_type":chat_type,"message_type":"text","content":json!({"text":text}).to_string(),"mentions":mentions,"thread_id":thread,"root_id":root,"parent_id":parent}}})
    }
    fn ingest(app: &Arc<App>, bot: &Binding, event: &Value) {
        feishu::ingest(app, bot, &serde_json::to_vec(event).unwrap()).unwrap();
    }
    fn status(app: &App, bot: &Binding, id: &str) -> Option<String> {
        app.db.sync(|c| {
            let status:Option<String>=c.query_row("SELECT status FROM messages WHERE project_id=? AND direction='in' AND source_id=?",params![bot.project_id,id],|r|r.get(0)).optional()?;
            if status.is_some(){return Ok(status);}
            Ok(c.query_row("SELECT CASE status WHEN 'pending' THEN 'routing' ELSE status END FROM feishu_group_intake WHERE project_id=? AND app_id=? AND source_id=?",params![bot.project_id,bot.app_id,id],|r|r.get(0)).optional()?)
        }).unwrap()
    }
    fn sent(app: &App, bot: &Binding, id: &str, thread: &str, root: &str, status: &str) {
        app.db.sync(|c|{c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,attachment,created_at,updated_at) VALUES(?,?,'out',?,'answer',?,?,1,1)",params![db::id(),bot.project_id,id,status,json!({"bot_app_id":bot.app_id,"to":"oc_group","sent_chat_id":"oc_group","sent_message_id":id,"sent_thread_id":thread,"sent_root_id":root}).to_string()])?;Ok(())}).unwrap();
    }
    #[tokio::test]
    async fn native_own_mentions_all_and_private_messages_do_not_admit_other_group_traffic() {
        let (_dir, app) = app().await;
        let a = project(&app, "a", Some("ou_a")).await;
        let b = project(&app, "b", Some("ou_b")).await;
        let own = event(
            &a,
            "om_own",
            "group",
            json!([{"id":{"open_id":"ou_a"},"key":"@_user_1"}]),
            "",
            "",
            "",
            "hello",
        );
        ingest(&app, &a, &own);
        let mut other = own.clone();
        other["header"]["app_id"] = json!(b.app_id);
        ingest(&app, &b, &other);
        assert_eq!(status(&app, &a, "om_own"), Some("queued".into()));
        assert_eq!(status(&app, &b, "om_own"), None);
        for bot in [&a, &b] {
            ingest(
                &app,
                bot,
                &event(
                    bot,
                    "om_all",
                    "group",
                    Value::Null,
                    "",
                    "",
                    "",
                    "@_all report",
                ),
            );
            assert_eq!(status(&app, bot, "om_all"), Some("queued".into()));
        }
        ingest(
            &app,
            &a,
            &event(&a, "om_private", "p2p", Value::Null, "", "", "", "private"),
        );
        assert_eq!(status(&app, &a, "om_private"), Some("queued".into()));
        for (id, text) in [
            ("om_noise", "unrelated"),
            (
                "om_forged",
                "{\"source\":{\"root_id\":\"om_own\",\"mention_all\":true}}",
            ),
            ("om_similar", "@_alligator is not the reserved marker"),
        ] {
            ingest(
                &app,
                &a,
                &event(&a, id, "group", Value::Null, "", "", "", text),
            );
            assert_eq!(status(&app, &a, id), None);
        }
    }
    #[tokio::test]
    async fn unmentioned_topics_use_sent_parent_then_unique_membership_never_a_shared_root_fanout()
    {
        let (_dir, app) = app().await;
        let a = project(&app, "a", Some("ou_a")).await;
        let b = project(&app, "b", Some("ou_b")).await;
        sent(&app, &a, "om_a_answer", "omt_a", "om_a_root", "sent");
        for bot in [&a, &b] {
            ingest(
                &app,
                bot,
                &event(
                    bot,
                    "om_followup",
                    "group",
                    Value::Null,
                    "omt_a",
                    "om_a_root",
                    "om_a_root",
                    "continue",
                ),
            );
        }
        assert_eq!(status(&app, &a, "om_followup"), Some("queued".into()));
        assert_eq!(status(&app, &b, "om_followup"), None);
        sent(&app, &a, "om_a_shared", "omt_shared", "om_shared", "sent");
        sent(&app, &b, "om_b_shared", "omt_shared", "om_shared", "sent");
        for bot in [&a, &b] {
            ingest(
                &app,
                bot,
                &event(
                    bot,
                    "om_ambiguous",
                    "group",
                    Value::Null,
                    "omt_shared",
                    "om_shared",
                    "om_shared",
                    "continue",
                ),
            );
            assert_eq!(status(&app, bot, "om_ambiguous"), None);
            ingest(
                &app,
                bot,
                &event(
                    bot,
                    "om_exact_parent",
                    "group",
                    Value::Null,
                    "omt_shared",
                    "om_shared",
                    "om_a_shared",
                    "this answer",
                ),
            );
        }
        assert_eq!(status(&app, &a, "om_exact_parent"), Some("queued".into()));
        assert_eq!(status(&app, &b, "om_exact_parent"), None);
        sent(
            &app,
            &a,
            "om_uncertain_answer",
            "omt_uncertain",
            "om_uncertain",
            "uncertain",
        );
        ingest(
            &app,
            &a,
            &event(
                &a,
                "om_unproven",
                "group",
                Value::Null,
                "omt_uncertain",
                "om_uncertain",
                "om_uncertain",
                "continue",
            ),
        );
        assert_eq!(status(&app, &a, "om_unproven"), None);
        let forged = event(
            &a,
            "om_wrong_chat",
            "group",
            Value::Null,
            "omt_a",
            "om_a_root",
            "om_a_answer",
            "continue",
        );
        let mut wrong = forged.clone();
        wrong["event"]["message"]["chat_id"] = json!("oc_other");
        ingest(&app, &a, &wrong);
        assert_eq!(status(&app, &a, "om_wrong_chat"), None);
    }
    #[tokio::test]
    async fn verified_initial_mention_allows_followup_but_legacy_unaddressed_rows_do_not_bootstrap_ownership()
     {
        let (_dir, app) = app().await;
        let a = project(&app, "a", Some("ou_a")).await;
        let own = event(
            &a,
            "om_initial",
            "group",
            json!([{"id":{"open_id":"ou_a"}}]),
            "",
            "",
            "",
            "initial",
        );
        ingest(&app, &a, &own);
        let follow = event(
            &a,
            "om_early_followup",
            "group",
            Value::Null,
            "omt_new",
            "om_initial",
            "om_initial",
            "before reply",
        );
        ingest(&app, &a, &follow);
        ingest(&app, &a, &follow);
        assert_eq!(status(&app, &a, "om_early_followup"), Some("queued".into()));
        let normalized = feishu::normalize(&event(
            &a,
            "om_legacy",
            "group",
            Value::Null,
            "",
            "",
            "",
            "legacy",
        ))
        .unwrap()
        .unwrap();
        app.db
            .ingest_bot(
                &a.project_id,
                &a.app_id,
                &normalized.0,
                &normalized.1,
                &normalized.2,
                &normalized.3,
            )
            .unwrap();
        ingest(
            &app,
            &a,
            &event(
                &a,
                "om_legacy_child",
                "group",
                Value::Null,
                "omt_legacy",
                "om_legacy",
                "om_legacy",
                "continue",
            ),
        );
        assert_eq!(status(&app, &a, "om_legacy_child"), None);
        app.db.sync(|c|{assert_eq!(c.query_row("SELECT count(*) FROM messages WHERE direction='in' AND source_id='om_early_followup'",[],|r|r.get::<_,i64>(0))?,1);c.execute("INSERT INTO cli_switch_requests(id,project_id,target,phase,generation,created_at,holds_input) VALUES('held',?,'pi','failed','old',1,1)",[&a.project_id])?;work::sync_inbound(c)?;assert!(crate::cli_switch::holds_input(c,&a.project_id)?);assert!(work::claim_next(c,&a.project_id,"old",db::now())?.is_none());Ok(())}).unwrap();
    }
    #[tokio::test]
    async fn identity_deferred_events_never_become_work_and_retry_without_blocking_known_topics_or_private_input()
     {
        let (_dir, app) = app().await;
        let a = project(&app, "a", None).await;
        let revision=app.db.sync(|c|{c.execute("INSERT INTO idle_handoffs(id,project_id,generation,status,baseline,native_seq,requested_at) VALUES('idle',?,'idle-generation','requested','baseline',1,1)",[&a.project_id])?;crate::cli_switch::revision(c,&a.project_id)}).unwrap();
        let own = event(
            &a,
            "om_delayed",
            "group",
            json!([{"id":{"open_id":"ou_a"}}]),
            "",
            "",
            "",
            "hello",
        );
        ingest(&app, &a, &own);
        let foreign = event(
            &a,
            "om_other_bot",
            "group",
            json!([{"id":{"open_id":"ou_other"}}]),
            "",
            "",
            "",
            "other",
        );
        ingest(&app, &a, &foreign);
        assert_eq!(status(&app, &a, "om_delayed"), Some("routing".into()));
        assert!(
            app.db.messages(&a.project_id).await.unwrap().is_empty(),
            "Unadmitted content is not ordinary history"
        );
        app.db
            .sync(|c| {
                assert_eq!(crate::cli_switch::revision(c, &a.project_id)?, revision);
                assert_eq!(
                    c.query_row(
                        "SELECT status FROM idle_handoffs WHERE id='idle'",
                        [],
                        |r| r.get::<_, String>(0)
                    )?,
                    "requested"
                );
                Ok(())
            })
            .unwrap();
        app.db
            .sync(|c| {
                assert_eq!(work::sync_inbound(c)?, 0);
                Ok(())
            })
            .unwrap();
        ingest(
            &app,
            &a,
            &event(
                &a,
                "om_private_ok",
                "p2p",
                Value::Null,
                "",
                "",
                "",
                "continue privately",
            ),
        );
        sent(&app, &a, "om_known", "omt_known", "om_root", "sent");
        ingest(
            &app,
            &a,
            &event(
                &a,
                "om_topic_ok",
                "group",
                Value::Null,
                "omt_known",
                "om_root",
                "om_known",
                "continue topic",
            ),
        );
        assert_eq!(status(&app, &a, "om_private_ok"), Some("queued".into()));
        assert_eq!(status(&app, &a, "om_topic_ok"), Some("queued".into()));
        app.db
            .sync(|c| {
                c.execute(
                    "INSERT INTO feishu_bot_identity(project_id,app_id,open_id) VALUES(?,?,'ou_a')",
                    params![a.project_id, a.app_id],
                )?;
                Ok(())
            })
            .unwrap();
        retry_waiting(&app).await.unwrap();
        retry_waiting(&app).await.unwrap();
        assert_eq!(status(&app, &a, "om_delayed"), Some("queued".into()));
        assert_eq!(status(&app, &a, "om_other_bot"), None);
        app.db
            .sync(|c| {
                assert_eq!(
                    c.query_row(
                        "SELECT count(*) FROM messages WHERE source_id='om_delayed'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    1
                );
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn human_mentions_in_owned_topics_and_legacy_native_mentions_keep_their_context() {
        let (_dir, app) = app().await;
        let a = project(&app, "a", Some("ou_a")).await;
        let legacy = feishu::normalize(&event(
            &a,
            "om_old_own",
            "group",
            json!([{"id":{"open_id":"ou_a"}}]),
            "",
            "",
            "",
            "old explicit mention",
        ))
        .unwrap()
        .unwrap();
        app.db
            .ingest_bot(
                &a.project_id,
                &a.app_id,
                &legacy.0,
                &legacy.1,
                &legacy.2,
                &legacy.3,
            )
            .unwrap();
        ingest(
            &app,
            &a,
            &event(
                &a,
                "om_human",
                "group",
                json!([{"id":{"open_id":"ou_human"},"mentioned_type":"user"}]),
                "omt_old",
                "om_old_own",
                "om_old_own",
                "coordinate with this person",
            ),
        );
        assert_eq!(status(&app, &a, "om_human"), Some("queued".into()));
        sent(&app, &a, "om_reply", "omt_verified", "om_verified", "sent");
        app.db
            .sync(|c| {
                c.execute(
                    "DELETE FROM feishu_bot_identity WHERE project_id=?",
                    [&a.project_id],
                )?;
                Ok(())
            })
            .unwrap();
        ingest(
            &app,
            &a,
            &event(
                &a,
                "om_human_without_identity",
                "group",
                json!([{"id":{"open_id":"ou_human"},"mentioned_type":"user"}]),
                "omt_verified",
                "om_verified",
                "om_reply",
                "continue with this person",
            ),
        );
        assert_eq!(
            status(&app, &a, "om_human_without_identity"),
            Some("queued".into())
        );
        ingest(
            &app,
            &a,
            &event(
                &a,
                "om_plain_without_identity",
                "group",
                Value::Null,
                "omt_verified",
                "om_verified",
                "om_reply",
                "plain topic remains immediate",
            ),
        );
        assert_eq!(
            status(&app, &a, "om_plain_without_identity"),
            Some("queued".into())
        );
        app.db
            .sync(|c| {
                c.execute(
                    "INSERT INTO feishu_bot_identity(project_id,app_id,open_id) VALUES(?,?,'ou_a')",
                    params![a.project_id, a.app_id],
                )?;
                Ok(())
            })
            .unwrap();
        retry_waiting(&app).await.unwrap();
        assert_eq!(
            status(&app, &a, "om_human_without_identity"),
            Some("queued".into())
        );
    }
    #[tokio::test]
    async fn unresolved_identity_backlog_cannot_starve_another_bots_ready_routing() {
        let (_dir, app) = app().await;
        let a = project(&app, "a", None).await;
        let b = project(&app, "b", None).await;
        for index in 0..130 {
            ingest(
                &app,
                &a,
                &event(
                    &a,
                    &format!("om_unknown_{index}"),
                    "group",
                    json!([{"id":{"open_id":"ou_a"}}]),
                    "",
                    "",
                    "",
                    "pending identity",
                ),
            );
        }
        ingest(
            &app,
            &b,
            &event(
                &b,
                "om_b_ready",
                "group",
                json!([{"id":{"open_id":"ou_b"}}]),
                "",
                "",
                "",
                "pending identity",
            ),
        );
        app.db
            .sync(|c| {
                c.execute(
                    "INSERT INTO feishu_bot_identity(project_id,app_id,open_id) VALUES(?,?,'ou_b')",
                    params![b.project_id, b.app_id],
                )?;
                Ok(())
            })
            .unwrap();
        retry_waiting(&app).await.unwrap();
        assert_eq!(status(&app, &b, "om_b_ready"), Some("queued".into()));
        assert_eq!(status(&app, &a, "om_unknown_0"), Some("routing".into()));
    }
    #[tokio::test]
    async fn failed_native_identity_lookup_retries_without_losing_group_or_private_control() {
        use axum::{
            Json, Router,
            extract::State,
            routing::{get, post},
        };
        use std::sync::atomic::{AtomicBool, Ordering};
        async fn info(State(healthy): State<Arc<AtomicBool>>) -> Json<Value> {
            if healthy.load(Ordering::SeqCst) {
                Json(json!({"code":0,"bot":{"open_id":"ou_a"}}))
            } else {
                Json(json!({"code":999,"msg":"fixture identity unavailable"}))
            }
        }
        let healthy = Arc::new(AtomicBool::new(false));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router=Router::new().route("/open-apis/bot/v3/info",get(info)).route("/open-apis/auth/v3/tenant_access_token/internal",post(||async{Json(json!({"code":0,"tenant_access_token":"private-fixture-token","expire":7200}))})).with_state(healthy.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let (_dir, app) = app().await;
        app.config.write().await.feishu_base = format!("http://{address}");
        let a = project(&app, "a", None).await;
        ingest(
            &app,
            &a,
            &event(
                &a,
                "om_group_delayed",
                "group",
                json!([{"id":{"open_id":"ou_a"}}]),
                "",
                "",
                "",
                "control",
            ),
        );
        ingest(
            &app,
            &a,
            &event(
                &a,
                "om_private_control",
                "p2p",
                Value::Null,
                "",
                "",
                "",
                "control privately",
            ),
        );
        app.service.gate.close_for_test().await.unwrap();
        let worker = tokio::spawn(run(app.clone()));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !worker.is_finished(),
            "A temporarily closed handover gate must not permanently stop identity recovery"
        );
        app.service.gate.reopen_for_test();
        for _ in 0..100 {
            let failed=app.db.sync(|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM feishu_bot_identity WHERE project_id=? AND failures>0 AND error!='' AND next_attempt_at>?)",params![a.project_id,db::now()],|r|r.get::<_,bool>(0))?)).unwrap();
            if failed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        app.db
            .sync(|c| {
                assert!(c.query_row(
                    "SELECT failures>0 AND error!='' FROM feishu_bot_identity WHERE project_id=?",
                    [&a.project_id],
                    |r| r.get::<_, bool>(0)
                )?);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            status(&app, &a, "om_private_control"),
            Some("queued".into())
        );
        assert_eq!(status(&app, &a, "om_group_delayed"), Some("routing".into()));
        healthy.store(true, Ordering::SeqCst);
        app.db
            .sync(|c| {
                c.execute(
                    "UPDATE feishu_bot_identity SET next_attempt_at=0 WHERE project_id=?",
                    [&a.project_id],
                )?;
                Ok(())
            })
            .unwrap();
        for _ in 0..200 {
            if status(&app, &a, "om_group_delayed") == Some("queued".into()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(status(&app, &a, "om_group_delayed"), Some("queued".into()));
        app.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap();
        server.abort();
        let _ = server.await;
    }
    #[tokio::test]
    async fn verified_user_stop_tombstones_unadmitted_input_without_changing_ordinary_queue_or_recovery()
     {
        let (_dir, app) = app().await;
        let a = project(&app, "a", None).await;
        let pending = event(
            &a,
            "om_before_stop",
            "group",
            json!([{"id":{"open_id":"ou_a"}}]),
            "",
            "",
            "",
            "before stop",
        );
        ingest(&app, &a, &pending);
        ingest(
            &app,
            &a,
            &event(
                &a,
                "om_ordinary_pending",
                "p2p",
                Value::Null,
                "",
                "",
                "",
                "ordinary pending",
            ),
        );
        app.stop(&a.project_id).await.unwrap();
        app.db
            .sync(|c| {
                c.execute(
                    "INSERT INTO feishu_bot_identity(project_id,app_id,open_id) VALUES(?,?,'ou_a')",
                    params![a.project_id, a.app_id],
                )?;
                Ok(())
            })
            .unwrap();
        retry_waiting(&app).await.unwrap();
        ingest(&app, &a, &pending);
        assert_eq!(status(&app, &a, "om_before_stop"), Some("cancelled".into()));
        assert_eq!(
            status(&app, &a, "om_ordinary_pending"),
            Some("acknowledged".into())
        );
        ingest(
            &app,
            &a,
            &event(
                &a,
                "om_after_stop",
                "group",
                json!([{"id":{"open_id":"ou_a"}}]),
                "",
                "",
                "",
                "new instruction",
            ),
        );
        assert_eq!(status(&app, &a, "om_after_stop"), Some("queued".into()));
        app.db
            .sync(|c| {
                assert!(!c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM messages WHERE source_id='om_before_stop')",
                    [],
                    |r| r.get::<_, bool>(0)
                )?);
                c.execute(
                    "DELETE FROM feishu_bot_identity WHERE project_id=?",
                    [&a.project_id],
                )?;
                Ok(())
            })
            .unwrap();
        ingest(
            &app,
            &a,
            &event(
                &a,
                "om_before_recovery",
                "group",
                json!([{"id":{"open_id":"ou_a"}}]),
                "",
                "",
                "",
                "preserve during runtime recovery",
            ),
        );
        let p = app.db.project(&a.project_id).await.unwrap();
        app.stop_inner_mode(&p, false).await.unwrap();
        assert_eq!(
            status(&app, &a, "om_before_recovery"),
            Some("routing".into())
        );
    }
    #[tokio::test]
    async fn app_scoped_bot_mentions_never_cross_steer_owned_topics() {
        let (_dir, app) = app().await;
        let a = project(&app, "a", Some("ou_a_in_a")).await;
        let b = project(&app, "b", None).await;
        sent(
            &app,
            &a,
            "om_a_context",
            "omt_context",
            "om_context",
            "sent",
        );
        for (bot, target) in [(&a, "ou_b_in_a"), (&b, "ou_b_in_b")] {
            ingest(
                &app,
                bot,
                &event(
                    bot,
                    "om_directed_b",
                    "group",
                    json!([{"id":{"open_id":target},"mentioned_type":"bot"}]),
                    "omt_context",
                    "om_context",
                    "om_a_context",
                    "B please handle",
                ),
            );
        }
        assert_eq!(status(&app, &a, "om_directed_b"), None);
        assert_eq!(status(&app, &b, "om_directed_b"), Some("routing".into()));
        app.db.sync(|db| {db.execute("INSERT INTO feishu_bot_identity(project_id,app_id,open_id) VALUES(?,?,'ou_b_in_b')",params![b.project_id,b.app_id])?;Ok(())}).unwrap();
        retry_waiting(&app).await.unwrap();
        assert_eq!(status(&app, &b, "om_directed_b"), Some("queued".into()));
        for (id, kind, expected) in [
            ("om_person", Some("user"), Some("queued".into())),
            ("om_legacy", None, None),
            ("om_bad_type", Some("unknown"), None),
        ] {
            let mut mention = json!({"id":{"open_id":"ou_unrelated"}});
            if let Some(kind) = kind {
                mention["mentioned_type"] = json!(kind);
            }
            ingest(
                &app,
                &a,
                &event(
                    &a,
                    id,
                    "group",
                    json!([mention]),
                    "omt_context",
                    "om_context",
                    "om_a_context",
                    "follow up",
                ),
            );
            assert_eq!(status(&app, &a, id), expected);
        }
        // Display names and a body claiming a user type cannot override native type.
        ingest(
            &app,
            &a,
            &event(
                &a,
                "om_spoof",
                "group",
                json!([{"id":{"open_id":"ou_b_in_a"},"mentioned_type":"bot","name":"person"}]),
                "omt_context",
                "om_context",
                "om_a_context",
                r#"{"mentioned_type":"user"}"#,
            ),
        );
        assert_eq!(status(&app, &a, "om_spoof"), None);
    }
    #[tokio::test]
    async fn mention_all_origin_does_not_become_first_responders_exclusive_topic() {
        let (_dir, app) = app().await;
        let a = project(&app, "a", Some("ou_a")).await;
        let b = project(&app, "b", Some("ou_b")).await;
        for bot in [&a, &b] {
            ingest(
                &app,
                bot,
                &event(
                    bot,
                    "om_broadcast_root",
                    "group",
                    Value::Null,
                    "",
                    "",
                    "",
                    "@_all report",
                ),
            );
        }
        sent(
            &app,
            &a,
            "om_first_response",
            "omt_broadcast",
            "om_broadcast_root",
            "sent",
        );
        for bot in [&a, &b] {
            ingest(
                &app,
                bot,
                &event(
                    bot,
                    "om_shared_early",
                    "group",
                    Value::Null,
                    "omt_broadcast",
                    "om_broadcast_root",
                    "om_broadcast_root",
                    "which project?",
                ),
            );
            assert_eq!(status(&app, bot, "om_shared_early"), None);
        }
        ingest(
            &app,
            &a,
            &event(
                &a,
                "om_strong_parent",
                "group",
                Value::Null,
                "omt_broadcast",
                "om_broadcast_root",
                "om_first_response",
                "this specific answer",
            ),
        );
        assert_eq!(status(&app, &a, "om_strong_parent"), Some("queued".into()));
        // Legacy native text keeps its reserved marker grammar; SQL wildcards
        // must not turn unrelated words into a broadcast veto.
        for (id, text) in [
            ("om_legacy_plain", "an @mall or @_alligator example"),
            ("om_legacy_all", "@_all legacy report"),
        ] {
            let normalized =
                feishu::normalize(&event(&a, id, "group", Value::Null, "", "", "", text))
                    .unwrap()
                    .unwrap();
            let mut attachment = normalized.3;
            attachment["source"]
                .as_object_mut()
                .unwrap()
                .remove("mention_all");
            app.db
                .ingest_bot(
                    &a.project_id,
                    &a.app_id,
                    &normalized.0,
                    &normalized.1,
                    &normalized.2,
                    &attachment,
                )
                .unwrap();
            sent(
                &app,
                &a,
                &format!("{id}_reply"),
                &format!("omt_{id}"),
                id,
                "sent",
            );
            let child = format!("{id}_child");
            ingest(
                &app,
                &a,
                &event(
                    &a,
                    &child,
                    "group",
                    Value::Null,
                    &format!("omt_{id}"),
                    id,
                    id,
                    "continue",
                ),
            );
            assert_eq!(status(&app, &a, &child).is_some(), id == "om_legacy_plain");
        }
    }
}
