//! The private edition offered to a newly confirmed reader. A campaign's own
//! snapshot is authoritative. A previously released edition can be pinned at handoff.
use crate::core::{Res, Tx, conf, req_str, uid};
use crate::{delivery, editorial};
use chrono::DateTime;
use serde_json::{Value, json};

fn enabled(tx: &Tx<'_>) -> bool {
    conf(tx.env, "WELCOME_CURRENT_ISSUE").as_deref() == Some("true")
}

fn timestamp(v: &Value) -> Option<i64> {
    DateTime::parse_from_rfc3339(v.as_str()?)
        .ok()
        .map(|t| t.timestamp())
}

fn window(source: &Value, send_at: i64) -> Option<(i64, i64)> {
    let edition = timestamp(&source["edition_at"])?;
    let start = edition.max(send_at);
    let end = (edition + 7 * 86400).min(timestamp(&source["archive_at"]).unwrap_or(i64::MAX));
    (start < end).then_some((start, end))
}

fn full_audience(rule: &Value) -> bool {
    rule.as_object().is_some_and(|fields| {
        fields.iter().all(|(key, value)| match key.as_str() {
            "statuses" => value
                .as_array()
                .is_some_and(|v| v.is_empty() || v.iter().all(|s| s == "active")),
            "tags_all" | "tags_any" => value.as_array().is_some_and(Vec::is_empty),
            "test_only" => value == false,
            _ => false,
        })
    })
}

pub fn register(tx: &mut Tx<'_>, campaign: &Value, issue: &Value, rule: &Value) -> Res<()> {
    if !enabled(tx) || !full_audience(rule) {
        return Ok(());
    }
    let Some(due) = timestamp(&campaign["due_at"]) else {
        return Ok(());
    };
    let Some((start, end)) = window(&issue["draft"]["source"], due) else {
        return Ok(());
    };
    let id = req_str(campaign, "id")?;
    tx.put(
        "welcome_release",
        id,
        json!({"id":id,"issue_id":issue["id"],"start":start,"end":end,"campaign":id}),
    )
}

async fn seed(tx: &Tx<'_>, now: i64) -> Res<Option<Value>> {
    let Some(pin) = conf(tx.env, "WELCOME_SEED_ISSUE") else {
        return Ok(None);
    };
    let Some((id, version)) = pin
        .rsplit_once('@')
        .and_then(|(id, v)| v.parse::<u64>().ok().map(|v| (id, v)))
    else {
        return Ok(None);
    };
    let Some(issue) = tx
        .get("issue_revision", &editorial::revision_key(id, version))
        .await?
    else {
        return Ok(None);
    };
    let Some((start, end)) = window(&issue["draft"]["source"], 0) else {
        return Ok(None);
    };
    if now < start || now >= end {
        return Ok(None);
    }
    let Some(current) = tx.get("issue", id).await? else {
        return Ok(None);
    };
    if current["state"] == "archived" || !delivery::previously_sent(tx, &current).await? {
        return Ok(None);
    }
    Ok(Some(
        json!({"issue_id":id,"start":start,"end":end,"version":version}),
    ))
}

pub async fn enqueue(tx: &mut Tx<'_>, subscriber: &str) -> Res<bool> {
    if !enabled(tx) {
        return Ok(false);
    }
    let now = chrono::Utc::now().timestamp();
    let rows = delivery::records(
        tx,
        "SELECT r.body FROM newsman_records r
      JOIN newsman_records c ON c.kind='campaign' AND c.id=r.id
      JOIN newsman_records i ON i.kind='issue' AND i.id=json_extract(r.body,'$.issue_id')
      WHERE r.kind='welcome_release' AND json_extract(r.body,'$.start')<=?1
        AND json_extract(r.body,'$.end')>?1 AND json_extract(c.body,'$.state')!='stopped'
        AND json_extract(i.body,'$.state')!='archived'
      ORDER BY json_extract(r.body,'$.start') DESC,r.id DESC LIMIT 1",
        &[(now as f64).into()],
    )
    .await?;
    let release = match rows.into_iter().next() {
        Some(v) => v,
        None => match seed(tx, now).await? {
            Some(v) => v,
            None => return Ok(false),
        },
    };
    let id = uid("att");
    tx.put("attempt",&id,json!({"id":id,"kind":"welcome_issue","version":1,"state":"queued",
        "subscriber_id":subscriber,"issue_id":release["issue_id"],"welcome_campaign":release["campaign"],
        "issue_version":release["version"],"expires_s":release["end"],"updated_at":crate::core::now(),"tries":0}))?;
    Ok(true)
}

pub async fn snapshot(tx: &Tx<'_>, a: &Value) -> Res<Option<Value>> {
    if !enabled(tx) || a["expires_s"].as_i64().unwrap_or(0) <= chrono::Utc::now().timestamp() {
        return Ok(None);
    }
    let issue_id = req_str(a, "issue_id")?;
    if tx
        .get("issue", issue_id)
        .await?
        .is_none_or(|i| i["state"] == "archived")
    {
        return Ok(None);
    }
    if let Some(id) = a["welcome_campaign"].as_str() {
        if tx
            .get("campaign", id)
            .await?
            .is_none_or(|c| c["state"] == "stopped")
        {
            return Ok(None);
        }
        return tx.get("message_snapshot", id).await;
    }
    let Some(version) = a["issue_version"].as_u64() else {
        return Ok(None);
    };
    if conf(tx.env, "WELCOME_SEED_ISSUE").as_deref() != Some(&format!("{issue_id}@{version}")) {
        return Ok(None);
    }
    Ok(Some(
        json!({"rendered":editorial::render(tx,issue_id,version).await?, "settings":editorial::settings(tx).await?}),
    ))
}
