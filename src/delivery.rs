//! Recipient-level delivery state. All external submission happens after the durable fence commits.
use crate::audience;
use crate::core::{
    Actor, AppError, Res, Tx, expect_version, local, now, page_values, req_str, sha256, uid,
};
use crate::editorial;
use crate::provider::{self, Feedback, Mail, Submission};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use worker::{Env, Request, Response};

const CHUNK_SIZE: usize = 128;
const PLAN_TTL_MIN: i64 = 20;
const MAX_PER_TICK: usize = 12;
const FENCE_SECONDS: i64 = 300;

#[derive(serde::Deserialize)]
struct StoredRecord {
    body: String,
}

/// Used only for bounded indexed reads. Commands continue to apply mutations through the optimistic Tx.
pub(crate) async fn records(
    tx: &Tx<'_>,
    sql: &str,
    args: &[wasm_bindgen::JsValue],
) -> Res<Vec<Value>> {
    let rows = tx
        .env
        .d1("DB")?
        .prepare(sql)
        .bind(args)?
        .all()
        .await?
        .results::<StoredRecord>()?;
    rows.into_iter()
        .map(|r| Ok(serde_json::from_str(&r.body)?))
        .collect()
}
async fn ready_attempt(tx: &Tx<'_>, paused: bool, at: i64) -> Res<Option<Value>> {
    let rows=records(tx,"SELECT body FROM newsman_records WHERE kind='attempt' AND json_extract(body,'$.state')='queued'
        AND coalesce(json_extract(body,'$.retry_at'),0)<=? AND (?=0 OR json_extract(body,'$.kind')!='campaign')
        ORDER BY coalesce(json_extract(body,'$.retry_at'),0),id LIMIT 1",&[(at as f64).into(),(i32::from(paused)).into()]).await?;
    Ok(rows.into_iter().next())
}
fn clock() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&now())
        .expect("UTC clock")
        .to_utc()
}
fn time(v: &Value, key: &str) -> Res<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(req_str(v, key)?)
        .map(|t| t.to_utc())
        .map_err(|_| AppError::invalid(format!("Invalid timestamp in {key}")))
}
fn public(v: &Value, keys: &[&str]) -> Value {
    Value::Object(
        keys.iter()
            .filter_map(|&k| {
                v.get(k)
                    .filter(|v| !v.is_null())
                    .map(|value| (k.to_owned(), value.clone()))
            })
            .collect(),
    )
}
fn count(label: &str, n: usize) -> Value {
    json!({"label":label,"value":n,"unit":"recipients"})
}
fn conflict(message: &str) -> AppError {
    AppError::new("conflict", message, 409)
}
fn bump(v: &mut Value) {
    v["version"] = json!(v["version"].as_u64().unwrap_or(0) + 1);
}
fn hash_json(v: &Value) -> Res<String> {
    Ok(sha256(&serde_json::to_vec(v)?))
}
pub async fn dispatch(tx: &Tx<'_>) -> Res<Value> {
    Ok(tx
        .get("dispatch", "state")
        .await?
        .unwrap_or_else(|| json!({"id":"state","paused":true,"version":1})))
}
fn attempt_out(v: &Value) -> Value {
    public(
        v,
        &[
            "id",
            "version",
            "campaign_id",
            "subscriber_id",
            "state",
            "provider_id",
            "updated_at",
        ],
    )
}
async fn accepted_ids(tx: &Tx<'_>, filter: &Value) -> Res<(Vec<String>, usize)> {
    let mut selected = Vec::new();
    let mut excluded = 0;
    let mut after = String::new();
    loop {
        let page = tx.page("subscriber", &after, 500).await?;
        if page.is_empty() {
            break;
        }
        for sub in &page {
            if audience::matches(sub, filter) {
                if audience::eligible(sub) {
                    selected.push(req_str(sub, "id")?.to_owned())
                } else {
                    excluded += 1
                }
            }
        }
        if page.len() < 500 {
            break;
        }
        after = req_str(page.last().expect("nonempty page"), "id")?.to_owned();
    }
    selected.sort_unstable();
    Ok((selected, excluded))
}
fn audience_digest(ids: &[String]) -> String {
    sha256(ids.join("\n").as_bytes())
}
fn job_out(v: &Value) -> Value {
    public(v, &["id", "version", "state", "kind", "counts", "error"])
}

async fn sendable(tx: &Tx<'_>, issue: &Value) -> Res<()> {
    let external = issue["draft"]["source"]["external_key"]
        .as_str()
        .or_else(|| issue["external_key"].as_str());
    if issue["state"] == "archived" {
        return Err(conflict("Archived messages cannot start a new campaign"));
    }
    if issue["state"] == "historical"
        || tx.list("historical_send").await?.iter().any(|r| {
            (r["issue_id"] == issue["id"]
                || external.is_some_and(|k| r["external_key"] == k || r["issue_id"] == k))
                && r["status"].as_str().is_some_and(|s| {
                    matches!(
                        s.to_ascii_lowercase().as_str(),
                        "sent" | "completed" | "delivered"
                    )
                })
        })
    {
        return Err(conflict(
            "This edition was already sent by the previous publisher",
        ));
    }
    Ok(())
}

async fn prepare(tx: &mut Tx<'_>, input: &Value) -> Res<Value> {
    let issue_id = req_str(input, "issue_id")?;
    let issue_version = input["issue_version"]
        .as_u64()
        .ok_or_else(|| AppError::invalid("Missing issue version"))?;
    let issue = tx.require("issue", issue_id).await?;
    sendable(tx, &issue).await?;
    if issue["version"] != issue_version || issue["state"] == "historical" {
        return Err(conflict(
            "Only the current sendable issue revision can be scheduled",
        ));
    }
    let settings = editorial::settings(tx).await?;
    let rendered = editorial::render(tx, issue_id, issue_version).await?;
    let (ids, excluded) = accepted_ids(tx, &input["audience"]).await?;
    let timestamp = clock();
    let due = input
        .get("due_at")
        .map(|_| time(input, "due_at"))
        .transpose()?
        .unwrap_or(timestamp);
    let id = uid("plan");
    let mut plan = json!({"id":id,"issue_digest":rendered["digest"],"eligibility_digest":audience_digest(&ids),
        "expires_at":(timestamp+Duration::minutes(PLAN_TTL_MIN)).to_rfc3339(),"due_at":due.to_rfc3339(),
        "subject":rendered["subject"],"from_address":rendered["from_address"],"reply_to":rendered["reply_to"],
        "counts":[count("eligible",ids.len()),count("excluded",excluded)],"audience_rule":input["audience"],
        "warnings":rendered["warnings"],"cost":{"currency":"USD","estimated_messages":ids.len(),"ses_base":ids.len() as f64*0.0001}});
    let settings_digest = hash_json(&settings)?;
    plan["digest"] = json!(hash_json(&json!([
        &plan,
        issue_id,
        issue_version,
        &settings_digest
    ]))?);
    tx.put("send_plan",&id,json!({"id":id,"plan":plan,"issue_id":issue_id,"issue_version":issue_version,"settings_digest":settings_digest}))?;
    Ok(plan)
}

async fn commit_plan(tx: &mut Tx<'_>, input: &Value) -> Res<Value> {
    let stored = tx.require("send_plan", req_str(input, "plan_id")?).await?;
    let plan = &stored["plan"];
    if input["plan_digest"] != plan["digest"] {
        return Err(conflict("The send plan digest has changed"));
    }
    if let Some(id) = stored["campaign_id"].as_str() {
        return campaign_out(tx, &tx.require("campaign", id).await?).await;
    }
    if clock() > time(plan, "expires_at")? {
        return Err(conflict("The send plan expired. Prepare a fresh plan"));
    }
    let issue_id = req_str(&stored, "issue_id")?;
    let issue = tx.require("issue", issue_id).await?;
    sendable(tx, &issue).await?;
    let settings = editorial::settings(tx).await?;
    if issue["version"] != stored["issue_version"]
        || issue["state"] == "historical"
        || stored["settings_digest"] != hash_json(&settings)?
    {
        return Err(conflict(
            "The issue or publication settings changed. Prepare the send again",
        ));
    }
    let rendered =
        editorial::render(tx, issue_id, stored["issue_version"].as_u64().unwrap_or(0)).await?;
    if rendered["digest"] != plan["issue_digest"] {
        return Err(conflict("The rendered proof changed"));
    }
    let (ids, _) = accepted_ids(tx, &plan["audience_rule"]).await?;
    if plan["eligibility_digest"] != audience_digest(&ids) {
        return Err(conflict(
            "The eligible audience changed. Review the new count",
        ));
    }
    if dispatch(tx).await?["paused"] != false {
        return Err(conflict(
            "Dispatch is paused. Resume it before committing a send",
        ));
    }
    let n = ids.len();
    let limit = settings["send_budget"].as_u64().unwrap_or_default() as usize;
    if !local(tx.env) && n > limit {
        return Err(conflict(
            "The audience exceeds the publication's monthly send ceiling",
        ));
    }
    let id = uid("campaign");
    // Large audiences are frozen in bounded rows. The scheduler creates only a few attempts per invocation.
    for (i, chunk) in ids.chunks(CHUNK_SIZE).enumerate() {
        let key = format!("{id}-{i}");
        tx.put("campaign_chunk", &key, json!({"id":key,"members":chunk}))?;
    }
    let campaign = json!({"id":id,"issue_id":issue_id,"version":1,"issue_digest":plan["issue_digest"],
        "state":if n==0{"finished"}else{"scheduled"},"due_at":plan["due_at"],"total":n,"cursor":0,
        "plan_id":stored["id"]});
    tx.put(
        "message_snapshot",
        &id,
        json!({"id":id,"rendered":rendered,"settings":settings}),
    )?;
    tx.put("campaign", &id, campaign.clone())?;
    let mut completed = stored;
    completed["campaign_id"] = json!(id);
    let key = req_str(&completed, "id")?.to_owned();
    tx.put("send_plan", &key, completed)?;
    campaign_out(tx, &campaign).await
}

async fn campaign_out(tx: &Tx<'_>, record: &Value) -> Res<Value> {
    let mut result = public(
        record,
        &[
            "id",
            "issue_id",
            "version",
            "issue_digest",
            "state",
            "due_at",
        ],
    );
    #[derive(serde::Deserialize)]
    struct CountRow {
        state: String,
        n: usize,
        accepted: usize,
        delivered: usize,
    }
    let grouped=tx.env.d1("DB")?.prepare("SELECT json_extract(body,'$.state') AS state,COUNT(*) AS n,
        COUNT(json_extract(body,'$.accepted_at')) AS accepted,COUNT(json_extract(body,'$.delivered_at')) AS delivered
        FROM newsman_records WHERE kind='attempt' AND json_extract(body,'$.campaign_id')=? GROUP BY json_extract(body,'$.state')")
        .bind(&[req_str(record,"id")?.into()])?.all().await?.results::<CountRow>()?;
    let mut counters: BTreeMap<String, usize> = BTreeMap::new();
    let (mut rows, mut submitted, mut delivered) = (0, 0, 0);
    for g in grouped {
        rows += g.n;
        submitted += g.accepted;
        delivered += g.delivered;
        if !matches!(g.state.as_str(), "accepted" | "delivered") {
            counters.insert(g.state, g.n);
        }
    }
    counters.insert("accepted".into(), submitted);
    counters.insert("delivered".into(), delivered);
    let total = record["total"].as_u64().unwrap_or_default() as usize;
    if rows < total {
        let pending = if record["state"] == "stopped" {
            "skipped"
        } else {
            "queued"
        };
        *counters.entry(pending.to_owned()).or_default() += total - rows;
    }
    let mut counts = vec![count("audience", total)];
    counts.extend(counters.into_iter().map(|(k, v)| count(&k, v)));
    result["counts"] = json!(counts);
    Ok(result)
}

async fn test_send(tx: &mut Tx<'_>, input: &Value) -> Res<Value> {
    let settings = editorial::settings(tx).await?;
    let recipient = req_str(input, "recipient")?;
    let canonical = audience::email_key(recipient)
        .ok_or_else(|| AppError::invalid("Recipient email is invalid"))?;
    if !settings["test_recipients"].as_array().is_some_and(|a| {
        a.iter()
            .filter_map(Value::as_str)
            .any(|s| audience::email_key(s).as_ref() == Some(&canonical))
    }) {
        return Err(AppError::new(
            "recipient_not_allowed",
            "Add this address to the publication's test recipients before sending a proof",
            403,
        ));
    }
    let rendered = editorial::render(
        tx,
        req_str(input, "issue_id")?,
        input["issue_version"].as_u64().unwrap_or(0),
    )
    .await?;
    let id = uid("test");
    let job = json!({"id":id,"version":1,"kind":"test","state":"queued","counts":[],"owner":tx.actor.id,"scope":"send"});
    tx.put("job", &id, job.clone())?;
    tx.put("mail_queue",&id,json!({"id":id,"kind":"test","subscriber_id":id,"state":"queued","created_at":now(),
        "subject":format!("[Test] {}",req_str(&rendered,"subject")?),"html":rendered["html"],"text":rendered["plain_text"],
        "from_address":rendered["from_address"],"reply_to":rendered["reply_to"],"recipient":canonical,"version":1}))?;
    Ok(job_out(&job))
}

fn check(name: &str, state: &str, detail: &str) -> Value {
    json!({"name":name,"state":state,"detail":detail})
}
async fn health(tx: &Tx<'_>) -> Res<Value> {
    let mode = dispatch(tx).await?;
    let settings = editorial::settings(tx).await?;
    let mut checks = vec![check("storage", "ready", "D1 is reachable")];
    for field in ["from_address", "reply_to", "postal_footer", "public_origin"] {
        let ok = settings[field]
            .as_str()
            .is_some_and(|v| !v.trim().is_empty());
        checks.push(check(
            field,
            if ok { "ready" } else { "missing" },
            if ok {
                "Configured"
            } else {
                "Configure this publication setting"
            },
        ));
    }
    let ready = provider::configuration(tx.env);
    if let Err(error) = &ready {
        checks.push(check("SES sender", "missing", &error.message));
    }
    let quota = if ready.is_ok() {
        provider::quota(tx.env).await.ok()
    } else {
        None
    };
    let local_mode = local(tx.env);
    if !local_mode && settings["send_budget"].as_u64().unwrap_or_default() == 0 {
        checks.push(check(
            "send budget",
            "missing",
            "Set a monthly recipient ceiling above zero to enable live mail",
        ));
    }
    let account_ok = quota.as_ref().is_some_and(|q| {
        q["SendingEnabled"] == true && (local_mode || q["ProductionAccessEnabled"] == true)
    });
    checks.push(check(
        "SES account",
        if account_ok { "ready" } else { "missing" },
        if local_mode {
            "Local capture transport"
        } else if account_ok {
            "Production access and sending enabled"
        } else {
            "Verify production access, IAM and regional sending status"
        },
    ));
    checks.push(check(
        "reader proof key",
        if audience::proof_secret(tx.env).is_ok() {
            "ready"
        } else {
            "missing"
        },
        "Signing key for unsubscribe and confirmation URLs",
    ));
    let feedback = tx.get("provider_state", "feedback").await?;
    checks.push(check(
        "SNS feedback",
        if local_mode || feedback.as_ref().is_some_and(|v| v["subscribed"] == true) {
            "ready"
        } else {
            "missing"
        },
        feedback
            .as_ref()
            .and_then(|v| v["last_at"].as_str())
            .unwrap_or(if local_mode {
                "Local capture sink"
            } else {
                "Awaiting signed confirmation or event"
            }),
    ));
    let attempts = tx.list("attempt").await?;
    let pending = attempts.iter().filter(|a| a["state"] == "queued").count();
    let unknown = attempts.iter().filter(|a| a["state"] == "unknown").count();
    let mut metrics = vec![count("queued", pending), count("unknown", unknown)];
    let max_lag = attempts
        .iter()
        .filter(|a| a["state"] == "queued")
        .filter_map(|a| time(a, "updated_at").ok())
        .min()
        .map(|t| (clock() - t).num_seconds().max(0))
        .unwrap_or(0);
    metrics.push(json!({"label":"oldest ready attempt","value":max_lag,"unit":"seconds"}));
    if let Some(q) = quota {
        for field in ["Max24HourSend", "MaxSendRate", "SentLast24Hours"] {
            if let Some(n) = q["SendQuota"][field].as_f64().filter(|n| *n >= 0.0) {
                metrics.push(json!({"label":field,"value":n,"unit":if field=="MaxSendRate"{"recipients/second"}else{"recipients"}}));
            }
        }
    }
    Ok(json!({"paused":mode["paused"],"version":mode["version"],"checks":checks,"metrics":metrics}))
}

pub async fn run(tx: &mut Tx<'_>, name: &str, a: &Value) -> Res<Value> {
    match name {
        "health_get" => health(tx).await,
        "dispatch_set" => {
            let mut d = dispatch(tx).await?;
            expect_version(&d, a)?;
            if a["paused"] == false {
                let h = health(tx).await?;
                if h["checks"].as_array().is_some_and(|list| {
                    list.iter()
                        .any(|i| i["state"] == "missing" || i["state"] == "failed")
                }) {
                    return Err(conflict(
                        "Resolve the missing items in health_get before resuming",
                    ));
                }
            }
            bump(&mut d);
            d["paused"] = a["paused"].clone();
            d["reason"] = a["reason"].clone();
            tx.put("dispatch", "state", d.clone())?;
            Ok(public(&d, &["id", "version"]))
        }
        "send_test" => test_send(tx, a).await,
        "send_prepare" => prepare(tx, a).await,
        "send_commit" => commit_plan(tx, a).await,
        "campaign_get" => campaign_out(tx, &tx.require("campaign", req_str(a, "id")?).await?).await,
        "campaigns_list" => {
            let mut p = page_values(tx.list("campaign").await?, a)?;
            let mut items = Vec::new();
            for v in p["items"].as_array().into_iter().flatten() {
                items.push(campaign_out(tx, v).await?);
            }
            p["items"] = json!(items);
            Ok(p)
        }
        "send_cancel" => {
            let id = req_str(a, "id")?;
            let mut c = tx.require("campaign", id).await?;
            expect_version(&c, a)?;
            if c["state"] != "finished" && c["state"] != "stopped" {
                c["state"] = json!("stopped");
                bump(&mut c);
                c["stop_reason"] = a["reason"].clone();
                tx.put("campaign", id, c.clone())?;
            }
            campaign_out(tx, &c).await
        }
        "attempts_list" => {
            tx.require("campaign", req_str(a, "campaign_id")?).await?;
            page_values(
                tx.list("attempt")
                    .await?
                    .iter()
                    .filter(|v| {
                        v["campaign_id"] == a["campaign_id"]
                            && (a["state"].is_null() || a["state"] == v["state"])
                    })
                    .map(attempt_out)
                    .collect(),
                a,
            )
        }
        "attempt_resolve" => {
            let id = req_str(a, "id")?;
            let mut v = tx.require("attempt", id).await?;
            expect_version(&v, a)?;
            if v["state"] != "unknown" {
                return Err(conflict("Only unknown attempts can be reconciled"));
            }
            if req_str(a, "evidence_note")?.trim().is_empty() {
                return Err(AppError::invalid("Describe the reconciliation evidence"));
            }
            let outcome = req_str(a, "outcome")?;
            let next = match outcome {
                "accepted_with_evidence" => {
                    v["provider_id"] = json!(req_str(a, "provider_id")?);
                    v["accepted_at"] = json!(now());
                    "accepted"
                }
                "permanent_skip" => "skipped",
                "proven_not_submitted" => "queued",
                _ => return Err(AppError::invalid("Unknown resolution")),
            };
            v["resolution"] = a["evidence_note"].clone();
            v["resolution_outcome"] = a["outcome"].clone();
            v["state"] = json!(next);
            v["updated_at"] = json!(now());
            bump(&mut v);
            tx.put("attempt", id, v.clone())?;
            sync_related(tx, &v).await?;
            Ok(attempt_out(&v))
        }
        _ => Err(AppError::new(
            "unknown_operation",
            "Unknown delivery operation",
            404,
        )),
    }
}

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use url::Url;

fn link_token(secret: &str, attempt: &str, link: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("valid HMAC key");
    mac.update(format!("newsman/click/v1\n{attempt}\n{link}").as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn message_links(
    html: &str,
    text: &str,
    origin: &str,
    issue: &str,
    attempt: &str,
    secret: &str,
) -> Res<(String, String, Vec<Value>)> {
    use lol_html::{RewriteStrSettings, element, rewrite_str};
    let public = Url::parse(origin).map_err(|_| AppError::invalid("Invalid publication origin"))?;
    let mut links = Vec::new();
    let mut replacements = BTreeMap::new();
    let rendered = rewrite_str(
        html,
        RewriteStrSettings {
            element_content_handlers: vec![element!("a[href]", |el| {
                // lol_html exposes the serialized attribute. Decode exactly the entity set emitted by
                // our html5ever-based sanitizer, once, before storing an actual redirect destination.
                let href = el
                    .get_attribute("href")
                    .unwrap_or_default()
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&quot;", "\"")
                    .replace("&nbsp;", "\u{a0}")
                    .replace("&amp;", "&");
                let Ok(destination) = Url::parse(&href) else {
                    return Ok(());
                };
                if !["https", "http"].contains(&destination.scheme())
                    || !destination.username().is_empty()
                    || destination.password().is_some()
                {
                    return Ok(());
                }
                if destination.origin() == public.origin()
                    && ["/confirm/", "/unsubscribe/", "/r/", "/subscribe/"]
                        .iter()
                        .any(|p| destination.path().starts_with(p))
                {
                    return Ok(());
                }
                let purpose = if el.get_attribute("data-purpose").as_deref() == Some("sponsor") {
                    "sponsor"
                } else {
                    "editorial"
                };
                let id = sha256(format!("{issue}\n{purpose}\n{href}").as_bytes());
                let path = format!(
                    "{}/r/{attempt}/{id}/{}",
                    origin.trim_end_matches('/'),
                    link_token(secret, attempt, &id)
                );
                replacements
                    .entry(href.clone())
                    .or_insert_with(|| path.clone());
                links.push(json!({"id":id,"issue_id":issue,"purpose":purpose,"url":href}));
                el.set_attribute("href", &path)?;
                Ok(())
            })],
            ..RewriteStrSettings::default()
        },
    )
    .map_err(|_| AppError::invalid("Unable to prepare links in the email"))?;
    // Match complete known destinations, preserving plain text and URLs with shared prefixes.
    let rendered_text = if replacements.is_empty() {
        text.to_owned()
    } else {
        let re = regex::Regex::new(r#"https?://[^\s<>"']+"#).expect("static URL regex");
        re.replace_all(text, |c: &regex::Captures<'_>| {
            let raw = &c[0];
            if let Some(value) = replacements.get(raw) {
                return value.clone();
            }
            let cleaned = raw.trim_end_matches([',', '.', ')', ']', ';', '!', '?']);
            replacements
                .get(cleaned)
                .map(|v| format!("{v}{}", &raw[cleaned.len()..]))
                .unwrap_or_else(|| raw.to_owned())
        })
        .into_owned()
    };
    Ok((rendered, rendered_text, links))
}

pub async fn redirect(req: Request, env: &Env) -> Res<Response> {
    if req.method() != worker::Method::Get {
        return Ok(Response::error("Method not allowed", 405)?);
    }
    let path = req.path();
    let parts: Vec<_> = path.trim_matches('/').split('/').collect();
    if parts.len() != 4
        || parts[0] != "r"
        || parts[1].len() > 128
        || parts[2].len() != 64
        || parts[3].len() > 100
    {
        return Ok(Response::error("Link unavailable", 404)?);
    }
    use subtle::ConstantTimeEq;
    let token = link_token(&audience::proof_secret(env)?, parts[1], parts[2]);
    if !bool::from(token.as_bytes().ct_eq(parts[3].as_bytes())) {
        return Ok(Response::error("Link unavailable", 404)?);
    }
    let mut tx = Tx::begin(env, Actor::system()).await?;
    let link = tx.require("link", parts[2]).await?;
    let attempt = tx.require("attempt", parts[1]).await?;
    if attempt["issue_id"] != link["issue_id"] {
        return Ok(Response::error("Link unavailable", 404)?);
    }
    let url = Url::parse(req_str(&link, "url")?)
        .map_err(|_| AppError::invalid("Invalid stored destination"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(AppError::invalid("Invalid stored destination"));
    }
    let id = uid("click");
    tx.put("click",&id,json!({"id":id,"issue_id":link["issue_id"],"link_id":link["id"],"subscriber_id":attempt["subscriber_id"],
        "campaign_id":attempt["campaign_id"],"purpose":link["purpose"],"at":now()}))?;
    // Analytics failures cannot break a published URL. Only the already-validated destination is used.
    let _ = tx.commit().await;
    let mut res = Response::empty()?.with_status(302);
    res.headers_mut().set("Location", url.as_str())?;
    res.headers_mut()
        .set("Cache-Control", "private, no-store")?;
    res.headers_mut().set("Referrer-Policy", "no-referrer")?;
    Ok(res)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
fn footer(body: &str, link: &str) -> String {
    let f = format!("<p><a href=\"{}\">Unsubscribe</a></p>", esc(link));
    if let Some(pos) = body.to_ascii_lowercase().rfind("</body>") {
        format!("{}{}{}", &body[..pos], f, &body[pos..])
    } else {
        format!("{body}{f}")
    }
}

async fn build_mail(tx: &mut Tx<'_>, a: &Value, submit_id: &str) -> Res<Option<Mail>> {
    let source = req_str(a, "kind")?;
    let subscriber_id = req_str(a, "subscriber_id")?;
    if source == "campaign" {
        let campaign = tx.require("campaign", req_str(a, "campaign_id")?).await?;
        let Some(sub) = tx.get("subscriber", subscriber_id).await? else {
            return Ok(None);
        };
        if campaign["state"] == "stopped" || !audience::eligible(&sub) {
            return Ok(None);
        }
        let snapshot = tx
            .get("message_snapshot", req_str(&campaign, "id")?)
            .await?
            .unwrap_or_else(|| campaign.clone());
        let settings = &snapshot["settings"];
        let rendered = &snapshot["rendered"];
        let secret = audience::proof_secret(tx.env)?;
        let api_origin = crate::core::conf(tx.env, "PUBLIC_ORIGIN").unwrap_or_else(|| {
            settings["public_origin"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        });
        let origin = api_origin.trim_end_matches('/');
        let unsub = format!(
            "{origin}/unsubscribe/{}",
            audience::unsubscribe_proof(&secret, &sub)
        );
        let html = req_str(rendered, "html")?;
        let text = req_str(rendered, "plain_text")?;
        let (html, text, links) = if settings["click_tracking"] == true {
            message_links(
                html,
                text,
                origin,
                req_str(&campaign, "issue_id")?,
                req_str(a, "id")?,
                &secret,
            )?
        } else {
            (html.to_owned(), text.to_owned(), Vec::new())
        };
        for link in links {
            let id = req_str(&link, "id")?.to_owned();
            tx.put("link", &id, link)?;
        }
        return Ok(Some(Mail {
            from: req_str(rendered, "from_address")?.into(),
            reply_to: req_str(rendered, "reply_to")?.into(),
            to: req_str(&sub, "email")?.into(),
            subject: req_str(rendered, "subject")?.into(),
            text: format!("{text}\n\nUnsubscribe: {unsub}"),
            html: footer(&html, &unsub),
            unsubscribe: Some(unsub),
            attempt_id: submit_id.to_owned(),
            campaign_id: Some(req_str(&campaign, "id")?.into()),
        }));
    }
    // Erasing a reader also erases their pending confirmation payload. A queued reference may
    // survive that transaction and must be skipped without blocking the next recipient.
    let Some(q) = tx.get("mail_queue", req_str(a, "queue_id")?).await? else {
        return Ok(None);
    };
    let settings = editorial::settings(tx).await?;
    let recipient = if source == "confirmation" {
        let Some(sub) = tx.get("subscriber", subscriber_id).await? else {
            return Ok(None);
        };
        if !matches!(sub["status"].as_str(), Some("pending" | "unsubscribed"))
            || sub["challenge_nonce"] != q["nonce"]
            || sub["challenge_until"].as_i64().unwrap_or(0) < clock().timestamp()
        {
            return Ok(None);
        }
        req_str(&sub, "email")?.to_owned()
    } else {
        let to = req_str(&q, "recipient")?;
        if !settings["test_recipients"].as_array().is_some_and(|v| {
            v.iter()
                .filter_map(Value::as_str)
                .any(|i| audience::email_key(i) == audience::email_key(to))
        }) {
            return Ok(None);
        }
        to.to_owned()
    };
    let mut text = req_str(&q, "text")?.to_owned();
    let mut html = req_str(&q, "html")?.to_owned();
    let mut unsubscribe = None;
    if source == "test"
        && let Some(sub) = audience::lookup(tx, &recipient).await?
        && audience::eligible(&sub)
    {
        let origin = crate::core::conf(tx.env, "PUBLIC_ORIGIN")
            .unwrap_or(req_str(&settings, "public_origin")?.to_owned());
        let proof = audience::unsubscribe_proof(&audience::proof_secret(tx.env)?, &sub);
        let link = format!("{}/unsubscribe/{proof}", origin.trim_end_matches('/'));
        text.push_str(&format!("\n\nUnsubscribe: {link}"));
        html = footer(&html, &link);
        unsubscribe = Some(link);
    }
    Ok(Some(Mail {
        to: recipient,
        from: q["from_address"]
            .as_str()
            .unwrap_or(req_str(&settings, "from_address")?)
            .into(),
        reply_to: q["reply_to"]
            .as_str()
            .unwrap_or(req_str(&settings, "reply_to")?)
            .into(),
        subject: req_str(&q, "subject")?.into(),
        text,
        html,
        unsubscribe,
        attempt_id: submit_id.into(),
        campaign_id: None,
    }))
}

// Advance the durable outbox without issuing an HTTP request. A crash after this stage can be recovered by cron.
async fn discover(env: &Env) -> Res<()> {
    let mut tx = Tx::begin(env, Actor::system()).await?;
    let timestamp = clock();
    const EXPIRED: &str="SELECT body FROM newsman_records WHERE kind='attempt' AND json_extract(body,'$.state')='submitting'
        AND json_extract(body,'$.lease_until')<=? ORDER BY json_extract(body,'$.lease_until'),id LIMIT 10";
    for mut a in records(&tx, EXPIRED, &[(timestamp.timestamp() as f64).into()]).await? {
        a["state"] = json!("unknown");
        a["updated_at"] = json!(now());
        bump(&mut a);
        let key = req_str(&a, "id")?.to_owned();
        tx.put("attempt", &key, a.clone())?;
        sync_related(&mut tx, &a).await?;
    }
    let mut n = 0;
    for mut mail in records(&tx,"SELECT body FROM newsman_records WHERE kind='mail_queue' AND json_extract(body,'$.state')='queued' ORDER BY id LIMIT ?", &[(MAX_PER_TICK as i32).into()]).await? {
        let id = req_str(&mail, "id")?.to_owned();
        tx.put("attempt",&id,json!({"id":id,"version":1,"kind":mail["kind"],"queue_id":id,"subscriber_id":mail["subscriber_id"],
            "state":"queued","updated_at":now(),"tries":0}))?;
        mail["state"] = json!("running");
        tx.put("mail_queue", &id, mail)?;
        n += 1;
    }
    if dispatch(&tx).await?["paused"] == false {
        let due=records(&tx,"SELECT body FROM newsman_records WHERE kind='campaign' AND json_extract(body,'$.state') IN ('scheduled','sending')
            AND unixepoch(json_extract(body,'$.due_at'))<=? AND json_extract(body,'$.cursor')<json_extract(body,'$.total')
            ORDER BY unixepoch(json_extract(body,'$.due_at')),id LIMIT ?", &[(timestamp.timestamp() as f64).into(),(MAX_PER_TICK as i32).into()]).await?;
        for candidate in due {
            let mut campaign = tx.require("campaign", req_str(&candidate, "id")?).await?;
            if n >= MAX_PER_TICK {
                break;
            }
            if !matches!(campaign["state"].as_str(), Some("scheduled" | "sending"))
                || time(&campaign, "due_at")? > timestamp
            {
                continue;
            }
            let id = req_str(&campaign, "id")?.to_owned();
            let total = campaign["total"].as_u64().unwrap_or(0) as usize;
            let start = campaign["cursor"].as_u64().unwrap_or(0) as usize;
            if start >= total {
                continue;
            }
            let chunk = tx
                .require("campaign_chunk", &format!("{id}-{}", start / CHUNK_SIZE))
                .await?;
            let members = chunk["members"]
                .as_array()
                .ok_or_else(|| AppError::invalid("Invalid audience snapshot"))?;
            let slice = members
                .iter()
                .skip(start % CHUNK_SIZE)
                .take(MAX_PER_TICK - n);
            let mut pos = start;
            for member in slice {
                let sub = member
                    .as_str()
                    .ok_or_else(|| AppError::invalid("Invalid audience member"))?;
                let key = format!("a_{}", sha256(format!("{id}/{sub}").as_bytes()));
                tx.put("attempt",&key,json!({"id":key,"version":1,"kind":"campaign","campaign_id":id,"issue_id":campaign["issue_id"],
                    "subscriber_id":sub,"state":"queued","updated_at":now(),"tries":0}))?;
                n += 1;
                pos += 1;
            }
            campaign["cursor"] = json!(pos);
            campaign["state"] = json!("sending");
            bump(&mut campaign);
            tx.put("campaign", &id, campaign)?;
        }
    }
    tx.commit().await
}

async fn sync_related(tx: &mut Tx<'_>, a: &Value) -> Res<()> {
    if let Some(qid) = a["queue_id"].as_str() {
        if let Some(mut q) = tx.get("mail_queue", qid).await? {
            q["state"] = a["state"].clone();
            tx.put("mail_queue", qid, q)?;
        }
        if a["kind"] == "test" {
            let mut job = tx.require("job", qid).await?;
            job["state"] = json!(match a["state"].as_str() {
                Some("accepted" | "delivered") => "succeeded",
                Some("unknown") => "needs_review",
                Some("queued" | "submitting") => "running",
                _ => "failed",
            });
            bump(&mut job);
            tx.put("job", qid, job)?;
        }
    }
    if let Some(id) = a["campaign_id"].as_str() {
        let mut c = tx.require("campaign", id).await?;
        if c["state"] == "stopped" || c["total"] != c["cursor"] {
            return Ok(());
        }
        let mut ongoing = matches!(a["state"].as_str(), Some("queued" | "submitting"));
        let mut unknown = a["state"] == "unknown";
        // At most ten other attempts can change during the expiry pass. Read the staged values
        // for up to eleven indexed candidates per state, which covers this transaction's overlay.
        for state in ["queued", "submitting", "unknown"] {
            if (state == "unknown" && unknown) || (state != "unknown" && ongoing) {
                continue;
            }
            let candidates=records(tx,"SELECT body FROM newsman_records WHERE kind='attempt' AND json_extract(body,'$.campaign_id')=?
                AND json_extract(body,'$.state')=? AND id<>? LIMIT 11",&[id.into(),state.into(),req_str(a,"id")?.into()]).await?;
            for item in candidates {
                let live = tx.require("attempt", req_str(&item, "id")?).await?;
                match live["state"].as_str() {
                    Some("queued" | "submitting") => ongoing = true,
                    Some("unknown") => unknown = true,
                    _ => {}
                }
                if (state == "unknown" && unknown) || (state != "unknown" && ongoing) {
                    break;
                }
            }
        }
        let new = if ongoing {
            "sending"
        } else if unknown {
            "needs_review"
        } else {
            "finished"
        };
        if c["state"] != new {
            c["state"] = json!(new);
            bump(&mut c);
            tx.put("campaign", id, c)?;
        }
    }
    Ok(())
}

async fn capacity(tx: &mut Tx<'_>, quota: &Value, budget: u64, t: &DateTime<Utc>) -> Res<bool> {
    let q = &quota["SendQuota"];
    let max_rate = q["MaxSendRate"].as_f64().unwrap_or(0.0);
    let daily = q["Max24HourSend"].as_f64().unwrap_or(0.0);
    let observed = q["SentLast24Hours"].as_f64().unwrap_or(0.0);
    if quota["SendingEnabled"] != true || max_rate <= 0.0 || (daily >= 0.0 && observed >= daily) {
        return Ok(false);
    }
    let mut state = tx
        .get("capacity", "ses")
        .await?
        .unwrap_or_else(|| json!({"id":"ses"}));
    let month = t.format("%Y-%m").to_string();
    let now_s = t.timestamp();
    let monthly = if state["month"] == month {
        state["month_used"].as_u64().unwrap_or(0)
    } else {
        0
    };
    let second = if state["second"] == now_s {
        state["second_used"].as_u64().unwrap_or(0)
    } else {
        0
    };
    let hour = now_s.div_euclid(3600);
    let mut buckets = state["hours"].as_object().cloned().unwrap_or_default();
    buckets.retain(|key, _| key.parse::<i64>().ok().is_some_and(|h| h >= hour - 24));
    let rolling: u64 = buckets.values().filter_map(Value::as_u64).sum();
    if monthly >= budget
        || second >= max_rate.max(1.0).floor() as u64
        || (daily >= 0.0 && rolling >= daily as u64)
    {
        return Ok(false);
    }
    let current = buckets
        .get(&hour.to_string())
        .and_then(Value::as_u64)
        .unwrap_or(0);
    buckets.insert(hour.to_string(), json!(current + 1));
    state["hours"] = json!(buckets);
    state["month"] = json!(month);
    state["month_used"] = json!(monthly + 1);
    state["second"] = json!(now_s);
    state["second_used"] = json!(second + 1);
    tx.put("capacity", "ses", state)?;
    Ok(true)
}

enum Work {
    Idle,
    Skipped,
    Mail(String, Mail),
}
async fn next_submission(env: &Env, quota: &Value) -> Res<Work> {
    let mut tx = Tx::begin(env, Actor::system()).await?;
    if tx.get("import_guard", "active").await?.is_some() {
        return Ok(Work::Idle);
    }
    let paused = dispatch(&tx).await?["paused"] != false;
    let t = clock();
    let Some(mut a) = ready_attempt(&tx, paused, t.timestamp()).await? else {
        return Ok(Work::Idle);
    };
    let key = req_str(&a, "id")?.to_owned();
    let submit_id = uid("ses");
    let per_issue = if a["kind"] == "campaign" {
        Some(sha256(
            format!(
                "{}/{}",
                req_str(&a, "issue_id")?,
                req_str(&a, "subscriber_id")?
            )
            .as_bytes(),
        ))
    } else {
        None
    };
    let previous = if let Some(k) = &per_issue {
        tx.get("edition_delivery", k).await?
    } else {
        None
    };
    let duplicate = previous.is_some_and(|p| p["attempt_id"] != key);
    let mail = if duplicate {
        None
    } else {
        build_mail(&mut tx, &a, &submit_id).await?
    };
    let Some(mail) = mail else {
        a["state"] = json!("skipped");
        a["updated_at"] = json!(now());
        bump(&mut a);
        tx.put("attempt", &key, a.clone())?;
        sync_related(&mut tx, &a).await?;
        tx.commit().await?;
        return Ok(Work::Skipped);
    };
    let settings = editorial::settings(&tx).await?;
    let budget = if local(env) {
        u64::MAX
    } else {
        settings["send_budget"].as_u64().unwrap_or(0)
    };
    if !capacity(&mut tx, quota, budget, &t).await? {
        return Ok(Work::Idle);
    }
    if let Some(k) = per_issue {
        tx.put("edition_delivery", &k, json!({"id":k,"attempt_id":key}))?;
    }
    a["state"] = json!("submitting");
    a["submission_id"] = json!(submit_id);
    a["lease_until"] = json!(t.timestamp() + FENCE_SECONDS);
    a["updated_at"] = json!(now());
    a["tries"] = json!(a["tries"].as_u64().unwrap_or(0) + 1);
    bump(&mut a);
    tx.put("attempt", &key, a)?;
    tx.put(
        "submission",
        &submit_id,
        json!({"id":submit_id,"attempt_id":key,"at":now()}),
    )?;
    tx.commit().await?;
    Ok(Work::Mail(key, mail))
}

async fn finish(env: &Env, id: &str, mail: &Mail, outcome: &Submission) -> Res<()> {
    let mut tx = Tx::begin(env, Actor::system()).await?;
    let mut a = tx.require("attempt", id).await?;
    if a["submission_id"] != mail.attempt_id
        || !matches!(a["state"].as_str(), Some("submitting" | "unknown"))
    {
        return Ok(());
    }
    let tries = a["tries"].as_u64().unwrap_or(1);
    let state = match outcome {
        Submission::Accepted(saved) => {
            a["provider_id"] = json!(saved);
            a["accepted_at"] = json!(now());
            "accepted"
        }
        Submission::Throttled if tries < 8 => {
            let delay = 15_i64 * (1_i64 << tries.min(6));
            a["retry_at"] = json!(
                clock().timestamp()
                    + delay
                    + i64::from(id.as_bytes().last().copied().unwrap_or_default() % 15)
            );
            "queued"
        }
        Submission::Throttled | Submission::Rejected => "rejected",
        Submission::Unknown => "unknown",
    };
    if local(env) && matches!(outcome, Submission::Accepted(_)) {
        let mut capture = serde_json::to_value(mail)?;
        capture["id"] = json!(id);
        capture["at"] = json!(now());
        tx.put("capture", id, capture)?;
    }
    a["state"] = json!(state);
    a["updated_at"] = json!(now());
    bump(&mut a);
    tx.put("attempt", id, a.clone())?;
    sync_related(&mut tx, &a).await?;
    tx.commit().await
}

pub async fn tick(env: &Env) -> Res<()> {
    discover(env).await?;
    let tx = Tx::begin(env, Actor::system()).await?;
    let paused = dispatch(&tx).await?["paused"] != false;
    if tx.get("import_guard", "active").await?.is_some()
        || ready_attempt(&tx, paused, clock().timestamp())
            .await?
            .is_none()
    {
        return Ok(());
    }
    provider::configuration(env)?;
    let quota = provider::quota(env).await?;
    let mut dispatched = 0;
    for _ in 0..MAX_PER_TICK {
        let (id, mail) = match next_submission(env, &quota).await? {
            Work::Idle => break,
            Work::Skipped => {
                dispatched += 1;
                continue;
            }
            Work::Mail(id, m) => (id, m),
        };
        let result = if local(env) {
            Submission::Accepted(format!("capture-{}", mail.attempt_id))
        } else {
            provider::submit(env, &mail).await
        };
        // A persistence conflict repeats only the local result write, never the provider submission.
        let mut saved = false;
        for _ in 0..3 {
            match finish(env, &id, &mail, &result).await {
                Ok(()) => {
                    saved = true;
                    break;
                }
                Err(e) if e.code == "concurrent_update" => (),
                Err(e) => return Err(e),
            }
        }
        if !saved {
            return Err(conflict("The provider outcome is pending reconciliation"));
        }
        dispatched += 1;
    }
    if dispatched == MAX_PER_TICK {
        env.queue("SEND_QUEUE")?
            .send(serde_json::json!({"kind":"delivery"}))
            .await?;
    }
    Ok(())
}

pub async fn record_feedback(tx: &mut Tx<'_>, event: &Feedback) -> Res<()> {
    if matches!(event.kind.as_str(), "hard_bounce" | "complaint") {
        for addr in &event.addresses {
            audience::apply_feedback_suppression(tx, addr, &event.kind).await?;
        }
    }
    let Some(tag) = event.attempt_id.as_ref() else {
        return Ok(());
    };
    let Some(submission) = tx.get("submission", tag).await? else {
        return Ok(());
    };
    let id = req_str(&submission, "attempt_id")?;
    let mut a = tx.require("attempt", id).await?;
    if matches!(event.kind.as_str(), "hard_bounce" | "complaint") && event.addresses.is_empty() {
        if let Some(sub) = tx.get("subscriber", req_str(&a, "subscriber_id")?).await? {
            if let Some(email) = sub["email"].as_str() {
                audience::apply_feedback_suppression(tx, email, &event.kind).await?
            }
        }
    }
    if a["submission_id"] != *tag {
        return Ok(());
    }
    let kind = event.kind.as_str();
    let former = a["state"].as_str().unwrap_or("");
    let next = match (former, kind) {
        ("complaint", _) | (_, "complaint") => "complaint",
        ("hard_bounce", _) | (_, "hard_bounce") => "hard_bounce",
        ("delivered", _) | (_, "delivered") => "delivered",
        ("rejected", _) | (_, "rejected") => "rejected",
        ("transient_bounce", _) | (_, "transient_bounce") => "transient_bounce",
        _ => "accepted",
    };
    if former == next
        && a["provider_id"] == event.message_id
        && !a["accepted_at"].is_null()
        && (kind != "delivered" || !a["delivered_at"].is_null())
    {
        return Ok(());
    }
    if a["accepted_at"].is_null() {
        a["accepted_at"] = json!(event.at);
    }
    if kind == "delivered" && a["delivered_at"].is_null() {
        a["delivered_at"] = json!(event.at);
    }
    a["state"] = json!(next);
    a["provider_id"] = json!(event.message_id);
    a["updated_at"] = json!(now());
    bump(&mut a);
    tx.put("attempt", id, a.clone())?;
    sync_related(tx, &a).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn click_links_are_bound_to_issue_recipient_and_classification() {
        let origin = "https://publication.example";
        let html = r#"<p><a href="https://sources.example/story?a=1&amp;b=2" data-purpose="sponsor">Sponsor</a> <a href="https://publication.example/unsubscribe/proof">Leave</a></p>"#;
        let plaintext = "See https://sources.example/story?a=1&b=2.";
        let (html, text, links) =
            message_links(html, plaintext, origin, "issue-9", "attempt-2", "secret").unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0]["purpose"], "sponsor");
        assert_eq!(links[0]["url"], "https://sources.example/story?a=1&b=2");
        assert!(html.contains("https://publication.example/unsubscribe/proof"));
        let id = links[0]["id"].as_str().unwrap();
        let token = link_token("secret", "attempt-2", id);
        assert!(text.ends_with(&format!("/r/attempt-2/{id}/{token}.")));
        assert_ne!(token, link_token("secret", "other-attempt", id));
    }
    #[test]
    fn unsubscribe_footer_inside_body_escapes_the_link() {
        let rendered = footer(
            "<HTML><BODY>Hi</BODY></HTML>",
            "https://site.example/a?x=1&y=2",
        );
        assert!(rendered.contains("x=1&amp;y=2"));
        assert!(rendered.ends_with("</p></BODY></HTML>"));
    }
}
