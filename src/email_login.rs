//! Browser-bound, single-use email codes for a deployment-managed operator roster.
use crate::access::{self, Verified};
use crate::core::{self, Actor, AppError, Res, Tx, conf, req_str, sha256};
use crate::identity_ops::normalized_email;
use crate::provider::{Mail, Submission};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use worker::{Env, Method, Request, Response};

const PENDING: &str = "__Host-newsman-challenge";
const SESSION: &str = "__Host-newsman-session";
const CODE_TTL: i64 = 600;
const SESSION_TTL: i64 = 12 * 3600;

pub fn enabled(env: &Env) -> bool {
    conf(env, "OPERATOR_AUTH_MODE").as_deref() == Some("email")
}
pub fn permitted(env: &Env, email: &str) -> bool {
    !enabled(env) || in_list(env, "OPERATOR_EMAILS", email)
}
pub fn pilot_permitted(env: &Env, email: &str) -> bool {
    env.var("PILOT_RECIPIENTS").is_err() || in_list(env, "PILOT_RECIPIENTS", email)
}
fn in_list(env: &Env, key: &str, email: &str) -> bool {
    conf(env, key).is_some_and(|s| s.split(',').any(|v| v.trim().eq_ignore_ascii_case(email)))
}
fn random<const N: usize>() -> Res<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| AppError::new("unavailable", "Randomness unavailable", 503))?;
    Ok(bytes)
}
fn token() -> Res<String> {
    Ok(URL_SAFE_NO_PAD.encode(random::<32>()?))
}
pub fn cookie(req: &Request, name: &str) -> Option<String> {
    let header = req.headers().get("Cookie").ok()??;
    header
        .split(';')
        .filter_map(|v| v.trim().split_once('='))
        .find(|(k, v)| *k == name && v.len() == 43)
        .map(|(_, v)| v.to_owned())
}
fn set_cookie(resp: &mut Response, name: &str, value: &str, ttl: i64) -> Res<()> {
    resp.headers_mut().append(
        "Set-Cookie",
        &format!("{name}={value}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={ttl}"),
    )?;
    Ok(())
}
fn digest(env: &Env, id: &str, code: &str) -> Res<String> {
    let secret = conf(env, "NEWSMAN_SECRET")
        .filter(|v| v.len() >= 32)
        .ok_or_else(|| {
            AppError::new(
                "configuration",
                "Configure the operator session secret",
                503,
            )
        })?;
    let mut h = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC key");
    h.update(b"newsman-login-v1\0");
    h.update(id.as_bytes());
    h.update(code.as_bytes());
    Ok(hex::encode(h.finalize().into_bytes()))
}
pub async fn actor(req: &Request, env: &Env) -> Res<Actor> {
    access::origin(req, env)?;
    let raw = cookie(req, SESSION).ok_or_else(access::unauth)?;
    let tx = Tx::begin(env, Actor::system()).await?;
    let s = tx
        .get("web_session", &sha256(raw.as_bytes()))
        .await?
        .filter(|v| v["expires_s"].as_i64().unwrap_or(0) > access::seconds())
        .ok_or_else(access::unauth)?;
    let m = tx
        .get("operator", req_str(&s, "operator_id")?)
        .await?
        .filter(|m| access::active_operator(env, m) && m["email"] == s["email"])
        .ok_or_else(access::unauth)?;
    Ok(Actor {
        id: req_str(&m, "id")?.into(),
        email: req_str(&m, "email")?.into(),
        scopes: access::SCOPES.iter().map(|s| s.to_string()).collect(),
        grant_id: None,
    })
}
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn next_path(input: &str) -> &str {
    if input.starts_with("/oauth/authorize?")
        && input.len() <= 4096
        && !input.chars().any(char::is_control)
    {
        input
    } else {
        "/"
    }
}
fn page(verify: bool, next: &str, message: &str) -> Res<Response> {
    let (label, attr, action, button) = if verify {
        (
            "Email code",
            r#"name="code" type="text" inputmode="numeric" autocomplete="one-time-code" pattern="[0-9]{8}" maxlength="8""#,
            "/login/verify",
            "Sign in",
        )
    } else {
        (
            "Email address",
            r#"name="email" type="email" autocomplete="email" maxlength="254""#,
            "/login/request",
            "Email me a code",
        )
    };
    let hint = if verify {
        "If your address is approved, check your inbox for an eight-digit code. It expires in 10 minutes."
    } else {
        "Use your approved operator email address."
    };
    let mut r = Response::from_html(format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Sign in · newsman</title><link rel="stylesheet" href="/styles.css"></head><body class="login-page"><main class="login-panel"><span class="login-brand">newsman</span><h1>Operator sign-in</h1><p>{hint}</p><p role="status">{msg}</p><form method="post" action="{action}"><label>{label}<input required {attr}></label><input type="hidden" name="next" value="{next}"><button type="submit">{button}</button></form></main></body></html>"#,
        msg = esc(message),
        next = esc(next_path(next))
    ))?;
    r.headers_mut().set("Content-Security-Policy","default-src 'none'; style-src 'self'; font-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'")?;
    crate::form_with_origin(r)
}
fn redirect(path: &str) -> Res<Response> {
    let mut r = Response::empty()?.with_status(303);
    r.headers_mut().set("Location", path)?;
    Ok(r)
}
pub fn signin_redirect(next: &str) -> Res<Response> {
    let q = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("next", next_path(next))
        .finish();
    redirect(&format!("/login?{q}"))
}
async fn request_code(env: &Env, email: &str, next: &str) -> Res<Response> {
    let raw = token()?;
    let mut response = page(true, next, "")?;
    set_cookie(&mut response, PENDING, &raw, CODE_TTL)?;
    let Ok(email) = normalized_email(email) else {
        return Ok(response);
    };
    if !permitted(env, &email) {
        return Ok(response);
    }
    let mut tx = Tx::begin(env, Actor::system()).await?;
    let rate_id = sha256(email.as_bytes());
    let rate = tx.get("login_rate", &rate_id).await?.unwrap_or(Value::Null);
    let at = access::seconds();
    let window = rate["start"].as_i64().unwrap_or(0);
    let used = if window > at - 3600 {
        rate["used"].as_u64().unwrap_or(0)
    } else {
        0
    };
    if rate["last"].as_i64().is_some_and(|last| last + 60 > at) || used >= 5 {
        // A resend while throttled must preserve the browser handle for the code already in flight.
        response.headers_mut().delete("Set-Cookie")?;
        return Ok(response);
    }
    let id = sha256(raw.as_bytes());
    let code = format!("{:08}", u64::from_be_bytes(random::<8>()?) % 100_000_000);
    if let Some(previous) = rate["challenge"].as_str() {
        tx.delete("login_challenge", previous)?;
    }
    tx.put(
        "login_rate",
        &rate_id,
        json!({"start":if used==0{at}else{window},"last":at,"used":used+1,"challenge":id}),
    )?;
    tx.put("login_challenge",&id,json!({"email":email,"digest":digest(env,&id,&code)?,"tries":0,"next":next_path(next),"expires_s":at+CODE_TTL}))?;
    let settings = crate::editorial::settings(&tx).await?;
    let mail = Mail {
        from: req_str(&settings, "from_address")?.into(),
        reply_to: req_str(&settings, "reply_to")?.into(),
        to: email,
        subject: "Your newsman sign-in code".into(),
        text: format!("Your newsman sign-in code is {code}. It expires in 10 minutes."),
        html: format!(
            "<p>Your newsman sign-in code is <strong>{code}</strong>. It expires in 10 minutes.</p>"
        ),
        unsubscribe: None,
        attempt_id: core::uid("login"),
        campaign_id: None,
    };
    if core::local(env) {
        tx.put("capture", &mail.attempt_id, serde_json::to_value(&mail)?)?;
    }
    tx.commit().await?;
    if !core::local(env)
        && !matches!(
            crate::provider::submit(env, &mail).await,
            Submission::Accepted(_)
        )
    {
        return Err(AppError::new(
            "mail_unavailable",
            "The sign-in email could not be delivered. Try again in a minute.",
            503,
        ));
    }
    Ok(response)
}
async fn verify(req: &Request, env: &Env, code: &str) -> Res<Response> {
    let invalid = || {
        Ok(page(
            true,
            "/",
            "The code is invalid or expired. You can request a new one.",
        )?
        .with_status(401))
    };
    let Some(raw) = cookie(req, PENDING) else {
        return invalid();
    };
    let id = sha256(raw.as_bytes());
    let mut tx = Tx::begin(env, Actor::system()).await?;
    let Some(mut challenge) = tx.get("login_challenge", &id).await? else {
        return invalid();
    };
    let email = req_str(&challenge, "email")?.to_owned();
    if !permitted(env, &email) || challenge["expires_s"].as_i64().unwrap_or(0) <= access::seconds()
    {
        return invalid();
    }
    let expected = req_str(&challenge, "digest")?;
    if code.len() != 8
        || !bool::from(
            expected
                .as_bytes()
                .ct_eq(digest(env, &id, code)?.as_bytes()),
        )
    {
        let tries = challenge["tries"].as_u64().unwrap_or(0) + 1;
        if tries >= 5 {
            tx.delete("login_challenge", &id)?
        } else {
            challenge["tries"] = json!(tries);
            tx.put("login_challenge", &id, challenge)?
        }
        tx.commit().await?;
        return invalid();
    }
    let identity = Verified {
        issuer: "newsman:email".into(),
        sub: email.clone(),
        email: email.clone(),
    };
    let actor = access::resolve_identity(env, &identity).await?;
    // Binding the first seat can write to D1. Re-read the one-time challenge in the session transaction.
    let mut tx = Tx::begin(env, actor.clone()).await?;
    access::revalidate(&tx).await?;
    if tx.get("login_challenge", &id).await?.is_none_or(|c| {
        c["digest"] != challenge["digest"]
            || c["expires_s"].as_i64().unwrap_or(0) <= access::seconds()
    }) {
        return invalid();
    }
    let session = token()?;
    tx.delete("login_challenge", &id)?;
    tx.put(
        "web_session",
        &sha256(session.as_bytes()),
        json!({"operator_id":actor.id,"email":email,"expires_s":access::seconds()+SESSION_TTL}),
    )?;
    tx.commit().await?;
    let mut r = redirect(next_path(challenge["next"].as_str().unwrap_or("/")))?;
    set_cookie(&mut r, SESSION, &session, SESSION_TTL)?;
    set_cookie(&mut r, PENDING, "", 0)?;
    Ok(r)
}
pub async fn http(mut req: Request, env: &Env) -> Res<Response> {
    let origin = access::origin(&req, env)?;
    let url = req.url()?;
    if req.method() == Method::Get && url.path() == "/login" {
        let next = url
            .query_pairs()
            .find(|(k, _)| k == "next")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        return page(false, &next, "");
    }
    if req.method() != Method::Post {
        return Ok(Response::error("Use POST", 405)?);
    }
    if req.headers().get("Origin")?.as_deref() != Some(origin.as_str()) {
        return Err(AppError::new(
            "invalid_origin",
            "Open the Newsman sign-in page and submit the form from there.",
            403,
        ));
    }
    let bytes = crate::bounded_body(&mut req, 8192).await?;
    let fields = url::form_urlencoded::parse(&bytes)
        .into_owned()
        .collect::<std::collections::BTreeMap<_, _>>();
    let field = |k: &str| fields.get(k).map(String::as_str).unwrap_or_default();
    match url.path() {
        "/login/request" => request_code(env, field("email"), field("next")).await,
        "/login/verify" => verify(&req, env, field("code")).await,
        "/login/logout" => {
            if let Some(raw) = cookie(&req, SESSION) {
                let mut tx = Tx::begin(env, Actor::system()).await?;
                tx.delete("web_session", &sha256(raw.as_bytes()))?;
                tx.commit().await?
            }
            let mut r = redirect("/login")?;
            set_cookie(&mut r, SESSION, "", 0)?;
            Ok(r)
        }
        _ => Ok(Response::error("Not found", 404)?),
    }
}
