//! Exchange a deployment-owned, short-lived OIDC assertion for scoped AWS role credentials.
use crate::core::{AppError, Res, conf};
use crate::provider::Credentials;
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use chrono::{DateTime, Utc};
use rsa::{
    RsaPrivateKey,
    pkcs1v15::SigningKey,
    pkcs8::DecodePrivateKey,
    signature::{SignatureEncoding, Signer},
    traits::PublicKeyParts,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use worker::{Env, Fetch, Method, Request, RequestInit, RequestRedirect, Response};

const AUD: &str = "sts.amazonaws.com";
const SUBJECT: &str = "newsman-worker";
fn unavailable() -> AppError {
    AppError::new(
        "workload_identity_unavailable",
        "The AWS workload identity is unavailable",
        503,
    )
}
fn issuer(env: &Env) -> Res<String> {
    let origin = conf(env, "PUBLIC_ORIGIN").ok_or_else(unavailable)?;
    let u = url::Url::parse(&origin).map_err(|_| unavailable())?;
    if u.scheme() != "https" || u.origin().ascii_serialization() != origin.trim_end_matches('/') {
        return Err(unavailable());
    }
    Ok(format!(
        "{}/workload-identity",
        u.origin().ascii_serialization()
    ))
}
fn key(env: &Env) -> Res<RsaPrivateKey> {
    let pem = conf(env, "AWS_WORKLOAD_KEY").ok_or_else(unavailable)?;
    let bytes = STANDARD.decode(pem.trim()).map_err(|_| unavailable())?;
    RsaPrivateKey::from_pkcs8_der(&bytes).map_err(|_| unavailable())
}
fn jwk(k: &RsaPrivateKey) -> Value {
    let n = k.n().to_bytes_be();
    json!({"kty":"RSA","alg":"RS256","use":"sig","kid":URL_SAFE_NO_PAD.encode(Sha256::digest(&n)),"n":URL_SAFE_NO_PAD.encode(n),"e":URL_SAFE_NO_PAD.encode(k.e().to_bytes_be())})
}
pub fn discovery(req: &Request, env: &Env) -> Res<Response> {
    if conf(env, "AWS_ROLE_ARN").is_none() || req.method() != Method::Get {
        return Ok(Response::error("Not found", 404)?);
    }
    crate::access::origin(req, env)?;
    let iss = issuer(env)?;
    let data = match req.path().as_str() {
        "/workload-identity/.well-known/openid-configuration" => {
            json!({"issuer":iss,"jwks_uri":format!("{iss}/jwks.json"),"response_types_supported":["id_token"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["RS256"]})
        }
        "/workload-identity/jwks.json" => json!({"keys":[jwk(&key(env)?)]}),
        _ => return Ok(Response::error("Not found", 404)?),
    };
    let mut r = Response::from_json(&data)?;
    r.headers_mut()
        .set("Cache-Control", "public, max-age=300")?;
    Ok(r)
}
fn assertion(k: RsaPrivateKey, iss: &str, at: i64) -> String {
    let hdr = json!({"alg":"RS256","typ":"JWT","kid":jwk(&k)["kid"]});
    let claims = json!({"iss":iss,"sub":SUBJECT,"aud":AUD,"iat":at,"exp":at+300});
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(hdr.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let sig = SigningKey::<Sha256>::new(k).sign(input.as_bytes());
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()))
}
thread_local! {static CACHE:RefCell<Option<(String,i64,Credentials)>>=const{RefCell::new(None)};}
pub async fn credentials(env: &Env, role: &str, region: &str) -> Res<Credentials> {
    let iss = issuer(env)?;
    let cache_key = format!("{iss}|{role}|{region}");
    let at = Utc::now().timestamp();
    if let Some(c) = CACHE.with(|v| {
        v.borrow()
            .as_ref()
            .filter(|(id, exp, _)| id == &cache_key && *exp > at + 300)
            .map(|(_, _, c)| c.clone())
    }) {
        return Ok(c);
    }
    let jwt = assertion(key(env)?, &iss, at);
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("Action", "AssumeRoleWithWebIdentity")
        .append_pair("Version", "2011-06-15")
        .append_pair("RoleArn", role)
        .append_pair("RoleSessionName", "newsman-worker")
        .append_pair("WebIdentityToken", &jwt)
        .append_pair("DurationSeconds", "3600")
        .finish();
    let host = crate::provider::regional_host(region, "sts").ok_or_else(unavailable)?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_redirect(RequestRedirect::Manual)
        .with_body(Some(body.into()));
    let mut request = Request::new_with_init(&format!("https://{host}/"), &init)?;
    request
        .headers_mut()?
        .set("Content-Type", "application/x-www-form-urlencoded")?;
    let mut res = Fetch::Request(request).send().await?;
    if res.status_code() != 200 {
        return Err(unavailable());
    }
    let xml = res.text().await?;
    let doc = roxmltree::Document::parse(&xml).map_err(|_| unavailable())?;
    let node = doc
        .descendants()
        .find(|n| n.has_tag_name("Credentials"))
        .ok_or_else(unavailable)?;
    let field = |name| {
        node.children()
            .find(|n| n.has_tag_name(name))
            .and_then(|n| n.text())
            .ok_or_else(unavailable)
    };
    let creds = Credentials {
        access_key: field("AccessKeyId")?.into(),
        secret_key: field("SecretAccessKey")?.into(),
        session_token: Some(field("SessionToken")?.into()),
    };
    let exp = DateTime::parse_from_rfc3339(field("Expiration")?)
        .map_err(|_| unavailable())?
        .timestamp();
    CACHE.with(|v| *v.borrow_mut() = Some((cache_key, exp, creds.clone())));
    Ok(creds)
}
