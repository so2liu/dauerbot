use serde_json::Value;

pub(super) type ChannelId = String;

pub(super) const LOCAL: &str = "local:default";

#[derive(Clone, Copy)]
pub(super) enum Signal {
    Steer,
    Wake,
}

impl Signal {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::Wake => "wake",
        }
    }
}

pub(super) fn ingress(source: &str, payload: &Value) -> Option<ChannelId> {
    if source != "feishu" {
        return Some(LOCAL.into());
    }
    let attachment = &payload["attachment"];
    let bot = attachment["bot_app_id"]
        .as_str()
        .filter(|id| !id.is_empty())?;
    let chat = attachment["source"]["chat_id"]
        .as_str()
        .filter(|id| !id.is_empty())?;
    Some(format!("feishu:{bot}:{chat}"))
}

pub(super) fn for_event(
    db: &rusqlite::Connection,
    project: &str,
    source: &str,
    payload: &Value,
) -> anyhow::Result<Option<ChannelId>> {
    use rusqlite::{OptionalExtension, params};
    if source == "timer"
        && let Some(timer) = payload["timer_id"].as_str()
    {
        let origin = db
            .query_row(
                "SELECT json_extract(r.checkpoint,'$.channel_id') FROM work_timers t
             JOIN manager_effects e ON e.id=t.request_id
             JOIN manager_runs r ON r.id=e.run_id AND r.project_id=t.project_id
             WHERE t.id=? AND t.project_id=? AND json_valid(r.checkpoint)",
                params![timer, project],
                |row| row.get::<_, Option<ChannelId>>(0),
            )
            .optional()?
            .flatten();
        if origin.is_some() {
            return Ok(origin);
        }
        if let Some(chat) = payload["to"].as_str().filter(|to| to.starts_with("oc_"))
            && let Some(bot) = crate::bot::get(db, project)?
        {
            return Ok(Some(format!("feishu:{}:{chat}", bot.app_id)));
        }
    }
    if source == "autonomy"
        && let (Some(bot), Some(chat)) = (payload["app_id"].as_str(), payload["to"].as_str())
    {
        return Ok(Some(format!("feishu:{bot}:{chat}")));
    }
    Ok(ingress(source, payload))
}

pub(super) fn bound(
    c: &rusqlite::Connection,
    project: &str,
    channel_id: &str,
) -> anyhow::Result<bool> {
    let Some((bot, _)) = channel_id
        .strip_prefix("feishu:")
        .and_then(|value| value.split_once(':'))
    else {
        return Ok(true);
    };
    Ok(crate::bot::get(c, project)?.is_some_and(|binding| binding.app_id == bot))
}
