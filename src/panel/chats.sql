WITH observed AS (
    SELECT project_id,chat_id,seen_at,2 AS source_rank FROM conversations WHERE chat_id!=''
    UNION ALL
    SELECT id,chat_id,0,3 FROM projects WHERE chat_id!=''
    UNION ALL
    SELECT project_id,COALESCE(NULLIF(json_extract(attachment,'$.sent_chat_id'),''),
           json_extract(attachment,'$.to'),''),MAX(updated_at),1
    FROM messages WHERE direction='out' AND status='sent'
    GROUP BY project_id,2
    UNION ALL
    SELECT project_id,chat_id,MAX(created_at),2 FROM feishu_group_intake
    WHERE status='pending' GROUP BY project_id,chat_id
), membership AS (
    SELECT project_id,chat_id,MAX(seen_at) AS last_seen_at,MAX(source_rank) AS source_rank
    FROM observed WHERE
        (chat_id GLOB 'oc_*' OR chat_id GLOB 'ou_*')
        AND (:project IS NULL OR project_id=:project)
        AND (:chat IS NULL OR chat_id=:chat)
    GROUP BY project_id,chat_id
), labels AS (
    SELECT DISTINCT chat_id FROM membership
), grouping AS (
    SELECT g.chat_id,
        (SELECT c.name FROM conversations c WHERE c.chat_id=g.chat_id AND trim(c.name)!=''
         ORDER BY COALESCE(NULLIF(json_extract(c.context,'$.fetched_at'),0),c.seen_at) DESC,c.project_id LIMIT 1) AS name,
        COALESCE(
          (SELECT c.kind FROM conversations c WHERE c.chat_id=g.chat_id AND c.kind IN ('group','p2p')
           ORDER BY c.seen_at DESC,c.project_id LIMIT 1),
          CASE WHEN g.chat_id GLOB 'ou_*' THEN 'p2p'
               WHEN EXISTS(SELECT 1 FROM feishu_group_intake i WHERE i.chat_id=g.chat_id AND i.status='pending') THEN 'group'
               ELSE 'unknown' END) AS kind,
        COALESCE((SELECT json_extract(c.context,'$.members_status')
                  FROM conversations c WHERE c.chat_id=g.chat_id AND json_extract(c.context,'$.members_status') IS NOT NULL
                  ORDER BY COALESCE(json_extract(c.context,'$.fetched_at'),c.seen_at) DESC,c.project_id LIMIT 1),'unavailable') AS members_status
    FROM labels g
)
SELECT m.chat_id,m.project_id,m.last_seen_at,p.name AS project_name,p.agent,b.app_id,
       g.name,g.kind,g.members_status,
       COALESCE(i.open_id!='',0) AS identity_confirmed,
       COALESCE(i.failures>0,0) AS identity_failed,
       CASE m.source_rank WHEN 3 THEN 'configured' WHEN 2 THEN 'inbound' ELSE 'outbound_only' END AS source_kind
FROM membership m JOIN projects p ON p.id=m.project_id
JOIN grouping g ON g.chat_id=m.chat_id
LEFT JOIN project_bots b ON b.project_id=p.id
LEFT JOIN feishu_bot_identity i ON i.project_id=p.id AND i.app_id=b.app_id
ORDER BY p.name,p.id
