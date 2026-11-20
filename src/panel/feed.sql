, chosen AS MATERIALIZED (
    SELECT s.* FROM panel_rows s
    LEFT JOIN messages m ON s.routing=0 AND m.id=s.id
    LEFT JOIN feishu_group_intake i ON s.routing=1 AND i.project_id=s.project_id AND i.app_id=s.app_id AND i.source_id=s.source_id
    WHERE (NOT :needs OR s.needs_action)
      AND (:bucket_end IS NULL OR s.activity_at<:bucket_end)
      AND s.activity_at<=:now
      AND (:before IS NULL OR s.activity_at<:before OR (s.activity_at=:before AND s.id<:before_id))
      AND (:search IS NULL OR instr(lower(COALESCE(m.content,i.content,'')),:search)>0
           OR instr(lower(s.project_name),:search)>0
           OR EXISTS(SELECT 1 FROM conversations c WHERE c.chat_id=s.chat_id AND instr(lower(c.name),:search)>0))
    ORDER BY s.activity_at DESC,s.id DESC
    LIMIT :limit
)
