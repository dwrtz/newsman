use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde_json::{Value, json};

use crate::{
    core::{AppError, Res, Tx, now, page_values, req_str},
    editorial::settings,
};

struct Window {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

impl Window {
    fn parse(a: &Value) -> Res<Self> {
        let at = |field: &str| -> Res<DateTime<Utc>> {
            Ok(DateTime::parse_from_rfc3339(req_str(a, field)?)
                .map_err(|_| AppError::invalid(format!("Invalid timestamp for {field}.")))?
                .to_utc())
        };
        let win = Self {
            start: at("from")?,
            end: at("to")?,
        };
        if win.start >= win.end {
            return Err(AppError::invalid(
                "Select a report end time after the start.",
            ));
        }
        Ok(win)
    }

    fn contains(&self, field: &Value) -> bool {
        field
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .is_some_and(|v| self.start <= v && v < self.end)
    }
}

fn metric(label: &str, value: usize, unit: &str, source: &str) -> Value {
    json!({"label":label,"value":value,"unit":unit,"source":source})
}
fn rate(label: &str, n: usize, base: usize, source: &str) -> Value {
    if base == 0 {
        json!({"label":label,"unit":"percent","denominator":0,"unavailable_reason":"No eligible denominator in this window.","source":source})
    } else {
        json!({"label":label,"unit":"percent","value":((10000.0*n as f64/base as f64).round()/100.0),"denominator":base,"source":source})
    }
}

fn attempt_key(a: &Value, campaign_map: &BTreeMap<String, String>) -> Option<(String, String)> {
    let issue = campaign_map.get(a["campaign_id"].as_str()?)?;
    Some((issue.clone(), a["subscriber_id"].as_str()?.to_owned()))
}

async fn delivery_report(
    tx: &Tx<'_>,
    w: &Window,
    kind: &str,
    issue: Option<&str>,
) -> Res<Vec<Value>> {
    let campaigns = tx
        .list("campaign")
        .await?
        .into_iter()
        .filter_map(|c| {
            Some((
                c["id"].as_str()?.to_owned(),
                c["issue_id"].as_str()?.to_owned(),
            ))
        })
        .filter(|(_, value)| issue.is_none_or(|s| value == s))
        .collect::<BTreeMap<_, _>>();
    let attempts = tx.list("attempt").await?;
    let mut delivered = BTreeSet::new();
    let mut accepted = BTreeSet::new();
    let mut states: BTreeMap<String, usize> = BTreeMap::new();
    for a in &attempts {
        let Some(key) = attempt_key(a, &campaigns) else {
            continue;
        };
        if w.contains(&a["accepted_at"]) {
            accepted.insert(key.clone());
        }
        if w.contains(&a["delivered_at"]) {
            delivered.insert(key);
        }
        if w.contains(&a["updated_at"]) {
            *states
                .entry(a["state"].as_str().unwrap_or("unknown").to_owned())
                .or_default() += 1;
        }
    }
    let mut rows = vec![];
    if kind == "edition" {
        rows.push(metric("accepted", accepted.len(), "messages", "ses"));
        rows.push(metric("delivered", delivered.len(), "messages", "ses"));
        for key in [
            "hard_bounce",
            "complaint",
            "rejected",
            "transient_bounce",
            "unknown",
        ] {
            rows.push(metric(
                key,
                *states.get(key).unwrap_or(&0),
                "messages",
                "delivery_ledger",
            ));
        }
    }
    let tracking = settings(tx).await?["click_tracking"]
        .as_bool()
        .unwrap_or(false);
    let links = tx
        .list("link")
        .await?
        .into_iter()
        .filter(|link| issue.is_none_or(|id| link["issue_id"] == id))
        .collect::<Vec<_>>();
    let clicks = tx
        .list("click")
        .await?
        .into_iter()
        .filter(|v| w.contains(&v["at"]) && issue.is_none_or(|id| v["issue_id"] == id))
        .collect::<Vec<_>>();
    if !tracking && clicks.is_empty() && links.is_empty() {
        rows.push(json!({"label":"unique clickers","unit":"readers","unavailable_reason":"Click tracking is off.","source":"first_party_redirect"}));
        return Ok(rows);
    }
    let mut readers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut editorial = BTreeSet::new();
    let mut sponsor = BTreeSet::new();
    for c in &clicks {
        let (Some(issue_id), Some(reader), Some(link)) = (
            c["issue_id"].as_str(),
            c["subscriber_id"].as_str(),
            c["link_id"].as_str(),
        ) else {
            continue;
        };
        readers
            .entry(link.to_owned())
            .or_default()
            .insert(reader.to_owned());
        if !delivered.contains(&(issue_id.to_owned(), reader.to_owned())) {
            continue;
        }
        let key = (issue_id.to_owned(), reader.to_owned());
        if c["purpose"] == "sponsor" {
            sponsor.insert(key);
        } else {
            editorial.insert(key);
        }
    }
    if kind == "links" {
        for link in links {
            let Some(id) = link["id"].as_str() else {
                continue;
            };
            let count = readers.get(id).map(BTreeSet::len).unwrap_or(0);
            let purpose = link["purpose"].as_str().unwrap_or("editorial");
            rows.push(metric(
                &format!("{purpose} {id}"),
                count,
                "unique clickers",
                "first_party_redirect",
            ));
        }
        if rows.is_empty() {
            rows.push(metric("tracked links", 0, "links", "first_party_redirect"));
        }
    } else {
        rows.push(rate(
            "editorial clickers per delivered recipient",
            editorial.len(),
            delivered.len(),
            "first_party_redirect",
        ));
        rows.push(rate(
            "sponsor clickers per delivered recipient",
            sponsor.len(),
            delivered.len(),
            "first_party_redirect",
        ));
    }
    Ok(rows)
}

async fn audience_report(tx: &Tx<'_>, w: &Window, kind: &str) -> Res<Vec<Value>> {
    let subs = tx
        .list("subscriber")
        .await?
        .into_iter()
        .filter_map(|s| Some((s["id"].as_str()?.to_owned(), s)))
        .collect::<BTreeMap<_, _>>();
    let events = tx.list("consent_event").await?;
    let zone: Tz = settings(tx).await?["time_zone"]
        .as_str()
        .unwrap_or("UTC")
        .parse()
        .unwrap_or(chrono_tz::UTC);
    let mut seen = BTreeSet::new();
    let mut counts: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for e in &events {
        if e["kind"] != "first_confirmed" || !w.contains(&e["at"]) {
            continue;
        }
        let Some(id) = e["object_id"].as_str() else {
            continue;
        };
        let Some(sub) = subs.get(id) else { continue };
        if sub["status"] == "test_only" || !seen.insert(id) {
            continue;
        }
        let label = if kind == "acquisition" {
            sub["first_touch"]["utm_campaign"]
                .as_str()
                .or_else(|| sub["first_touch"]["utm_source"].as_str())
                .unwrap_or("unknown")
                .to_owned()
        } else {
            let at = DateTime::parse_from_rfc3339(e["at"].as_str().unwrap_or(""))
                .map_err(|_| AppError::invalid("Invalid consent time."))?;
            at.with_timezone(&zone).format("%Y-%m").to_string()
        };
        let c = counts.entry(label).or_default();
        c.0 += 1;
        if sub["status"] == "active" {
            c.1 += 1;
        }
    }
    let mut rows = vec![metric(
        "first confirmations",
        seen.len(),
        "readers",
        "consent_ledger",
    )];
    for (key, (total, active)) in counts {
        if kind == "acquisition" {
            rows.push(metric(
                &format!("campaign {key}"),
                total,
                "readers",
                "consent_ledger",
            ));
        } else {
            rows.push(metric(
                &format!("{key} new readers"),
                total,
                "readers",
                "consent_ledger",
            ));
            rows.push(rate(
                &format!("{key} currently active"),
                active,
                total,
                "consent_ledger",
            ));
        }
    }
    Ok(rows)
}

pub async fn report(tx: &Tx<'_>, a: &Value) -> Res<Vec<Value>> {
    let kind = req_str(a, "kind")?;
    let w = Window::parse(a)?;
    match kind {
        "acquisition" | "cohort" => audience_report(tx, &w, kind).await,
        "edition" | "links" => delivery_report(tx, &w, kind, a["issue_id"].as_str()).await,
        "usage" => {
            let attempts = tx.list("attempt").await?;
            let count = attempts
                .iter()
                .filter(|v| w.contains(&v["accepted_at"]))
                .count();
            let unknown = attempts
                .iter()
                .filter(|v| v["state"] == "unknown" && w.contains(&v["updated_at"]))
                .count();
            let accepted_messages =
                metric("accepted submissions", count, "messages", "delivery_ledger");
            let estimated = json!({"label":"estimated SES base charge","value":count as f64*0.0001,"unit":"USD","source":"ses_list_price"});
            Ok(vec![
                accepted_messages,
                metric(
                    "uncertain submissions",
                    unknown,
                    "messages",
                    "delivery_ledger",
                ),
                estimated,
            ])
        }
        _ => Err(AppError::invalid("Unknown report type.")),
    }
}

async fn dashboard(tx: &Tx<'_>) -> Res<Value> {
    let subscribers = tx.list("subscriber").await?;
    let issues = tx.list("issue").await?;
    let campaigns = tx.list("campaign").await?;
    let attempts = tx.list("attempt").await?;
    let cfg = settings(tx).await?;
    let dispatch = crate::delivery::dispatch(tx).await?;
    let scheduled = campaigns
        .iter()
        .filter(|c| matches!(c["state"].as_str(), Some("scheduled" | "sending")))
        .collect::<Vec<_>>();
    let now = now();
    let active = subscribers
        .iter()
        .filter(|s| crate::audience::eligible(s))
        .count();
    let metrics = vec![
        metric("active subscribers", active, "readers", "audience"),
        metric("messages", issues.len(), "messages", "editor"),
        metric(
            "delivered",
            attempts
                .iter()
                .filter(|a| a["delivered_at"].is_string())
                .count(),
            "messages",
            "ses",
        ),
        metric(
            "scheduled campaigns",
            scheduled.len(),
            "campaigns",
            "scheduler",
        ),
    ];
    let paused = dispatch["paused"] != false;
    let has_footer = cfg["postal_footer"]
        .as_str()
        .is_some_and(|s| !s.trim().is_empty());
    let mut output = json!({"metrics":metrics,"checks":[
        {"name":"Delivery","state":if paused{"warning"}else{"ready"},"detail":if paused{"Dispatch is paused."}else{"Dispatch is enabled."}},
        {"name":"Sender footer","state":if has_footer{"ready"}else{"missing"},"detail":if has_footer{"The sender footer is configured."}else{"Add a postal footer in Settings."}}
    ]});
    if let Some(c) = scheduled.iter().min_by_key(|c| {
        let due = c["due_at"].as_str().unwrap_or("");
        (due > now.as_str(), due)
    }) {
        output["current_campaign_id"] = c["id"].clone();
    }
    Ok(output)
}

pub async fn run(tx: &mut Tx<'_>, name: &str, a: &Value) -> Res<Value> {
    match name {
        "report_get" => page_values(report(tx, a).await?, a),
        "dashboard_get" => dashboard(tx).await,
        _ => Err(AppError::new(
            "not_found",
            "Unknown reporting operation.",
            404,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_are_half_open_and_compare_instants() {
        let w =
            Window::parse(&json!({"from":"2026-10-01T10:00:00+01:00","to":"2026-10-01T12:00:00Z"}))
                .unwrap();
        assert!(w.contains(&json!("2026-10-01T09:00:00Z")));
        assert!(!w.contains(&json!("2026-10-01T12:00:00Z")));
        assert!(!w.contains(&Value::Null));
    }
    #[test]
    fn unavailable_rates_keep_denominators() {
        assert!(rate("sample", 0, 0, "test").get("value").is_none());
        assert_eq!(rate("sample", 1, 2, "test")["value"], 50.);
    }
}
