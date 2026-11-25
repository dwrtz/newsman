CREATE INDEX IF NOT EXISTS newsman_due_confirmation_reminder
ON newsman_records(json_extract(body,'$.reminder_due_s'),id)
WHERE kind='subscriber' AND json_extract(body,'$.reminder_due_s') IS NOT NULL;
