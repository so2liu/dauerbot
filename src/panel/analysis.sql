WITH source AS (
    SELECT m.id,m.project_id,m.direction,m.source_id,m.status,m.created_at,m.updated_at,
        CASE m.direction
            WHEN 'in' THEN COALESCE(json_extract(m.attachment,'$.source.chat_id'),'')
            ELSE COALESCE(NULLIF(json_extract(m.attachment,'$.sent_chat_id'),''),
                          json_extract(m.attachment,'$.to'),'')
        END AS chat_id,
        COALESCE(json_extract(m.attachment,'$.source.chat_type'),'unknown') AS chat_kind,
        COALESCE(json_extract(m.attachment,'$.bot_app_id'),'') AS app_id,
        json_extract(m.attachment,'$.source.sender.open_id') AS sender_id,
        json_extract(m.attachment,'$.source.root_id') AS root_id,
        json_extract(m.attachment,'$.source.thread_id') AS thread_id,
        CASE WHEN m.direction='out' AND json_type(m.attachment,'$.interactive_card')='object'
             THEN 'interactive' ELSE COALESCE(json_extract(m.attachment,'$.source.message_type'),'') END AS message_type,
        json_extract(m.attachment,'$.daemon_control') IS NOT NULL AS is_control,
        COALESCE(json_extract(m.attachment,'$.resources_ready'),0)!=1 AND
            (json_extract(m.attachment,'$.file_key') IS NOT NULL OR
             COALESCE(json_array_length(m.attachment,'$.resources'),0)>0) AS waiting_resource,
        CASE WHEN m.source_id GLOB 'manager-receipt-*' OR
                  json_extract(m.attachment,'$.manager_receipt_origin') IS NOT NULL OR
                  json_extract(m.attachment,'$.discussion_intent')='receipt' THEN 'receipt'
             WHEN m.source_id GLOB 'attention-*' OR
                  json_extract(m.attachment,'$.manager_notice')=1 OR
                  json_extract(m.attachment,'$.manager_provider_revision') IS NOT NULL THEN 'notice'
             ELSE 'reply' END AS outbound_kind,
        0 AS routing
    FROM messages m
    WHERE (:project IS NULL OR m.project_id=:project)
      AND (:id IS NULL OR m.id=:id)
      AND (:start IS NULL OR
           CASE WHEN m.direction='out' AND m.status='sent' THEN m.updated_at ELSE m.created_at END>=:start)
      AND (:include_out=1 OR m.direction='in')
    UNION ALL
    SELECT 'intake:'||i.project_id||':'||i.app_id||':'||i.source_id,i.project_id,'in',i.source_id,
        i.status,i.created_at,i.created_at,i.chat_id,'group',i.app_id,
        json_extract(i.attachment,'$.source.sender.open_id'),
        json_extract(i.attachment,'$.source.root_id'),
        json_extract(i.attachment,'$.source.thread_id'),
        COALESCE(json_extract(i.attachment,'$.source.message_type'),''),0,0,'reply',1
    FROM feishu_group_intake i
    WHERE i.status='pending'
      AND (:project IS NULL OR i.project_id=:project)
      AND (:id IS NULL OR 'intake:'||i.project_id||':'||i.app_id||':'||i.source_id=:id)
      AND (:start IS NULL OR i.created_at>=:start)
), candidate AS MATERIALIZED (
    SELECT s.*,p.name AS project_name,p.paused,p.generation,e.id AS event_id,e.status AS event_status,
           CASE WHEN s.direction='out' AND s.status='sent' THEN s.updated_at ELSE s.created_at END AS activity_at
    FROM source s JOIN projects p ON p.id=s.project_id
    LEFT JOIN work_events e ON s.routing=0 AND s.direction='in' AND e.message_id=s.id
    WHERE (:chat IS NULL OR s.chat_id=:chat)
), delegated AS MATERIALIZED (
    SELECT e.id,e.project_id,e.status,e.created_at,
           json_extract(e.payload,'$.origin_event_id') AS origin
    FROM work_events e WHERE e.source='management'
      AND (:project IS NULL OR e.project_id=:project)
), event_links AS (
    SELECT c.id AS item_id,e.id,e.status,e.created_at,e.project_id
    FROM candidate c JOIN work_events e ON e.id=c.event_id
    UNION ALL
    SELECT c.id,e.id,e.status,e.created_at,e.project_id
    FROM candidate c JOIN delegated e ON e.project_id=c.project_id AND e.origin=c.event_id
), latest_worker_run AS (
    SELECT r.event_id,r.status,r.started_at,
           ROW_NUMBER() OVER (PARTITION BY r.event_id ORDER BY r.rowid DESC) AS rank
    FROM work_runs r JOIN (SELECT DISTINCT id FROM event_links) e ON e.id=r.event_id
), worker_evidence AS (
    SELECT e.item_id,
        MAX(e.status IN ('failed','uncertain') OR COALESCE(r.status IN ('failed','uncertain'),0)) AS failed,
        MAX(e.status IN ('submitting','submitted') OR COALESCE(r.status IN ('submitting','submitted','running'),0)) AS active,
        MAX(e.status='queued' AND e.created_at<:now-120000) AS stale_queue,
        MAX(e.status='queued') AS queued,
        MAX(e.status IN ('delivered','completed','consumed','transferred','cancelled','dismissed') OR
            COALESCE(r.status IN ('settled','completed'),0)) AS ended,
        MIN(CASE WHEN r.started_at>=e.created_at THEN r.started_at-e.created_at END) AS queue_ms
    FROM event_links e LEFT JOIN latest_worker_run r ON r.event_id=e.id AND r.rank=1
    GROUP BY e.item_id
), input_links AS MATERIALIZED (
    SELECT c.id AS item_id,i.id,i.status,i.created_at,c.project_id,c.event_id
    FROM candidate c JOIN manager_inputs i ON i.project_id=c.project_id AND i.origin_event_id=c.event_id
), run_links AS MATERIALIZED (
    SELECT i.item_id, r.id, r.status
    FROM input_links i JOIN manager_run_inputs b ON b.input_id=i.id
    JOIN manager_runs r ON r.id=b.run_id
    UNION
    SELECT c.id,r.id,r.status
    FROM candidate c JOIN manager_runs r ON r.project_id=c.project_id AND r.origin_event_id=c.event_id
), manager_input_evidence AS (
    SELECT item_id,MAX(status='queued' AND created_at<:now-120000) AS stale_queue,
           MAX(status IN ('queued','processing')) AS active,MAX(status='processing') AS processing,
           MAX(status IN ('consumed','transferred','cancelled')) AS ended
    FROM input_links GROUP BY item_id
), manager_run_evidence AS (
    SELECT r.item_id,MAX(r.status='working') AS active,
           MAX(COALESCE(e.status='uncertain',0)) AS uncertain,
           MAX(r.status='complete') AS ended
    FROM run_links r LEFT JOIN manager_effects e ON e.run_id=r.id
    GROUP BY r.item_id
), outbound AS MATERIALIZED (
    SELECT m.id,m.project_id,m.status,m.updated_at,m.created_at,
        COALESCE(json_extract(m.attachment,'$.bot_app_id'),'') AS app_id,
        json_extract(m.attachment,'$.sent_chat_id') AS chat_id,
        json_extract(m.attachment,'$.to') AS to_id,
        COALESCE(json_extract(m.attachment,'$.manager_receipt_origin'),
                 json_extract(m.attachment,'$.discussion_scope')) AS origin,
        COALESCE(json_extract(m.attachment,'$.resolved_reply_to_message_id'),
                 json_extract(m.attachment,'$.reply_to_message_id')) AS anchor,
        json_extract(m.attachment,'$.sent_thread_id') AS thread_id,
        json_extract(m.attachment,'$.sent_root_id') AS root_id,
        CASE WHEN m.source_id GLOB 'manager-receipt-*' OR
                  json_extract(m.attachment,'$.manager_receipt_origin') IS NOT NULL OR
                  json_extract(m.attachment,'$.discussion_intent')='receipt' THEN 'receipt'
             WHEN m.source_id GLOB 'attention-*' OR
                  json_extract(m.attachment,'$.manager_notice')=1 OR
                  json_extract(m.attachment,'$.manager_provider_revision') IS NOT NULL THEN 'notice'
             ELSE 'reply' END AS kind
    FROM messages m
    WHERE m.direction='out' AND (:project IS NULL OR m.project_id=:project)
      AND (:start IS NULL OR m.updated_at>=:start)
), inbound_keys AS (
    SELECT id,project_id,app_id,chat_id,sender_id,chat_kind,created_at,
           'anchor' AS kind,source_id AS topic
    FROM candidate WHERE direction='in' AND routing=0 AND chat_id!=''
    UNION ALL
    SELECT id,project_id,app_id,chat_id,sender_id,chat_kind,created_at,'anchor',root_id
    FROM candidate WHERE direction='in' AND routing=0 AND chat_id!='' AND root_id!=''
    UNION ALL
    SELECT id,project_id,app_id,chat_id,sender_id,chat_kind,created_at,'thread',thread_id
    FROM candidate WHERE direction='in' AND routing=0 AND chat_id!='' AND thread_id!=''
), outbound_keys AS MATERIALIZED (
    SELECT id,project_id,app_id,chat_id,to_id,created_at,'anchor' AS key_kind,anchor AS topic
    FROM outbound WHERE origin IS NULL AND anchor!=''
    UNION ALL
    SELECT id,project_id,app_id,chat_id,to_id,created_at,'anchor',root_id
    FROM outbound WHERE origin IS NULL AND root_id!=''
    UNION ALL
    SELECT id,project_id,app_id,chat_id,to_id,created_at,'thread',thread_id
    FROM outbound WHERE origin IS NULL AND thread_id!=''
), pairs AS (
    SELECT c.id AS item_id,o.id AS out_id,c.created_at
    FROM candidate c JOIN outbound o ON o.project_id=c.project_id AND o.app_id=c.app_id AND o.origin=c.event_id
    WHERE c.direction='in' AND c.routing=0 AND o.created_at>=c.created_at
    UNION
    SELECT i.id,o.id,i.created_at
    FROM inbound_keys i JOIN outbound_keys o
    ON o.project_id=i.project_id AND o.app_id=i.app_id AND o.key_kind=i.kind AND o.topic=i.topic
    WHERE o.created_at>=i.created_at AND
          (o.chat_id=i.chat_id OR o.to_id=i.chat_id OR (i.chat_kind='p2p' AND o.to_id=i.sender_id))
), reply_links AS (
    SELECT p.item_id,o.id,o.kind,o.status,o.updated_at,p.created_at,
           ROW_NUMBER() OVER (
               PARTITION BY p.item_id,(o.kind='reply' AND o.status='sent') ORDER BY o.updated_at DESC,o.id DESC
           ) AS newest
    FROM pairs p JOIN outbound o ON o.id=p.out_id
), reply_evidence AS (
    SELECT item_id,
        COUNT(*) FILTER (WHERE kind='reply' AND status='sent') AS reply_count,
        COUNT(*) FILTER (WHERE kind='receipt' AND status='sent') AS receipt_count,
        MIN(updated_at-created_at) FILTER (WHERE kind='reply' AND status='sent' AND updated_at>=created_at) AS first_reply_ms,
        MAX(id) FILTER (WHERE kind='reply' AND status='sent' AND newest=1) AS latest_reply,
        MAX(kind='reply' AND status IN ('failed','uncertain')) AS failed
    FROM reply_links GROUP BY item_id
), active_projects AS MATERIALIZED (
    SELECT DISTINCT project_id FROM manager_runs WHERE status='working'
), evidence AS (
    SELECT c.*,COALESCE(w.failed,0) AS worker_failed,
        COALESCE(r.uncertain,0) AS manager_uncertain, COALESCE(w.active,0) AS worker_active,
        COALESCE(w.queued,0) AS worker_queued, COALESCE(w.stale_queue,0) AS worker_stale,
        COALESCE(i.active,0) AS input_active,COALESCE(i.processing,0) AS manager_processing,
        COALESCE(i.stale_queue,0) AS manager_stale,COALESCE(r.active,0) AS manager_working,
        NOT(COALESCE(w.active OR w.queued,0) OR COALESCE(i.active,0) OR COALESCE(r.active,0))
          AND (COALESCE(w.ended,0) OR COALESCE(i.ended,0) OR COALESCE(r.ended,0)) AS ended,
        EXISTS(SELECT 1 FROM active_projects a WHERE a.project_id=c.project_id) AS project_working,
        EXISTS(SELECT 1 FROM manager_failures f WHERE f.project_id=c.project_id) AS project_failed,
        EXISTS(SELECT 1 FROM work_leases l WHERE l.project_id=c.project_id AND l.generation=c.generation AND l.active=1) AS takeover,
        EXISTS(SELECT 1 FROM cli_switch_requests s WHERE s.project_id=c.project_id AND s.holds_input=1) AS switching,
        COALESCE(o.reply_count,0) AS reply_count, COALESCE(o.receipt_count,0) AS receipt_count,
        COALESCE(o.failed,0) AS reply_failed, o.first_reply_ms,o.latest_reply,w.queue_ms,
        (SELECT k.phase FROM model_control_commands k WHERE k.project_id=c.project_id AND k.source_id=c.source_id AND c.is_control) AS command_phase
    FROM candidate c LEFT JOIN worker_evidence w ON w.item_id=c.id
        LEFT JOIN manager_input_evidence i ON i.item_id=c.id
        LEFT JOIN manager_run_evidence r ON r.item_id=c.id
        LEFT JOIN reply_evidence o ON o.item_id=c.id
), classified AS (
    SELECT e.*,
        CASE
        WHEN direction='out' THEN 'outbound'
        WHEN status='dismissed' OR event_status='dismissed' THEN 'dismissed'
        WHEN status IN ('failed','uncertain') OR reply_failed OR command_phase='failed' THEN 'delivery_failure'
        WHEN worker_failed THEN 'worker_failure'
        WHEN manager_uncertain OR (project_failed AND NOT ended AND reply_count=0 AND NOT routing AND NOT is_control) THEN 'manager_failure'
        WHEN routing AND created_at<:now-30000 THEN 'routing_timeout'
        WHEN waiting_resource AND created_at<:now-90000 THEN 'resource_timeout'
        WHEN manager_stale AND NOT project_working THEN 'manager_queue_timeout'
        WHEN NOT waiting_resource AND NOT routing AND NOT is_control AND
             (worker_stale OR (event_id IS NULL AND status='queued' AND created_at<:now-120000)) THEN 'native_queue_timeout'
        WHEN NOT ended AND NOT is_control AND switching THEN 'cli_switch'
        WHEN NOT ended AND NOT is_control AND paused THEN 'paused'
        WHEN NOT ended AND NOT is_control AND takeover THEN 'takeover'
        WHEN is_control AND (command_phase='done' OR reply_count>0) THEN 'control'
        WHEN reply_count>0 THEN 'replied'
        WHEN ended AND receipt_count>0 THEN 'receipt_only'
        WHEN ended THEN 'no_reply'
        WHEN routing THEN 'routing_pending'
        WHEN waiting_resource THEN 'preparing_resources'
        WHEN input_active OR manager_working THEN 'manager_work'
        WHEN worker_active OR worker_queued THEN 'worker_work'
        ELSE 'pending_delivery' END AS closure_reason
    FROM evidence e
), states AS (
    SELECT x.*,
        CASE closure_reason
        WHEN 'outbound' THEN NULL
        WHEN 'dismissed' THEN 'control'
        WHEN 'control' THEN 'control'
        WHEN 'delivery_failure' THEN 'failed_or_uncertain'
        WHEN 'worker_failure' THEN 'failed_or_uncertain'
        WHEN 'manager_failure' THEN 'failed_or_uncertain'
        WHEN 'routing_timeout' THEN 'stuck'
        WHEN 'resource_timeout' THEN 'stuck'
        WHEN 'manager_queue_timeout' THEN 'stuck'
        WHEN 'native_queue_timeout' THEN 'stuck'
        WHEN 'cli_switch' THEN 'held'
        WHEN 'paused' THEN 'held'
        WHEN 'takeover' THEN 'held'
        WHEN 'receipt_only' THEN 'unanswered'
        WHEN 'no_reply' THEN 'unanswered'
        WHEN 'replied' THEN 'closed'
        ELSE 'running' END AS closure_state
    FROM classified x
), panel_rows AS (
    SELECT s.*,COALESCE(closure_state IN ('failed_or_uncertain','stuck','held','unanswered'),0) AS needs_action
    FROM states s
)
