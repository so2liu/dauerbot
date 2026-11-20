SELECT kind,id,status,at_ms FROM (
    SELECT CASE WHEN routing=1 THEN 'routing' WHEN direction='in' THEN 'inbound' ELSE outbound_kind END AS kind,
           id, status, CASE WHEN direction='out' AND status='sent' THEN updated_at ELSE created_at END AS at_ms
    FROM candidate
    UNION ALL SELECT 'worker_event', e.id, e.status, e.created_at FROM event_links e
    UNION ALL SELECT 'worker_run', r.id, r.status, r.started_at FROM event_links e JOIN work_runs r ON r.event_id=e.id
    UNION ALL SELECT 'worker_run',r.id,r.status,r.finished_at FROM event_links e JOIN work_runs r ON r.event_id=e.id WHERE r.finished_at IS NOT NULL
    UNION ALL SELECT 'manager_input',id,status,created_at FROM input_links
    UNION ALL SELECT 'manager_run',r.id,p.status,p.created_at FROM run_links r JOIN manager_runs p ON p.id=r.id
    UNION ALL SELECT 'control',k.id,k.phase,k.updated_at FROM candidate c
      JOIN model_control_commands k ON k.project_id=c.project_id AND k.source_id=c.source_id AND c.is_control
    UNION ALL SELECT kind,id,status,updated_at FROM reply_links
) AS timeline
ORDER BY at_ms,kind,id
