CREATE INDEX IF NOT EXISTS newsman_ready_attempt
ON newsman_records(json_extract(body,'$.state'),coalesce(json_extract(body,'$.retry_at'),0),id)
WHERE kind='attempt';

CREATE INDEX IF NOT EXISTS newsman_submitting_lease
ON newsman_records(json_extract(body,'$.lease_until'),id)
WHERE kind='attempt' AND json_extract(body,'$.state')='submitting';

CREATE INDEX IF NOT EXISTS newsman_campaign_attempt
ON newsman_records(json_extract(body,'$.campaign_id'),json_extract(body,'$.state'),id)
WHERE kind='attempt';

CREATE INDEX IF NOT EXISTS newsman_ready_confirmation
ON newsman_records(json_extract(body,'$.state'),id)
WHERE kind='mail_queue';

CREATE INDEX IF NOT EXISTS newsman_due_campaign
ON newsman_records(json_extract(body,'$.state'),unixepoch(json_extract(body,'$.due_at')),id)
WHERE kind='campaign';
