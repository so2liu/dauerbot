SELECT x.id,x.project_id,x.project_name,x.direction,x.chat_id,x.chat_kind,x.source_id,x.app_id,
       x.created_at,x.updated_at,x.activity_at,x.status,x.closure_state,x.closure_reason,x.needs_action,
       x.message_type,x.is_control,x.routing,x.event_id,x.event_status,x.command_phase,x.outbound_kind,
       x.waiting_resource,x.reply_count,x.receipt_count,x.first_reply_ms,
       substr(latest.content,1,240) AS reply_preview,
       COALESCE(substr(m.content,1,512),substr(i.content,1,512),'') AS preview,
       COALESCE(json_extract(m.attachment,'$.source.mentions'),json_extract(i.attachment,'$.source.mentions'),'[]') AS mentions,
       COALESCE(json_extract(m.attachment,'$.source.mention_all'),json_extract(i.attachment,'$.source.mention_all'),0) AS mention_all,
       COALESCE(x.root_id,x.thread_id,json_extract(m.attachment,'$.source.parent_id'),'')!='' OR
           COALESCE(x.thread_id,json_extract(m.attachment,'$.source.parent_id'),'')!='' AS topic_reply,
       COALESCE(json_extract(m.attachment,'$.resources'),json_extract(i.attachment,'$.resources'),
         CASE WHEN json_extract(COALESCE(m.attachment,i.attachment),'$.file_key') IS NOT NULL
         THEN json_array(json_object(
             'file_key',json_extract(COALESCE(m.attachment,i.attachment),'$.file_key'),
             'file_name',json_extract(COALESCE(m.attachment,i.attachment),'$.file_name'),
             'kind',json_extract(COALESCE(m.attachment,i.attachment),'$.kind')
         )) ELSE '[]' END) AS resources,
       (SELECT c.name FROM conversations c WHERE c.chat_id=x.chat_id AND trim(c.name)!=''
        ORDER BY COALESCE(NULLIF(json_extract(c.context,'$.fetched_at'),0),c.seen_at) DESC,c.project_id LIMIT 1) AS chat_name
FROM chosen x
LEFT JOIN messages m ON x.routing=0 AND m.id=x.id
LEFT JOIN messages latest ON latest.id=x.latest_reply
LEFT JOIN feishu_group_intake i ON x.routing=1 AND i.project_id=x.project_id AND i.app_id=x.app_id AND i.source_id=x.source_id
ORDER BY x.activity_at DESC,x.id DESC
