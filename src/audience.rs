use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

const CAMPAIGN_KEYS: [&str; 4] = ["utm_source", "utm_medium", "utm_campaign", "utm_content"];
const PROOF_TTL_SECONDS: i64 = 24 * 3600;
const RESEND_SECONDS: i64 = 120;

pub fn email_key(email: &str) -> Option<String> {
    let email = email.trim();
    let (local, domain) = email.rsplit_once('@')?;
    if local.is_empty()
        || local.len() > 64
        || email.len() > 254
        || domain.is_empty()
        || local.contains('@')
        || domain.starts_with('.')
        || domain.ends_with('.')
        || email.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return None;
    }
    // Keep aliases and dots intact. Treat the domain case-insensitively.
    Some(format!("{local}@{}", domain.to_ascii_lowercase()))
}

pub fn eligible(sub: &Value) -> bool {
    sub["status"] == "active" && sub["email"].is_string() && sub["suppression_reason"].is_null()
}

pub fn matches(sub: &Value, filter: &Value) -> bool {
    let tags = sub["tags"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if let Some(statuses) = filter["statuses"].as_array() {
        if !statuses.is_empty() && !statuses.contains(&sub["status"]) {
            return false;
        }
    }
    if let Some(all) = filter["tags_all"].as_array() {
        if !all.iter().all(|v| tags.contains(v)) {
            return false;
        }
    }
    if let Some(any) = filter["tags_any"].as_array() {
        if !any.is_empty() && !any.iter().any(|v| tags.contains(v)) {
            return false;
        }
    }
    if let Some(campaign) = filter["campaign"].as_str() {
        if sub["first_touch"]["utm_campaign"] != campaign {
            return false;
        }
    }
    if let Some(test_only) = filter["test_only"].as_bool() {
        if (sub["status"] == "test_only") != test_only {
            return false;
        }
    }
    for (bound, lower) in [("confirmed_since", true), ("confirmed_before", false)] {
        if let Some(value) = filter[bound].as_str() {
            let Some(time) = sub["confirmed_at"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            else {
                return false;
            };
            let Ok(cut) = chrono::DateTime::parse_from_rfc3339(value) else {
                return false;
            };
            if (lower && time < cut) || (!lower && time >= cut) {
                return false;
            }
        }
    }
    true
}

fn attribution(input: &Value) -> Value {
    let mut result = json!({});
    for key in CAMPAIGN_KEYS {
        if let Some(value) = input[key].as_str() {
            if !value.is_empty()
                && value.len() <= 128
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
            {
                result[key] = json!(value);
            }
        }
    }
    result
}

fn mac(secret: &str, data: &str) -> Hmac<Sha256> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(data.as_bytes());
    mac
}

pub fn address_digest(secret: &str, email: &str) -> String {
    URL_SAFE_NO_PAD.encode(
        mac(secret, &format!("address|{email}"))
            .finalize()
            .into_bytes(),
    )
}

fn proof(secret: &str, purpose: &str, subject: &str, nonce: &str) -> String {
    let body = format!("v1.{purpose}.{subject}.{nonce}");
    let signature = URL_SAFE_NO_PAD.encode(mac(secret, &body).finalize().into_bytes());
    format!("{body}.{signature}")
}

fn verify_proof<'a>(secret: &str, purpose: &str, token: &'a str) -> Option<(&'a str, &'a str)> {
    let (body, signature) = token.rsplit_once('.')?;
    let mut parts = body.split('.');
    if parts.next()? != "v1" || parts.next()? != purpose {
        return None;
    }
    let subject = parts.next()?;
    let nonce = parts.next()?;
    if subject.is_empty() || nonce.is_empty() || parts.next().is_some() {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(signature).ok()?;
    mac(secret, body).verify_slice(&bytes).ok()?;
    Some((subject, nonce))
}

pub fn unsubscribe_proof(secret: &str, sub: &Value) -> String {
    proof(
        secret,
        "leave",
        sub["id"].as_str().unwrap_or(""),
        &sub["consent_epoch"].as_u64().unwrap_or(1).to_string(),
    )
}

fn public_subscriber(value: &Value) -> Value {
    let mut out = json!({});
    for key in [
        "id",
        "version",
        "email",
        "status",
        "tags",
        "first_touch",
        "confirmed_at",
        "suppression_reason",
        "evidence_kind",
    ] {
        if !value[key].is_null() {
            out[key] = value[key].clone();
        }
    }
    out
}

use crate::core::{
    Actor, AppError, Res, Tx, conf, expect_version, local, now, page_values, req_str, uid,
};
use std::collections::BTreeSet;
use worker::{Env, Method, Request, Response};

pub fn proof_secret(env: &Env) -> Res<String> {
    match conf(env, "READER_PROOF_SECRET").filter(|v| v.len() >= 32) {
        Some(s) => Ok(s),
        None if local(env) => Ok("local-capture-newsman-reader-test-key".into()),
        None => Err(AppError::new(
            "configuration_missing",
            "Set READER_PROOF_SECRET to a random value of at least 32 bytes",
            503,
        )),
    }
}

fn epoch() -> i64 {
    chrono::DateTime::parse_from_rfc3339(&now())
        .expect("valid UTC clock")
        .timestamp()
}

pub async fn admit(tx: &mut Tx<'_>, hint: &str) -> Res<()> {
    let window = epoch() / 600;
    let id = address_digest(&proof_secret(tx.env)?, &format!("ip|{window}|{hint}"));
    let count = tx
        .get("reader_rate", &id)
        .await?
        .and_then(|v| v["count"].as_u64())
        .unwrap_or(0);
    if count >= 30 {
        return Err(AppError::new("rate_limited", "Please try again later", 429));
    }
    tx.put(
        "reader_rate",
        &id,
        json!({"id":id,"count":count+1,"expires_at":(window+1)*600}),
    )
}

pub async fn lookup(tx: &Tx<'_>, email: &str) -> Res<Option<Value>> {
    let key =
        email_key(email).ok_or_else(|| AppError::invalid("A valid email address is required"))?;
    let digest = address_digest(&proof_secret(tx.env)?, &key);
    if let Some(index) = tx.get("subscriber_key", &digest).await? {
        return tx
            .get("subscriber", req_str(&index, "subscriber_id")?)
            .await;
    }
    Ok(None)
}

pub fn new_subscriber(email: &str) -> Value {
    json!({"id":uid("sub"),"version":1,"email":email,"status":"pending","tags":[],"first_touch":{},
        "created_at":now(),"consent_epoch":1})
}

pub fn persist_subscriber(tx: &mut Tx<'_>, sub: &Value) -> Res<()> {
    let id = req_str(sub, "id")?;
    let digest = match sub["address_digest"].as_str() {
        Some(d) => d.to_owned(),
        None => address_digest(
            &proof_secret(tx.env)?,
            &email_key(req_str(sub, "email")?)
                .ok_or_else(|| AppError::invalid("Invalid address"))?,
        ),
    };
    let mut body = sub.clone();
    body["address_digest"] = json!(digest);
    tx.put(
        "subscriber_key",
        &digest,
        json!({"id":digest,"subscriber_id":id}),
    )?;
    tx.put("subscriber", id, body)
}

pub fn consent_event(
    tx: &mut Tx<'_>,
    id: &str,
    kind: &str,
    source: &str,
    reason: Option<&str>,
) -> Res<()> {
    let event_id = uid("ev");
    let mut e = json!({"id":event_id,"at":now(),"kind":kind,"object_id":id,"source":source});
    if tx.actor.id != Actor::reader().id {
        e["actor_id"] = json!(tx.actor.id);
    }
    if let Some(r) = reason {
        e["reason"] = json!(r);
    }
    tx.put("consent_event", &event_id, e)
}

fn clear_challenge(s: &mut Value) {
    for k in ["challenge_nonce", "challenge_until"] {
        s.as_object_mut().unwrap().remove(k);
    }
}

fn increment(s: &mut Value) {
    s["version"] = json!(s["version"].as_u64().unwrap_or(0) + 1);
}

pub async fn apply_feedback_suppression(tx: &mut Tx<'_>, email: &str, reason: &str) -> Res<()> {
    let email = email_key(email).ok_or_else(|| AppError::invalid("Invalid feedback address"))?;
    let existing = lookup(tx, &email).await?;
    let mut sub = existing.clone().unwrap_or_else(|| new_subscriber(&email));
    if sub["status"] == "erased"
        || (sub["status"] == "suppressed"
            && (sub["suppression_reason"] == reason || sub["suppression_reason"] == "complaint"))
    {
        return Ok(());
    }
    sub["suppression_reason"] = json!(reason);
    sub["status"] = json!("suppressed");
    clear_challenge(&mut sub);
    if existing.is_some() {
        increment(&mut sub);
    }
    persist_subscriber(tx, &sub)?;
    consent_event(
        tx,
        req_str(&sub, "id")?,
        "suppressed",
        "provider",
        Some(reason),
    )
}

/// Erasure retains only the keyed address tombstone needed to block future mail.
pub async fn erase_record(tx: &mut Tx<'_>, sub: &Value, version: u64) -> Res<()> {
    let id = req_str(sub, "id")?;
    let email = sub["email"].as_str().and_then(email_key);
    for (kind, field) in [
        ("consent_event", "object_id"),
        ("mail_queue", "subscriber_id"),
        ("historical_consent", "subscriber_id"),
    ] {
        for item in tx.list(kind).await? {
            let matches_address = kind == "historical_consent"
                && email.is_some()
                && item["email"].as_str().and_then(email_key) == email;
            if item[field] == id || matches_address {
                tx.delete(kind, req_str(&item, "id")?)?;
            }
        }
    }
    if local(tx.env) && email.is_some() {
        for captured in tx.list("capture").await? {
            if captured["to"].as_str().and_then(email_key) == email {
                tx.delete("capture", req_str(&captured, "id")?)?;
            }
        }
    }
    for receipt in tx.list("receipt").await? {
        if receipt["result"]["id"] == id && receipt["result"]["email"].is_string() {
            if let Some(key) = receipt["id"].as_str() {
                tx.delete("receipt", key)?;
            }
        }
    }
    let digest = match sub["address_digest"].as_str() {
        Some(v) => v.to_owned(),
        None => address_digest(
            &proof_secret(tx.env)?,
            email
                .as_deref()
                .ok_or_else(|| AppError::invalid("Missing address key"))?,
        ),
    };
    persist_subscriber(
        tx,
        &json!({"id":id,"version":version,"status":"erased","tags":[],"first_touch":{},"address_digest":digest}),
    )
}

fn check_inbox() -> Value {
    json!({"outcome":"check_inbox","next_path":"/subscribe/#check-inbox"})
}

async fn signup(tx: &mut Tx<'_>, a: &Value, resend: bool) -> Res<Value> {
    let email = email_key(req_str(a, "email")?)
        .ok_or_else(|| AppError::invalid("A valid email address is required"))?;
    if !crate::email_login::pilot_permitted(tx.env, &email) {
        return Ok(check_inbox());
    }
    let entry = lookup(tx, &email).await?;
    if resend && entry.is_none() {
        return Ok(check_inbox());
    }
    let mut sub = entry.clone().unwrap_or_else(|| new_subscriber(&email));
    if !(sub["status"] == "pending" || sub["status"] == "unsubscribed")
        || (resend && sub["status"] == "unsubscribed" && sub["challenge_nonce"].is_null())
    {
        return Ok(check_inbox());
    }
    let t = epoch();
    if t - sub["last_challenge_at"].as_i64().unwrap_or(0) < RESEND_SECONDS {
        return Ok(check_inbox());
    }
    let day = t / 86400;
    let count = if sub["challenge_day"].as_i64() == Some(day) {
        sub["challenge_count"].as_i64().unwrap_or(0)
    } else {
        0
    };
    if count >= 5 {
        return Ok(check_inbox());
    }
    let settings = crate::editorial::settings(tx).await?;
    let nonce = uid("c");
    let id = req_str(&sub, "id")?.to_owned();
    if entry.is_none() {
        sub["first_touch"] = attribution(&a["campaign"]);
    } else {
        increment(&mut sub);
    }
    sub["challenge_nonce"] = json!(nonce);
    sub["challenge_until"] = json!(t + PROOF_TTL_SECONDS);
    sub["last_challenge_at"] = json!(t);
    sub["challenge_day"] = json!(day);
    sub["challenge_count"] = json!(count + 1);
    sub["consent_version"] = settings["consent_version"].clone();
    persist_subscriber(tx, &sub)?;
    if entry.is_none() {
        consent_event(tx, &id, "signup_pending", "reader", None)?;
    }
    let token = proof(&proof_secret(tx.env)?, "join", &id, &nonce);
    let origin = req_str(&settings, "public_origin")?.trim_end_matches('/');
    let link = format!("{origin}/confirm/{token}");
    let publication = settings["publication_name"]
        .as_str()
        .unwrap_or("this mailing list");
    let message = format!(
        "Confirm your subscription to {publication} by opening this link and choosing Confirm.\n\n{link}"
    );
    let body = format!(
        "<p>Confirm your subscription to {}.</p><p><a href=\"{}\">Review and confirm</a></p>",
        escape(publication),
        escape(&link)
    );
    let q = uid("mail");
    tx.put("mail_queue",&q,json!({"id":q,"kind":"confirmation","subscriber_id":id,"nonce":nonce,"state":"queued","version":1,
        "created_at":now(),"subject":format!("Confirm your subscription to {publication}"),"text":message,"html":body}))?;
    Ok(check_inbox())
}

async fn consume(tx: &mut Tx<'_>, a: &Value, join: bool) -> Res<Value> {
    let secret = proof_secret(tx.env)?;
    let token = req_str(a, "token")?;
    let Some((id, nonce)) = verify_proof(&secret, if join { "join" } else { "leave" }, token)
    else {
        return Ok(json!({"outcome":"expired"}));
    };
    let Some(mut sub) = tx.get("subscriber", id).await? else {
        return Ok(json!({"outcome":"expired"}));
    };
    if join {
        if sub["status"] == "active" && sub["last_used_nonce"] == nonce {
            return Ok(json!({"outcome":"already_handled","next_path":"/confirmed/"}));
        }
        if sub["challenge_nonce"] != nonce
            || epoch() >= sub["challenge_until"].as_i64().unwrap_or(0)
            || !(sub["status"] == "pending" || sub["status"] == "unsubscribed")
        {
            return Ok(json!({"outcome":"expired"}));
        }
        let first = sub["first_confirmation_recorded"] != true;
        if first {
            sub["confirmed_at"] = json!(now());
            sub["first_confirmation_recorded"] = json!(true);
        }
        sub["consent_epoch"] = json!(sub["consent_epoch"].as_u64().unwrap_or(0) + 1);
        sub["last_used_nonce"] = json!(nonce);
        sub["status"] = json!("active");
        sub["evidence_kind"] = json!("double_opt_in");
        sub.as_object_mut().unwrap().remove("suppression_reason");
        clear_challenge(&mut sub);
        increment(&mut sub);
        persist_subscriber(tx, &sub)?;
        consent_event(
            tx,
            id,
            if first {
                "first_confirmed"
            } else {
                "reconfirmed"
            },
            "reader",
            None,
        )?;
        Ok(json!({"outcome":"confirmed","next_path":"/confirmed/"}))
    } else {
        if matches!(
            sub["status"].as_str(),
            Some("erased" | "suppressed" | "unsubscribed" | "held" | "test_only")
        ) {
            return Ok(json!({"outcome":"unsubscribed"}));
        }
        // Previously delivered messages can always opt out their recipient.
        if nonce
            .parse::<u64>()
            .ok()
            .is_none_or(|n| n > sub["consent_epoch"].as_u64().unwrap_or(1))
        {
            return Ok(json!({"outcome":"expired"}));
        }
        sub["status"] = json!("unsubscribed");
        sub["suppression_reason"] = json!("reader_opt_out");
        clear_challenge(&mut sub);
        increment(&mut sub);
        persist_subscriber(tx, &sub)?;
        consent_event(tx, id, "unsubscribed", "reader", None)?;
        Ok(json!({"outcome":"unsubscribed"}))
    }
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub async fn run(tx: &mut Tx<'_>, name: &str, a: &Value) -> Res<Value> {
    match name {
        "reader_info" => {
            let s = crate::editorial::settings(tx).await?;
            let mut r = json!({"publication_name":s["publication_name"],"public_origin":s["public_origin"],"consent_version":s["consent_version"]});
            for k in ["archive_url", "feed_url", "next_issue_at"] {
                if !s[k].is_null() {
                    r[k] = s[k].clone();
                } else if matches!(k, "archive_url" | "feed_url") {
                    if let Some(dest) = conf(tx.env, &k.to_ascii_uppercase())
                        .and_then(|u| url::Url::parse(&u).ok())
                        .filter(|u| u.scheme() == "https")
                    {
                        r[k] = json!(dest.as_str());
                    }
                }
            }
            Ok(r)
        }
        "reader_subscribe" | "reader_resend" => signup(tx, a, name == "reader_resend").await,
        "reader_confirm" | "reader_unsubscribe" => consume(tx, a, name == "reader_confirm").await,
        "subscribers_list" | "segment_preview" => {
            let search = a["search"].as_str().unwrap_or("").trim().to_lowercase();
            let docs = tx
                .list("subscriber")
                .await?
                .into_iter()
                .filter(|s| {
                    matches(s, &a["filter"])
                        && (search.is_empty()
                            || s["email"]
                                .as_str()
                                .is_some_and(|v| v.to_lowercase().contains(&search))
                            || s["id"].as_str().is_some_and(|v| v == search))
                })
                .map(|s| public_subscriber(&s))
                .collect::<Vec<_>>();
            let count = docs.len();
            let mut out = page_values(docs, a)?;
            if name == "segment_preview" {
                out["estimated"] = json!(count);
            }
            Ok(out)
        }
        "subscriber_get" => Ok(public_subscriber(
            &tx.require("subscriber", req_str(a, "id")?).await?,
        )),
        "subscriber_history" => {
            let id = req_str(a, "id")?;
            tx.require("subscriber", id).await?;
            page_values(
                tx.list("consent_event")
                    .await?
                    .into_iter()
                    .filter(|v| v["object_id"] == id)
                    .collect(),
                a,
            )
        }
        "subscriber_tags_set" | "subscriber_suppress" | "subscriber_erase" => {
            let id = req_str(a, "id")?;
            let mut sub = tx.require("subscriber", id).await?;
            expect_version(&sub, a)?;
            if sub["status"] == "erased" {
                return Err(AppError::new("not_found", "Subscriber was erased", 404));
            }
            increment(&mut sub);
            if name == "subscriber_erase" {
                erase_record(tx, &sub, sub["version"].as_u64().unwrap_or(1)).await?;
                return Ok(json!({"id":id,"version":sub["version"]}));
            }
            if name == "subscriber_tags_set" {
                let tags: BTreeSet<&str> = a["tags"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                sub["tags"] = json!(tags);
            } else {
                sub["status"] = json!("suppressed");
                if sub["suppression_reason"] != "complaint"
                    && sub["suppression_reason"] != "hard_bounce"
                {
                    sub["suppression_reason"] = json!("administrative");
                }
                clear_challenge(&mut sub);
                consent_event(tx, id, "suppressed", "operator", a["reason"].as_str())?;
            }
            persist_subscriber(tx, &sub)?;
            Ok(public_subscriber(&sub))
        }
        "segments_list" => page_values(tx.list("segment").await?, a),
        "segment_create" | "segment_update" | "segment_delete" => {
            let id = if name == "segment_create" {
                uid("seg")
            } else {
                req_str(a, "id")?.to_owned()
            };
            let version = if name == "segment_create" {
                1
            } else {
                let s = tx.require("segment", &id).await?;
                expect_version(&s, a)?;
                s["version"].as_u64().unwrap_or(0) + 1
            };
            if name == "segment_delete" {
                tx.delete("segment", &id)?;
                return Ok(json!({"id":id,"version":version}));
            }
            let doc = json!({"id":id,"version":version,"name":a["name"],"filter":a["filter"]});
            tx.put("segment", &id, doc.clone())?;
            Ok(doc)
        }
        _ => Err(AppError::new("not_found", "Unknown audience action", 404)),
    }
}

fn html_page(title: &str, body: &str) -> Res<Response> {
    let mut r = Response::from_html(format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><main><h1>{}</h1>{body}</main></html>",
        escape(title),
        escape(title)
    ))?;
    r.headers_mut().set("Cache-Control", "private, no-store")?;
    r.headers_mut().set("Referrer-Policy", "no-referrer")?;
    r.headers_mut().set(
        "Content-Security-Policy",
        "default-src 'none'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
    )?;
    Ok(r)
}

fn redirect(path: &str) -> Res<Response> {
    let mut r = Response::empty()?.with_status(303);
    r.headers_mut().set("Location", path)?;
    r.headers_mut().set("Cache-Control", "no-store")?;
    Ok(r)
}

pub async fn http(mut req: Request, env: &Env) -> Res<Response> {
    let url = req.url()?;
    let path = url.path().trim_end_matches('/');
    let method = req.method();
    let ip = req
        .headers()
        .get("CF-Connecting-IP")?
        .unwrap_or_else(|| "direct".into());
    let info = crate::execute_reader(env, "reader_info", json!({}), &ip).await?;
    let title = info["publication_name"].as_str().unwrap_or("Mailing list");
    if method == Method::Get && path == "/subscribe" {
        let tagged = attribution(
            &url.query_pairs()
                .into_owned()
                .map(|(k, v)| (k, json!(v)))
                .collect::<serde_json::Map<_, _>>()
                .into(),
        );
        let inputs = tagged
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                format!(
                    "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
                    escape(k),
                    escape(v.as_str().unwrap_or(""))
                )
            })
            .collect::<String>();
        return html_page(
            &format!("Subscribe to {title}"),
            &format!(
                "<form action=\"/api/newsletter/subscribe\" method=\"post\"><label>Email address <input type=\"email\" name=\"email\" autocomplete=\"email\" maxlength=\"254\" required></label>{inputs}<button>Subscribe</button></form><p id=\"check-inbox\">After subscribing, check your inbox to confirm.</p>"
            ),
        );
    }
    if method == Method::Get && path == "/confirmed" {
        return html_page(
            "Subscription confirmed",
            &format!("<p>Thank you for subscribing to {}.</p>", escape(title)),
        );
    }
    let scope = if let Some(token) = path.strip_prefix("/confirm/") {
        Some(("reader_confirm", token))
    } else {
        path.strip_prefix("/unsubscribe/")
            .map(|t| ("reader_unsubscribe", t))
    };
    if let Some((name, token)) = scope {
        if token.len() > 1024 || token.is_empty() {
            return Err(AppError::invalid("Invalid reader proof"));
        }
        let caption = if name == "reader_confirm" {
            "Confirm subscription"
        } else {
            "Unsubscribe"
        };
        if method == Method::Get {
            return html_page(
                caption,
                &format!("<form method=\"post\"><button>{caption}</button></form>"),
            );
        }
        if method == Method::Post {
            let out = crate::execute_reader(env, name, json!({"token":token}), &ip).await?;
            if out["outcome"] == "expired" {
                return Ok(html_page(
                    "Link expired",
                    "<p><a href=\"/subscribe/\">Request a new confirmation link</a>.</p>",
                )?
                .with_status(410));
            }
            if name == "reader_confirm" {
                return redirect("/confirmed/");
            }
            return html_page(
                "Unsubscribed",
                "<p>You have been removed from future mailings.</p>",
            );
        }
    }
    if method == Method::Post
        && (path == "/api/newsletter/subscribe" || path == "/api/newsletter/resend")
    {
        let origin = req.headers().get("Origin")?;
        if origin
            .is_some_and(|o| o != url.origin().ascii_serialization() && info["public_origin"] != o)
        {
            return Err(AppError::new(
                "permission_denied",
                "Cross-origin form is unavailable",
                403,
            ));
        }
        let bytes = crate::bounded_body(&mut req, 8192).await?;
        let fields: Value = url::form_urlencoded::parse(&bytes)
            .into_owned()
            .map(|(k, v)| (k, json!(v)))
            .collect::<serde_json::Map<_, _>>()
            .into();
        let name = if path.ends_with("resend") {
            "reader_resend"
        } else {
            "reader_subscribe"
        };
        let mut input = json!({"email":fields["email"]});
        if name == "reader_subscribe" {
            input["campaign"] = attribution(&fields);
        }
        crate::execute_reader(env, name, input, &ip).await?;
        return redirect("/subscribe/#check-inbox");
    }
    Err(AppError::new(
        "method_not_allowed",
        "Unsupported reader method",
        405,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn separate_proof_purposes() {
        let secret = "long-local-test-secret";
        let p = proof(secret, "join", "sub_one", "nonce");
        assert_eq!(verify_proof(secret, "join", &p), Some(("sub_one", "nonce")));
        assert_eq!(verify_proof(secret, "leave", &p), None);
        assert_eq!(verify_proof("different", "join", &p), None);
    }
    #[test]
    fn address_and_segment() {
        assert_eq!(
            email_key(" A+b@Example.ORG "),
            Some("A+b@example.org".into())
        );
        assert!(email_key("bad@@example.org").is_none());
        let s = json!({"status":"active","email":"x@example.test","tags":["member"],"confirmed_at":"2020-01-02T00:00:00Z"});
        assert!(eligible(&s));
        assert!(matches(
            &s,
            &json!({"tags_all":["member"],"confirmed_since":"2020-01-01T19:00:00-05:00"})
        ));
        assert!(!matches(
            &s,
            &json!({"confirmed_before":"2020-01-02T00:00:00Z"})
        ));
    }
}
