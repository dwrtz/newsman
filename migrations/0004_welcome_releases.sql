CREATE INDEX IF NOT EXISTS newsman_welcome_releases
ON newsman_records(json_extract(body,'$.start') DESC,id DESC)
WHERE kind='welcome_release';
