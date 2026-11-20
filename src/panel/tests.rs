use super::*;
use crate::{App, api, config::Config, db, work};
use axum::http::StatusCode;

fn seed_project(c: &Connection, id: &str) -> Result<()> {
    c.execute(
        "INSERT INTO projects(id,name,path,agent,chat_id,created_at,mcp_token,generation)
         VALUES(?1,?1,'/isolated-fixture','codex','oc_group',0,?2,'fixture-generation')",
        params![id, db::id()],
    )?;
    Ok(())
}

fn incoming(
    c: &Connection,
    project: &str,
    id: &str,
    body: &str,
    at: i64,
    extra: Value,
) -> Result<()> {
    let mut attachment = json!({
        "bot_app_id": format!("cli_{project}"),
        "source":{"chat_type":"group","chat_id":"oc_group","root_id":"","parent_id":"","thread_id":"",
                  "sender":{"open_id":"ou_operator"},"mentions":[{"key":"@_user_1"}]},
    });
    attachment
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,attachment,created_at,updated_at)
         VALUES(?1,?2,'in',?3,?4,'queued',?5,?6,?6)",
         params![id,project,format!("om_{id}"),body,attachment.to_string(),at])?;
    Ok(())
}

fn outgoing(c: &Connection, project: &str, id: &str, anchor: &str, at: i64) -> Result<()> {
    let attachment = json!({
        "bot_app_id": format!("cli_{project}"),
        "reply_to_message_id":format!("om_{anchor}"),"to":"oc_group","sent_chat_id":"oc_group",
        "sent_message_id":format!("om_sent_{id}"),
    });
    c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,attachment,created_at,updated_at)
        VALUES(?1,?2,'out',?1,'fixture response','sent',?3,?4-100,?4)",
        params![id,project,attachment.to_string(),at])?;
    Ok(())
}

#[test]
fn classifications_use_durable_evidence_and_keep_full_detail() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = db::Db::open(&dir.path().join("panel.sqlite"))?;
    let now = db::now();
    db.sync(|c| {
        for project in ["atlas", "blocked"] { seed_project(c, project)?; }
        incoming(c,"atlas","resource","@_user_1 Can you review this?",now-91_000,json!({
            "file_key":"img_1","file_name":"screen.png","kind":"image","resources_ready":false
        }))?;
        incoming(c,"atlas","queued","Queued question",now-121_000,json!({}))?;
        incoming(c,"atlas","receipt","Receipt only",now-10_000,json!({}))?;
        incoming(c,"atlas","closed","& ".repeat(240).as_str(),now-5000,json!({
            "resources_ready":true,"resources":[{"kind":"image","file_name":"plot.png","bytes":42,
            "width":10,"height":12,"sha256":"fixture-digest","local_path":"/isolated-fixture/plot.png",
            "base64":"private-pixels","cache_source_id":"fixture-cache-key"}]
        }))?;
        incoming(c,"blocked","held","Switch locked",now-1500,json!({}))?;
        incoming(c,"atlas","failed","Previously failed",now-2000,json!({}))?;
        incoming(c,"atlas","native","Native fixture",now-3000,json!({}))?;
        c.execute("UPDATE messages SET status='failed' WHERE id='failed'",[])?;
        work::sync_inbound(c)?;
        let queued: String = c.query_row("SELECT status FROM messages WHERE id='queued'",[],|row|row.get(0))?;
        assert_eq!(queued,"acknowledged");
        c.execute("UPDATE manager_inputs SET created_at=? WHERE origin_event_id='queued'",[now-121_000])?;
        c.execute("UPDATE manager_inputs SET status='consumed' WHERE origin_event_id IN ('receipt','closed')",[])?;
        c.execute("UPDATE work_events SET status='consumed' WHERE message_id IN ('receipt','closed')",[])?;
        c.execute("UPDATE work_events SET status='delivered' WHERE message_id='native'",[])?;
        c.execute("DELETE FROM manager_inputs WHERE origin_event_id='native'",[])?;
        c.execute("INSERT INTO work_runs(id,project_id,task_id,event_id,generation,status,started_at,updated_at,finished_at)
                   VALUES('native-run','atlas','native','native','fixture-generation','settled',?1,?2,?2)",
                   params![now-2500,now-2000])?;
        c.execute("INSERT INTO cli_switch_requests(id,project_id,target,phase,generation,created_at,holds_input)
                   VALUES('switch','blocked','agy','switching','fixture-generation',?,1)",[now])?;
        outgoing(c,"atlas","manager-receipt-atlas-receipt","receipt",now-9000)?;
        outgoing(c,"atlas","attention-false-close","receipt",now-3900)?;
        outgoing(c,"atlas","normal-reply","closed",now-3500)?;
        outgoing(c,"blocked","wrong-project","receipt",now-1000)?;
        c.execute("INSERT INTO feishu_group_intake(project_id,app_id,chat_id,source_id,content,attachment,created_at)
                   VALUES('atlas','cli_atlas','oc_other','om_pending','Pending group','{}',?)",[now-31_000])?;
        let before = c.total_changes();

        let result = messages(c,&Query::default(),now)?;
        let rows = result["items"].as_array().unwrap();
        let find = |id: &str| rows.iter().find(|row|row["id"]==id).unwrap();
        for (id,state,reason) in [
            ("resource","stuck","resource_timeout"),("queued","stuck","manager_queue_timeout"),
            ("receipt","unanswered","receipt_only"),("held","held","cli_switch"),
            ("closed","closed","replied"),("failed","failed_or_uncertain","delivery_failure"),
            ("native","unanswered","no_reply"),("intake:atlas:cli_atlas:om_pending","stuck","routing_timeout"),
        ] {
            assert_eq!(find(id)["closure_state"],state,"row {id}");
            assert_eq!(find(id)["closure_reason"],reason,"row {id}");
        }
        assert_eq!(find("resource")["badges"]["image_count"],1);
        assert_eq!(find("resource")["preview"], "Can you review this?");
        assert_eq!(find("closed")["reply_summary"]["first_reply_ms"],1500);
        assert_eq!(find("receipt")["reply_summary"]["receipt_count"],1);
        assert_eq!(find("receipt")["reply_summary"]["count"],0);
        let panel = overview(c,&HashMap::new(),&Query::default(),now)?;
        assert_eq!(panel["kpis"]["needs_action"],7);
        assert_eq!(panel["kpis"]["needs_action_breakdown"]["stuck_resource"],1);
        assert_eq!(panel["kpis"]["median_first_reply_ms"],1500);
        assert_eq!(panel["kpis"]["median_worker_queue_ms"],500);
        assert_eq!(panel["kpis"]["bot_out_receipt_or_notice"],2);

        let detail = message_detail(c,"closed",now)?.unwrap();
        assert!(detail["content"].as_str().unwrap().chars().count()>240);
        assert!(detail["preview"].as_str().unwrap().chars().count()<=240);
        assert_eq!(detail["source"]["sender"]["open_id"],"ou_operator");
        assert_eq!(detail["resources"][0]["bytes"],42);
        assert_eq!(detail["resources"][0]["width"],10);
        assert!(detail["resources"][0].get("base64").is_none());
        assert!(detail["resources"][0].get("cache_source_id").is_none());
        assert!(detail["timeline"].as_array().unwrap().iter().any(|row|
            row["kind"]=="reply" && row["at_ms"]==now-3500));
        let pending = message_detail(c,"intake:atlas:cli_atlas:om_pending",now)?.unwrap();
        assert_eq!(pending["source"]["chat_id"], "oc_other");
        assert_eq!(pending["timeline"][0]["kind"],"routing");
        assert_eq!(c.total_changes(),before);
        Ok(())
    })
}

#[test]
fn chats_merge_project_history_and_buckets_follow_local_boundaries() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = db::Db::open(&dir.path().join("panel.sqlite"))?;
    // At UTC 17:30, UTC+05:30 is 23:00 on the same local day.
    let now = 1_735_752_600_000_i64;
    db.sync(|c| {
        for project in ["atlas", "blocked", "no-bot"] { seed_project(c,project)?; }
        c.execute_batch("INSERT INTO conversations(project_id,chat_id,kind,name,context,seen_at)
            VALUES('atlas','oc_group','group','Old room','{\"fetched_at\":100}',250),
                  ('blocked','oc_group','group','New room','{\"fetched_at\":200,\"members_status\":\"snapshot\"}',200),
                  ('no-bot','oc_group','group','','{}',300);
            INSERT INTO project_bots(project_id,app_id,app_secret,updated_at)
            VALUES('atlas','cli_atlas','test-only',0),('blocked','cli_blocked','test-only',0);
            INSERT INTO feishu_bot_identity(project_id,app_id,open_id,failures)
            VALUES('atlas','cli_atlas','ou_atlas',0),('blocked','cli_blocked','',1);
            DELETE FROM project_bots WHERE project_id='blocked';")?;
        let runtime = HashMap::from([("atlas".into(),json!({"app_id":"cli_atlas","status":"receiving","error":""}))]);
        for (range,count,width) in [("24h",24,HOUR),("7d",7,24*HOUR),("30d",30,24*HOUR)] {
            let query = Query {range:Some(range.into()),tz_offset_minutes:Some(330),..Query::default()};
            let panel = overview(c,&runtime,&query,now)?;
            let chats = panel["chats"].as_array().unwrap();
            assert_eq!(chats.len(),1);
            assert_eq!(chats[0]["display_name"],"New room");
            assert_eq!(chats[0]["name_missing"],false);
            assert_eq!(chats[0]["members_status"],"snapshot");
            assert_eq!(chats[0]["bots"].as_array().unwrap().len(),3);
            assert_eq!(chats[0]["bots"][0]["ws_status"],"receiving");
            assert_eq!(chats[0]["bots"][0]["identity_status"],"confirmed");
            assert_eq!(chats[0]["bots"][1]["bound"],false);
            assert_eq!(chats[0]["bots"][1]["source_kind"],"configured");
            let buckets = panel["series"].as_array().unwrap();
            assert_eq!(buckets.len(),count);
            for (idx,bucket) in buckets.iter().enumerate() {
                assert_eq!(bucket["user_in"],0);
                assert_eq!(bucket["out_reply"],0);
                assert_eq!(bucket["out_receipt_or_notice"],0);
                let start = bucket["bucket_start_ms"].as_i64().unwrap();
                assert_eq!((start+330*60_000).rem_euclid(width),0);
                assert_eq!(bucket["bucket_end_ms"],start+width);
                if idx>0 {assert_eq!(buckets[idx-1]["bucket_end_ms"],start);}
            }
        }
        let scoped = overview(c,&runtime,&Query {project_id:Some("atlas".into()),..Query::default()},now)?;
        assert_eq!(scoped["chats"][0]["display_name"],"New room");
        assert_eq!(scoped["chats"][0]["bots"].as_array().unwrap().len(),1);
        Ok(())
    })
}

#[test]
fn list_bounds_filter_and_keyset_cursor() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = db::Db::open(&dir.path().join("panel.sqlite"))?;
    let now = db::now();
    db.sync(|c| {
        seed_project(c, "atlas")?;
        for ix in 0..203 {
            incoming(
                c,
                "atlas",
                &format!("item-{ix:03}"),
                "Search text",
                now - 1200,
                json!({}),
            )?;
        }
        let mut query = Query {
            limit: Some(999),
            q: Some("SEARCH".into()),
            ..Query::default()
        };
        let first = messages(c, &query, now)?;
        assert_eq!(first["items"].as_array().unwrap().len(), 200);
        query.before_ms = first["next_before_ms"].as_i64();
        query.before_id = first["next_before_id"].as_str().map(str::to_owned);
        let tail = messages(c, &query, now)?;
        assert_eq!(tail["items"].as_array().unwrap().len(), 3);
        assert_eq!(tail["items"][0]["id"], "item-002");
        assert!(tail["next_before_id"].is_null());
        Ok(())
    })
}

#[tokio::test]
async fn routes_require_local_admin_and_return_allowlisted_data() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let app = App::new(dir.path().join("store"), Config::default())?;
    let now = db::now();
    let project_credential: String = app.db.sync(|c| {
        seed_project(c, "atlas")?;
        incoming(c, "atlas", "one", "One fixture", now - 1000, json!({}))?;
        Ok(c.query_row(
            "SELECT mcp_token FROM projects WHERE id='atlas'",
            [],
            |row| row.get(0),
        )?)
    })?;
    let config_credential = app.config.read().await.token.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let listener_app = app.clone();
    let server =
        tokio::spawn(async move { axum::serve(listener, api::router(listener_app)).await });
    let http = reqwest::Client::builder().no_proxy().build()?;
    for route in ["overview", "messages", "messages/one"] {
        let url = format!("http://{address}/api/panel/{route}");
        assert_eq!(
            http.get(&url).send().await?.status(),
            StatusCode::UNAUTHORIZED
        );
        let mcp = http
            .get(&url)
            .bearer_auth(&project_credential)
            .header("x-hfb-generation", "fixture-generation")
            .send()
            .await?;
        assert_eq!(mcp.status(), StatusCode::UNAUTHORIZED);
        let response = http
            .get(&url)
            .bearer_auth(&config_credential)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        assert_eq!(status, StatusCode::OK, "{route}: {body}");
        let lower = body.to_ascii_lowercase();
        for field in ["secret", "api_key", "token", "checkpoint"] {
            assert!(
                !lower.contains(field),
                "{route} contained reserved field: {field}"
            );
        }
    }
    let missing = http
        .get(format!("http://{address}/api/panel/messages/missing"))
        .bearer_auth(&config_credential)
        .send()
        .await?;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(api::PROTOCOL, 7);
    assert_eq!(
        app.db
            .sync(|c| Ok(c.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?))?,
        7
    );
    server.abort();
    Ok(())
}
