//! Server-to-server adapter for a publication's existing signup and conversion service.
use crate::{
    audience,
    core::{Actor, AppError, Res, Tx, conf, req_str, sha256},
};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use worker::{Env, Method, Request, Response};

const BATCH: usize = 32;

pub fn record_event(tx: &mut Tx<'_>, sub: &Value, event: &str) -> Res<()> {
    let Some(created) = sub["publisher_signup_at"].as_i64() else {
        return Ok(());
    };
    let subscriber = req_str(sub, "id")?;
    let reference = sub["publisher_signup_id"].as_str();
    if event == "accepted" && reference.is_none() {
        return Ok(());
    }
    let id = format!("{event}_{subscriber}");
    let hash = reference
        .filter(|_| event == "confirmed")
        .and_then(|_| sub["email"].as_str())
        .map(|email| sha256(email.trim().to_lowercase().as_bytes()));
    tx.put(
        "publisher_event",
        &id,
        json!({"id":id,"type":event,"subscriber":subscriber,"signup_id":reference,
        "email_hash":hash,"campaign":if event == "accepted" { &sub["first_touch"] } else { &Value::Null },"created":created,"at":chrono::Utc::now().timestamp()}),
    )
}

async fn accept(tx: &mut Tx<'_>, input: &Value) -> Res<Value> {
    let email = audience::email_key(req_str(input, "email")?)
        .ok_or_else(|| AppError::invalid("Invalid email"))?;
    let reference = input["signup_id"].as_str();
    if !input["signup_id"].is_null()
        && !reference.is_some_and(|s| {
            uuid::Uuid::parse_str(s).is_ok_and(|u| u.hyphenated().to_string() == s)
        })
    {
        return Err(AppError::invalid("Invalid signup reference"));
    }
    let prior = audience::lookup(tx, &email).await?;
    if prior.as_ref().is_none_or(|s| {
        matches!(s["status"].as_str(), Some("pending" | "unsubscribed"))
            && (s["challenge_nonce"].is_null()
                || s["challenge_until"].as_i64().unwrap_or(0) <= chrono::Utc::now().timestamp())
    }) {
        audience::signup(tx, input, false, true).await?;
    }
    let sub = audience::lookup(tx, &email).await?;
    let receipt = sub
        .as_ref()
        .filter(|s| {
            reference.is_some()
                && s["publisher_signup_id"] == input["signup_id"]
                && s["publisher_signup_at"].is_i64()
                && matches!(s["status"].as_str(), Some("pending" | "active"))
        })
        .map(|s| json!({"id":s["id"],"created":s["publisher_signup_at"]}));
    Ok(json!({"accepted":true,"receipt":receipt}))
}

pub async fn http(mut req: Request, env: &Env) -> Res<Response> {
    let secret = conf(env, "PUBLISHER_INTEGRATION_KEY")
        .filter(|s| s.len() >= 32)
        .ok_or_else(|| AppError::new("not_found", "Integration unavailable", 404))?;
    let supplied = req.headers().get("Authorization")?.unwrap_or_default();
    if !bool::from(
        sha256(secret.as_bytes())
            .as_bytes()
            .ct_eq(sha256(supplied.strip_prefix("Bearer ").unwrap_or("").as_bytes()).as_bytes()),
    ) {
        return Err(AppError::new(
            "permission_denied",
            "Invalid publisher credential",
            403,
        ));
    }
    let path = req.path();
    let input = if req.method() == Method::Post {
        serde_json::from_slice(&crate::bounded_body(&mut req, 8192).await?)?
    } else {
        Value::Null
    };
    for attempt in 0..4 {
        let mut tx = Tx::begin(env, Actor::system()).await?;
        let result = match (req.method(), path.as_str()) {
            (Method::Post, "/integrations/publisher/subscribe") => accept(&mut tx, &input).await?,
            (Method::Get, "/integrations/publisher/events") => {
                json!({"items":tx.page("publisher_event","",BATCH).await?})
            }
            (Method::Post, "/integrations/publisher/ack") => {
                let ids = input["ids"]
                    .as_array()
                    .filter(|a| a.len() <= BATCH)
                    .ok_or_else(|| AppError::invalid("Invalid acknowledgement batch"))?;
                for id in ids {
                    let id = id
                        .as_str()
                        .filter(|s| {
                            s.strip_prefix("accepted_sub_")
                                .or_else(|| s.strip_prefix("confirmed_sub_"))
                                .is_some_and(|s| {
                                    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
                                })
                        })
                        .ok_or_else(|| AppError::invalid("Invalid confirmation ID"))?;
                    tx.delete("publisher_event", id)?;
                }
                json!({"acknowledged":ids.len()})
            }
            (Method::Post, "/integrations/publisher/opt-out") => {
                audience::legacy_opt_out(
                    &mut tx,
                    req_str(&input, "email")?,
                    input["at"]
                        .as_i64()
                        .ok_or_else(|| AppError::invalid("Missing opt-out time"))?,
                )
                .await?;
                json!({"accepted":true})
            }
            _ => return Err(AppError::new("not_found", "Unknown publisher route", 404)),
        };
        match tx.commit().await {
            Err(e) if e.code == "concurrent_update" && attempt < 3 => continue,
            other => other?,
        };
        return Ok(Response::from_json(&result)?);
    }
    unreachable!()
}
