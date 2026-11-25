//! Cloudflare Access identifies human operators. OAuth delegates their domain permissions to MCP clients.
use crate::core::{Actor, AppError, Res, Tx, conf, local, req_str, sha256};
use crate::identity_ops::{invitation, normalized_email};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use hmac::{Hmac, Mac};
use rsa::{
    BigUint, RsaPublicKey,
    pkcs1v15::{Signature, VerifyingKey},
    signature::Verifier,
    traits::PublicKeyParts,
};
use serde_json::{Value, json};
use sha2::Sha256;
use std::cell::RefCell;
use url::Url;
use worker::{Env, Fetch, Request, Response};

#[path = "access_oauth.rs"]
mod oauth;
pub use oauth::http;
pub const SCOPES: [&str; 4] = ["read", "write", "send", "admin"];

pub(crate) fn unauth() -> AppError {
    AppError::new("unauthorized", "Operator authentication required", 401)
}
pub(crate) fn denied() -> AppError {
    AppError::new(
        "permission_denied",
        "Operator access is unavailable for this identity",
        403,
    )
}
fn config_error() -> AppError {
    AppError::new(
        "configuration",
        "Operator authentication is not configured",
        503,
    )
}
pub(crate) fn seconds() -> i64 {
    chrono::Utc::now().timestamp()
}
fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}
pub(crate) fn loopback(u: &Url) -> bool {
    matches!(u.host(), Some(url::Host::Domain("localhost")))
        || matches!(u.host(),Some(url::Host::Ipv4(ip)) if ip.is_loopback())
        || matches!(u.host(),Some(url::Host::Ipv6(ip)) if ip.is_loopback())
}
fn local_request(env: &Env, url: &Url) -> bool {
    local(env)
        && loopback(url)
        && conf(env, "PUBLIC_ORIGIN").is_none_or(|raw| {
            Url::parse(&raw).is_ok_and(|configured| {
                loopback(&configured) && configured.origin() == url.origin()
            })
        })
}
pub fn origin(req: &Request, env: &Env) -> Res<String> {
    let u = req.url()?;
    let actual = u.origin().ascii_serialization();
    if local_request(env, &u) {
        return Ok(actual);
    }
    let configured = conf(env, "PUBLIC_ORIGIN").ok_or_else(config_error)?;
    let configured = Url::parse(&configured).map_err(|_| config_error())?;
    if configured.scheme() != "https"
        || configured.username() != ""
        || configured.password().is_some()
        || configured.path() != "/"
        || configured.query().is_some()
        || configured.fragment().is_some()
        || configured.origin().ascii_serialization() != actual
    {
        return Err(denied());
    }
    Ok(actual)
}

#[derive(Clone, Debug)]
pub(crate) struct Verified {
    pub(crate) issuer: String,
    pub(crate) sub: String,
    pub(crate) email: String,
}
fn team_issuer(raw: &str) -> Res<String> {
    let v = raw.trim_end_matches('/');
    let value = if v.starts_with("https://") {
        v.to_owned()
    } else {
        format!("https://{v}")
    };
    let u = Url::parse(&value).map_err(|_| config_error())?;
    if u.scheme() != "https"
        || !u
            .host_str()
            .is_some_and(|v| v.ends_with(".cloudflareaccess.com"))
        || u.path() != "/"
        || u.port().is_some()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err(config_error());
    }
    Ok(u.origin().ascii_serialization())
}
fn decode_json(encoded: &str) -> Res<Value> {
    serde_json::from_slice(&B64.decode(encoded).map_err(|_| unauth())?).map_err(|_| unauth())
}
fn verified_claims(claims: &Value, issuer: &str, audience: &str, now: i64) -> Res<Verified> {
    if claims["iss"] != issuer
        || claims["type"] != "app"
        || claims["common_name"].is_string()
        || claims["exp"].as_i64().is_none_or(|t| t <= now)
        || claims["iat"].as_i64().is_none_or(|t| t > now + 30)
        || claims["nbf"].as_i64().is_some_and(|t| t > now + 30)
        || !(claims["aud"] == audience || strings(&claims["aud"]).iter().any(|a| a == audience))
    {
        return Err(unauth());
    }
    let sub = claims["sub"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 1024)
        .ok_or_else(unauth)?;
    let email =
        normalized_email(claims["email"].as_str().ok_or_else(unauth)?).map_err(|_| unauth())?;
    Ok(Verified {
        issuer: issuer.into(),
        sub: sub.into(),
        email,
    })
}
thread_local! {
    static KEYS:RefCell<Option<(String,i64,Vec<Value>)>>=const {RefCell::new(None)};
}
async fn access_key(issuer: &str, kid: &str) -> Res<Value> {
    let now = seconds();
    if let Some(result) = KEYS.with(|cache| {
        let cache = cache.borrow();
        let (cached, at, keys) = cache.as_ref()?;
        if cached != issuer || now - at >= 300 {
            return None;
        }
        keys.iter()
            .find(|k| k["kid"] == kid)
            .map(|k| Ok(k.clone()))
            .or_else(|| (now - at < 30).then(unauth).map(Err))
    }) {
        return result;
    }
    let uri = Url::parse(&format!("{issuer}/cdn-cgi/access/certs")).map_err(|_| config_error())?;
    let mut response = Fetch::Url(uri).send().await?;
    if response.status_code() != 200 {
        return Err(config_error());
    }
    let body: Value = response.json().await?;
    let keys = body["keys"]
        .as_array()
        .filter(|ks| ks.len() <= 32)
        .ok_or_else(config_error)?
        .clone();
    let found = keys.iter().find(|k| k["kid"] == kid).cloned();
    KEYS.with(|cache| *cache.borrow_mut() = Some((issuer.into(), now, keys)));
    found.ok_or_else(unauth)
}
fn verify_signature(key: &Value, message: &[u8], signature: &str) -> Res<()> {
    if key["kty"] != "RSA"
        || key.get("alg").is_some_and(|a| a != "RS256")
        || key.get("use").is_some_and(|u| u != "sig")
    {
        return Err(unauth());
    }
    let component = |k: &str| -> Res<BigUint> {
        let bs = B64
            .decode(key[k].as_str().ok_or_else(unauth)?)
            .map_err(|_| unauth())?;
        Ok(BigUint::from_bytes_be(&bs))
    };
    let rsa = RsaPublicKey::new(component("n")?, component("e")?).map_err(|_| unauth())?;
    if !(2048..=4096).contains(&rsa.n().bits()) {
        return Err(unauth());
    }
    let bytes = B64.decode(signature).map_err(|_| unauth())?;
    let sig = Signature::try_from(bytes.as_slice()).map_err(|_| unauth())?;
    VerifyingKey::<Sha256>::new(rsa)
        .verify(message, &sig)
        .map_err(|_| unauth())
}
async fn access_identity(req: &Request, env: &Env) -> Res<Verified> {
    origin(req, env)?;
    if local_request(env, &req.url()?) {
        let email =
            conf(env, "LOCAL_OPERATOR_EMAIL").unwrap_or_else(|| "operator@newsman.test".into());
        return Ok(Verified {
            issuer: "newsman:local".into(),
            sub: "local-development-operator".into(),
            email: normalized_email(&email)?,
        });
    }
    let encoded = req
        .headers()
        .get("Cf-Access-Jwt-Assertion")?
        .filter(|s| s.len() <= 16384)
        .ok_or_else(unauth)?;
    let parts: Vec<_> = encoded.split('.').collect();
    if parts.len() != 3 {
        return Err(unauth());
    }
    let header = decode_json(parts[0])?;
    if header["alg"] != "RS256" || header.get("crit").is_some() {
        return Err(unauth());
    }
    let kid = header["kid"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .ok_or_else(unauth)?;
    let issuer = team_issuer(&conf(env, "ACCESS_TEAM_DOMAIN").ok_or_else(config_error)?)?;
    let audience = conf(env, "ACCESS_AUD")
        .filter(|s| !s.is_empty())
        .ok_or_else(config_error)?;
    let claims = verified_claims(&decode_json(parts[1])?, &issuer, &audience, seconds())?;
    let key = access_key(&issuer, kid).await?;
    let (signing_input, _) = encoded.rsplit_once('.').ok_or_else(unauth)?;
    verify_signature(&key, signing_input.as_bytes(), parts[2])?;
    Ok(claims)
}
async fn bootstrap(env: &Env, verified: &Verified) -> Res<()> {
    let mut tx = Tx::begin(env, Actor::system()).await?;
    if tx.get("access_setup", "bootstrap").await?.is_some() {
        return Ok(());
    }
    let variable = if crate::email_login::enabled(env) {
        "OPERATOR_EMAILS"
    } else {
        "BOOTSTRAP_OPERATORS"
    };
    let raw = conf(env, variable).unwrap_or_default();
    let mut emails = std::collections::BTreeSet::new();
    for email in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        emails.insert(normalized_email(email)?);
    }
    if emails.is_empty() && verified.issuer == "newsman:local" {
        emails.insert(verified.email.clone());
    }
    if emails.is_empty() {
        return Err(denied());
    }
    for email in emails {
        let record = invitation(&email);
        tx.put("operator", req_str(&record, "id")?, record.clone())?;
    }
    tx.put("access_setup", "bootstrap", json!({"id":"bootstrap"}))?;
    tx.commit().await
}
pub(crate) async fn resolve_identity(env: &Env, verified: &Verified) -> Res<Actor> {
    if !crate::email_login::permitted(env, &verified.email) {
        return Err(denied());
    }
    bootstrap(env, verified).await?;
    let mut tx = Tx::begin(env, Actor::system()).await?;
    let members = tx.list("operator").await?;
    let mut op = members
        .into_iter()
        .find(|op| op["email"] == verified.email && op["state"] != "removed")
        .ok_or_else(denied)?;
    if op["state"] == "invited" {
        op["issuer"] = json!(verified.issuer);
        op["subject"] = json!(verified.sub);
        op["state"] = json!("active");
        op["version"] = json!(op["version"].as_u64().unwrap_or_default() + 1);
        tx.put("operator", req_str(&op, "id")?, op.clone())?;
        tx.commit().await?;
    } else if op["state"] != "active"
        || op["issuer"] != verified.issuer
        || op["subject"] != verified.sub
    {
        return Err(denied());
    }
    Ok(Actor {
        id: req_str(&op, "id")?.into(),
        email: verified.email.clone(),
        scopes: SCOPES.iter().map(|s| s.to_string()).collect(),
        grant_id: None,
    })
}
pub async fn web_actor(req: &Request, env: &Env) -> Res<Actor> {
    match conf(env, "OPERATOR_AUTH_MODE").as_deref() {
        Some("email") => return crate::email_login::actor(req, env).await,
        None | Some("access") => (),
        _ => return Err(config_error()),
    }
    let identity = access_identity(req, env).await?;
    for retry in 0..4 {
        match resolve_identity(env, &identity).await {
            Err(e) if e.code == "concurrent_update" && retry < 3 => continue,
            result => return result,
        }
    }
    unreachable!()
}
pub fn csrf(req: &Request, env: &Env, actor: &Actor) -> Res<String> {
    let canonical = origin(req, env)?;
    let local_mode = local_request(env, &req.url()?);
    let secret = conf(env, "NEWSMAN_SECRET")
        .filter(|s| s.len() >= 32)
        .or_else(|| local_mode.then(|| "newsman-loopback-only-local-session".to_string()))
        .ok_or_else(config_error)?;
    let assertion = if crate::email_login::enabled(env) {
        crate::email_login::cookie(req, "__Host-newsman-session").ok_or_else(unauth)?
    } else {
        req.headers()
            .get("Cf-Access-Jwt-Assertion")?
            .unwrap_or_default()
    };
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()).map_err(|_| config_error())?;
    for s in [
        "operator-csrf-v1",
        &canonical,
        &actor.id,
        &sha256(assertion.as_bytes()),
    ] {
        mac.update(s.as_bytes());
        mac.update(&[0]);
    }
    Ok(B64.encode(mac.finalize().into_bytes()))
}

pub(crate) fn active_operator(env: &Env, op: &Value) -> bool {
    op["state"] == "active"
        && op["email"]
            .as_str()
            .is_some_and(|email| crate::email_login::permitted(env, email))
}
pub async fn revalidate(tx: &Tx<'_>) -> Res<()> {
    if tx.actor.id == Actor::reader().id || tx.actor.id == Actor::system().id {
        return Ok(());
    }
    let op = tx
        .get("operator", &tx.actor.id)
        .await?
        .filter(|v| active_operator(tx.env, v))
        .ok_or_else(denied)?;
    if let Some(id) = &tx.actor.grant_id {
        let grant = tx
            .get("grant", id)
            .await?
            .filter(|v| {
                v["active"] == true
                    && v["operator_id"] == op["id"]
                    && v["expires_s"].as_i64().is_some_and(|s| s > seconds())
            })
            .ok_or_else(unauth)?;
        let scopes = strings(&grant["scopes"]);
        if tx.actor.scopes.iter().any(|s| !scopes.contains(s)) {
            return Err(denied());
        }
    }
    Ok(())
}

pub async fn bearer(req: &Request, env: &Env) -> Res<Actor> {
    let canonical = origin(req, env)?;
    let header = req.headers().get("Authorization")?.ok_or_else(unauth)?;
    let (scheme, token) = header.split_once(' ').ok_or_else(unauth)?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.len() != 43 {
        return Err(unauth());
    }
    let tx = Tx::begin(env, Actor::system()).await?;
    let token = tx
        .get("access_token", &sha256(token.as_bytes()))
        .await?
        .ok_or_else(unauth)?;
    if token["resource"] != format!("{canonical}{}", req.path())
        || token["expires_s"].as_i64().is_none_or(|t| t <= seconds())
    {
        return Err(unauth());
    }
    let grant = tx
        .get("grant", req_str(&token, "grant_id")?)
        .await?
        .filter(|g| {
            g["active"] == true
                && g["resource"] == token["resource"]
                && g["expires_s"].as_i64().is_some_and(|t| t > seconds())
        })
        .ok_or_else(unauth)?;
    let member = tx
        .get("operator", req_str(&grant, "operator_id")?)
        .await?
        .filter(|m| active_operator(env, m))
        .ok_or_else(unauth)?;
    Ok(Actor {
        id: req_str(&member, "id")?.into(),
        email: req_str(&member, "email")?.into(),
        scopes: strings(&token["scopes"]),
        grant_id: Some(req_str(&grant, "id")?.into()),
    })
}

/// Expired credentials are already rejected. Remove a bounded batch from cold storage at midnight UTC.
pub async fn cleanup(env: &Env) -> Res<()> {
    let at = seconds();
    if at.rem_euclid(86400) >= 60 {
        return Ok(());
    }
    env.d1("DB")?.prepare(
        "DELETE FROM newsman_records WHERE (kind,id) IN (SELECT kind,id FROM newsman_records WHERE kind IN ('access_token','oauth_refresh','oauth_code','oauth_consent','web_session','login_challenge') AND json_extract(body,'$.expires_s') <= ? LIMIT 500)"
    ).bind(&[wasm_bindgen::JsValue::from_f64(at as f64)])?.run().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_identity_window_and_audience() {
        let mut c = json!({"iss":"https://team.cloudflareaccess.com","sub":"user1","email":"Owner@Example.test","type":"app","aud":["seat"],"exp":120,"iat":90});
        assert_eq!(
            verified_claims(&c, "https://team.cloudflareaccess.com", "seat", 100)
                .unwrap()
                .email,
            "owner@example.test"
        );
        assert!(verified_claims(&c, "https://team.cloudflareaccess.com", "seat", 120).is_err());
        assert!(verified_claims(&c, "https://other.cloudflareaccess.com", "seat", 100).is_err());
        assert!(verified_claims(&c, "https://team.cloudflareaccess.com", "wrong", 100).is_err());
        c["common_name"] = json!("service");
        assert!(verified_claims(&c, "https://team.cloudflareaccess.com", "seat", 100).is_err());
    }
    #[test]
    fn checks_rs256_signature() {
        // Public fixture only. The private key was discarded after signing this message.
        let key = json!({"kty":"RSA","alg":"RS256","e":"AQAB","n":"1nEDN70VsQARkRu-4GweqqVfcE14iZVXWrQRb7mJdG9CX7J0yJtbvLrjd4CSCn9-vO9O-SrHy-LK7f-mftn7NoHIbNx4JhDhJfqkq1h9_EfbS3IuD5dNqpMcyjeY0WqPIv6y-wsndk9OzXAu4Q7UuzxNN70FG_s1soYAN0L6fI9U3MFLWsH6eZQML-5EkzeNZzA-5YMfcO_sXBA68BsVAZFyXsN0US-2-llbU4xmdNLzjMJSTltYyhcGz6fmy9Ar2UtFmIohi-ENuJN5Yd8353t8-FPakz9rBsCzwSKeE8_n1x_eqbCQ2lbPvH_sLAqoAfOwrJEQFFwzuVEviZrJ1Q"});
        let sig = "oExvR88lmzUWEXD1aDRgCoAd-kfNE7fGiIHMZ-wYG7_9i59KTifjfyA-xmvccbuXHIp1bGlXuZRt6GzvcZV-vVbs77HjpQtPmUifblgMYXlCwWkTslD_BxIJ8PiA7NqjQ2-Qcp3e0C7OXBa9Q1mrzWqXWsb3C3HmnWNeLU0T33fFHPktR6aWzVpbHfExyPCGvRIo_5SUADi63y60l09qSMn2-ucv51SnaqFGFkEffXuptEt-Wkf1us7tb2vpCDqRoDPgAesdsH6oIknBb3IG6liw2RmmTVI3r-jUcmsIW3o5_dkuaDOkATpyxDWZyqxbSEsIERG8rARPQsLlXuC-IQ";
        assert!(verify_signature(&key, b"newsman-access-verification", sig).is_ok());
        assert!(verify_signature(&key, b"altered-claims", sig).is_err());
    }
    #[test]
    fn localhost_is_exact() {
        assert!(loopback(&Url::parse("http://[::1]:8787").unwrap()));
        assert!(!loopback(&Url::parse("https://localhost.example").unwrap()));
        assert!(team_issuer("https://id.example@team.cloudflareaccess.com").is_err());
    }
}
