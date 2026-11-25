//! OAuth authorization code and PKCE for the installation’s registered public clients.
//! Only a hash of a one-time code, access token, refresh token, or consent handle is persisted.
use super::*;
use crate::{bounded_body, core::uid};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use subtle::ConstantTimeEq;

const ACCESS_TTL: i64 = 900;
const GRANT_TTL: i64 = 30 * 24 * 3600;
#[path = "oauth_clients.rs"]
mod clients;
use clients::registered;

fn random() -> Res<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| config_error())?;
    Ok(B64.encode(bytes))
}
fn form_fields(bytes: &[u8]) -> Result<BTreeMap<String, String>, ()> {
    let mut fields = BTreeMap::new();
    for (k, v) in url::form_urlencoded::parse(bytes) {
        if fields.insert(k.into_owned(), v.into_owned()).is_some() {
            return Err(());
        }
    }
    Ok(fields)
}
fn text<'a>(f: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    f.get(key).map(String::as_str).unwrap_or_default()
}
fn as_json(status: u16, body: &Value) -> Res<Response> {
    let mut r = Response::from_json(body)?.with_status(status);
    r.headers_mut().set("Cache-Control", "no-store")?;
    Ok(r)
}
fn oauth_error(code: &str, desc: &str) -> Res<Response> {
    as_json(400, &json!({"error":code,"error_description":desc}))
}
fn redirect(url: &Url) -> Res<Response> {
    let mut r = Response::empty()?.with_status(303);
    r.headers_mut().set("Location", url.as_str())?;
    r.headers_mut().set("Cache-Control", "no-store")?;
    r.headers_mut().set("Referrer-Policy", "no-referrer")?;
    Ok(r)
}
fn authorization_error(
    callback: &str,
    issuer: &str,
    state: &str,
    code: &str,
    detail: &str,
) -> Res<Response> {
    let mut url = Url::parse(callback).map_err(|_| config_error())?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("iss", issuer)
            .append_pair("error", code)
            .append_pair("error_description", detail);
        if !state.is_empty() {
            query.append_pair("state", state);
        }
    }
    redirect(&url)
}
fn pkce_verifier(v: &str) -> bool {
    (43..=128).contains(&v.len())
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
}
fn pkce(v: &str) -> String {
    B64.encode(Sha256::digest(v.as_bytes()))
}
fn valid_challenge(v: &str) -> bool {
    v.len() == 43 && B64.decode(v).is_ok_and(|b| b.len() == 32)
}
fn parse_scopes(raw: &str) -> Option<Vec<String>> {
    let scopes = raw.split_ascii_whitespace().collect::<BTreeSet<_>>();
    if scopes.is_empty()
        || scopes
            .iter()
            .any(|s| !SCOPES.contains(s) && *s != "offline_access")
    {
        return None;
    }
    Some(scopes.into_iter().map(str::to_owned).collect())
}
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn dated(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .expect("timestamp in range")
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn allowed_resource(env: &Env, canonical: &str, requested: &str) -> bool {
    requested == format!("{canonical}/mcp")
        || (crate::email_login::enabled(env) && requested == format!("{canonical}/reader/mcp"))
}
async fn authorize_page(req: &Request, env: &Env, canonical: &str) -> Res<Response> {
    let u = req.url()?;
    let Ok(f) = form_fields(u.query().unwrap_or_default().as_bytes()) else {
        return oauth_error("invalid_request", "Duplicate parameter");
    };
    let client = text(&f, "client_id");
    let callback = text(&f, "redirect_uri");
    let state = text(&f, "state");
    if !registered(env, req, client, callback).await? {
        return oauth_error("invalid_client", "Unknown client or callback");
    }
    if state.len() > 1024 {
        return oauth_error("invalid_request", "State exceeds the size limit");
    }
    let reject = |code, detail| authorization_error(callback, canonical, state, code, detail);
    let resource = text(&f, "resource");
    if text(&f, "response_type") != "code" {
        return reject("unsupported_response_type", "Request an authorization code");
    }
    if !allowed_resource(env, canonical, resource) {
        return reject("invalid_target", "Supply the canonical MCP resource");
    }
    if text(&f, "code_challenge_method") != "S256" || !valid_challenge(text(&f, "code_challenge")) {
        return reject("invalid_request", "Supply a valid S256 challenge");
    }
    let Some(scopes) = parse_scopes(f.get("scope").map(String::as_str).unwrap_or("read")) else {
        return reject("invalid_scope", "Unknown or empty scope");
    };
    if resource.ends_with("/reader/mcp")
        && (!scopes.iter().any(|s| s == "read")
            || scopes.iter().any(|s| s != "read" && s != "offline_access"))
    {
        return reject("invalid_scope", "Reader MCP requires read access");
    }
    let actor = match super::web_actor(req, env).await {
        Err(e) if e.status == 401 && crate::email_login::enabled(env) => {
            return crate::email_login::signin_redirect(&format!(
                "{}?{}",
                u.path(),
                u.query().unwrap_or_default()
            ));
        }
        result => result?,
    };
    let handle = random()?;
    let id = sha256(handle.as_bytes());
    let mut tx = Tx::begin(env, actor.clone()).await?;
    super::revalidate(&tx).await?;
    tx.put("oauth_consent",&id,json!({"id":id,"operator_id":actor.id,"client":client,"redirect_uri":callback,
        "scope":scopes,"resource":resource,"challenge":text(&f,"code_challenge"),"state":state,"expires_s":seconds()+300}))?;
    tx.commit().await?;
    let csrf = super::csrf(req, env, &actor)?;
    let page = format!(
        r#"<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>Connect to newsman</title><main><h1>Authorize a newsman client</h1><p>Signed in as <strong>{email}</strong>. Client <strong>{client}</strong> requests access to {resource}.</p><p>Requested permissions: {scopes}.</p><p>Access tokens last 15 minutes. Offline access permits renewal for up to 30 days. You can revoke the connection in newsman.</p><p>Callback: {callback}</p><form action="/oauth/authorize" method="post"><input type="hidden" name="handle" value="{handle}"><input type="hidden" name="csrf" value="{csrf}"><button name="decision" value="approve">Allow</button> <button name="decision" value="deny">Deny</button></form></main></html>"#,
        email = esc(&actor.email),
        client = esc(client),
        resource = esc(&resource),
        scopes = esc(&scopes.join(", ")),
        callback = esc(callback)
    );
    let mut res = Response::from_html(page)?;
    res.headers_mut().set("Cache-Control", "no-store")?;
    res.headers_mut().set(
        "Content-Security-Policy",
        "default-src 'none'; frame-ancestors 'none'; form-action 'self'; base-uri 'none'",
    )?;
    res.headers_mut().set("Referrer-Policy", "no-referrer")?;
    Ok(res)
}
async fn decision(
    req: &Request,
    env: &Env,
    canonical: &str,
    f: &BTreeMap<String, String>,
) -> Res<Response> {
    let actor = super::web_actor(req, env).await?;
    if req.headers().get("Origin")?.as_deref() != Some(canonical)
        || !bool::from(
            super::csrf(req, env, &actor)?
                .as_bytes()
                .ct_eq(text(f, "csrf").as_bytes()),
        )
    {
        return Err(denied());
    }
    let approve = match text(f, "decision") {
        "approve" => true,
        "deny" => false,
        _ => return oauth_error("invalid_request", "Unknown consent decision"),
    };
    let mut tx = Tx::begin(env, actor.clone()).await?;
    super::revalidate(&tx).await?;
    let cid = sha256(text(f, "handle").as_bytes());
    let Some(p) = tx.get("oauth_consent", &cid).await?.filter(|p| {
        p["operator_id"] == actor.id && p["expires_s"].as_i64().is_some_and(|v| v > seconds())
    }) else {
        return oauth_error("invalid_request", "The authorization request has expired");
    };
    if !registered(
        env,
        req,
        req_str(&p, "client")?,
        req_str(&p, "redirect_uri")?,
    )
    .await?
    {
        return oauth_error("invalid_client", "Client registration changed");
    }
    let mut callback = Url::parse(req_str(&p, "redirect_uri")?).map_err(|_| config_error())?;
    tx.delete("oauth_consent", &cid)?;
    {
        let mut q = callback.query_pairs_mut();
        q.append_pair("iss", canonical);
        if let Some(state) = p["state"].as_str().filter(|s| !s.is_empty()) {
            q.append_pair("state", state);
        }
        if approve {
            let grant = uid("grant");
            let code = random()?;
            let code_id = sha256(code.as_bytes());
            let duration = if strings(&p["scope"]).iter().any(|s| s == "offline_access") {
                GRANT_TTL
            } else {
                ACCESS_TTL + 120
            };
            let exp = seconds() + duration;
            tx.put("grant",&grant,json!({"id":grant,"operator_id":actor.id,"client":p["client"],"scopes":p["scope"],
                "resource":p["resource"],"redirect_uri":p["redirect_uri"],"active":true,"version":1,"expires_at":dated(exp),"expires_s":exp}))?;
            tx.put(
                "oauth_code",
                &code_id,
                json!({"id":code_id,"grant_id":grant,"redirect_uri":p["redirect_uri"],
                "challenge":p["challenge"],"expires_s":seconds()+120}),
            )?;
            q.append_pair("code", &code);
        } else {
            q.append_pair("error", "access_denied");
        }
    }
    tx.commit().await?;
    redirect(&callback)
}
async fn token(
    req: &Request,
    env: &Env,
    canonical: &str,
    f: &BTreeMap<String, String>,
) -> Res<Response> {
    let mode = text(f, "grant_type");
    let client = text(f, "client_id");
    let resource = text(f, "resource");
    if req.headers().get("Authorization")?.is_some()
        || !text(f, "client_secret").is_empty()
        || f.contains_key("client_assertion")
        || f.contains_key("client_assertion_type")
    {
        return oauth_error(
            "invalid_client",
            "Use the registered public client with PKCE",
        );
    }
    if client.is_empty() {
        return oauth_error("invalid_client", "Missing registered client identifier");
    }
    if !allowed_resource(env, canonical, resource) {
        return oauth_error("invalid_target", "Supply the canonical MCP resource");
    }
    let (kind, key) = match mode {
        "authorization_code" => ("oauth_code", "code"),
        "refresh_token" => ("oauth_refresh", "refresh_token"),
        _ => {
            return oauth_error(
                "unsupported_grant_type",
                "Use authorization_code or refresh_token",
            );
        }
    };
    let candidate = text(f, key);
    if candidate.len() != 43 {
        return oauth_error("invalid_grant", "Invalid or expired grant");
    }
    let id = sha256(candidate.as_bytes());
    let mut tx = Tx::begin(env, Actor::system()).await?;
    let Some(credential) = tx.get(kind, &id).await? else {
        return oauth_error("invalid_grant", "Invalid or expired grant");
    };
    let Some(mut grant) = tx
        .get("grant", req_str(&credential, "grant_id")?)
        .await?
        .filter(|g| {
            g["active"] == true
                && g["client"] == client
                && g["resource"] == resource
                && g["expires_s"].as_i64().is_some_and(|t| t > seconds())
        })
    else {
        return oauth_error("invalid_grant", "Invalid or expired grant");
    };
    let gid = req_str(&grant, "id")?.to_owned();
    if !registered(env, req, client, req_str(&grant, "redirect_uri")?).await? {
        return oauth_error("invalid_client", "The client registration has changed");
    }
    if credential["expires_s"]
        .as_i64()
        .is_none_or(|t| t <= seconds())
        || tx
            .get("operator", req_str(&grant, "operator_id")?)
            .await?
            .is_none_or(|v| !active_operator(env, &v))
    {
        return oauth_error("invalid_grant", "Invalid or expired grant");
    }
    let all_scope = strings(&grant["scopes"]);
    let base_scope = if mode == "refresh_token" {
        strings(&credential["scopes"])
    } else {
        all_scope.clone()
    };
    let scopes = if let Some(raw) = f.get("scope") {
        match parse_scopes(raw) {
            Some(s) if s.iter().all(|v| base_scope.contains(v)) => s,
            _ => return oauth_error("invalid_scope", "Scope exceeds the grant"),
        }
    } else {
        base_scope
    };
    if mode == "authorization_code" {
        let verifier = text(f, "code_verifier");
        if !pkce_verifier(verifier)
            || !bool::from(
                pkce(verifier)
                    .as_bytes()
                    .ct_eq(req_str(&credential, "challenge")?.as_bytes()),
            )
            || credential["redirect_uri"] != text(f, "redirect_uri")
        {
            return oauth_error("invalid_grant", "Invalid or expired grant");
        }
        tx.delete(kind, &id)?;
    } else {
        if credential["used"] == true {
            grant["active"] = json!(false);
            grant["version"] = json!(grant["version"].as_u64().unwrap_or_default() + 1);
            tx.put("grant", &gid, grant)?;
            tx.commit().await?;
            return oauth_error("invalid_grant", "Refresh credential was already used");
        }
        let mut used = credential;
        used["used"] = json!(true);
        tx.put(kind, &id, used)?;
    }
    let access = random()?;
    let aid = sha256(access.as_bytes());
    let exp = grant["expires_s"].as_i64().unwrap_or_default();
    let access_ttl = ACCESS_TTL.min(exp - seconds());
    if access_ttl <= 0 {
        return oauth_error("invalid_grant", "Grant expired");
    }
    tx.put("access_token",&aid,json!({"id":aid,"grant_id":gid,"client":client,"resource":resource,"scopes":scopes,"expires_s":seconds()+access_ttl}))?;
    let mut response = json!({"token_type":"Bearer","access_token":access,"expires_in":access_ttl,"scope":scopes.join(" ")});
    if scopes.iter().any(|s| s == "offline_access") {
        let refresh = random()?;
        let rid = sha256(refresh.as_bytes());
        tx.put(
            "oauth_refresh",
            &rid,
            json!({"id":rid,"grant_id":gid,"used":false,"scopes":scopes,"expires_s":exp}),
        )?;
        response["refresh_token"] = json!(refresh);
    }
    tx.commit().await?;
    as_json(200, &response)
}
async fn revoke(env: &Env, f: &BTreeMap<String, String>) -> Res<Response> {
    let value = text(f, "token");
    let client = text(f, "client_id");
    if value.is_empty() || client.is_empty() {
        return oauth_error("invalid_request", "Supply token and client_id");
    }
    let id = sha256(value.as_bytes());
    let mut tx = Tx::begin(env, Actor::system()).await?;
    let credential = match tx.get("oauth_refresh", &id).await? {
        Some(c) => Some(c),
        None => tx.get("access_token", &id).await?,
    };
    if let Some(c) = credential {
        if let Some(mut grant) = tx
            .get("grant", req_str(&c, "grant_id")?)
            .await?
            .filter(|r| r["client"] == client && r["active"] == true)
        {
            let gid = req_str(&grant, "id")?.to_owned();
            grant["active"] = json!(false);
            grant["version"] = json!(grant["version"].as_u64().unwrap_or_default() + 1);
            tx.put("grant", &gid, grant)?;
            tx.commit().await?;
        }
    }
    Ok(Response::empty()?.with_status(200))
}
pub async fn http(mut req: Request, env: &Env) -> Res<Response> {
    let canonical = origin(&req, env)?;
    let path = req.path();
    let resource = format!("{canonical}/mcp");
    if req.method() == worker::Method::Get {
        match path.as_str() {
            "/.well-known/oauth-protected-resource"
            | "/.well-known/oauth-protected-resource/mcp" => {
                return as_json(
                    200,
                    &json!({"resource":resource,"authorization_servers":[canonical],"bearer_methods_supported":["header"],"scopes_supported":["read","write","send","admin","offline_access"]}),
                );
            }
            "/.well-known/oauth-protected-resource/reader/mcp"
                if crate::email_login::enabled(env) =>
            {
                return as_json(
                    200,
                    &json!({"resource":format!("{canonical}/reader/mcp"),"authorization_servers":[canonical],"bearer_methods_supported":["header"],"scopes_supported":["read","offline_access"]}),
                );
            }
            "/.well-known/oauth-authorization-server" => {
                return as_json(
                    200,
                    &json!({"issuer":canonical,"authorization_endpoint":format!("{canonical}/oauth/authorize"),"token_endpoint":format!("{canonical}/oauth/token"),
                    "revocation_endpoint":format!("{canonical}/oauth/revoke"),"response_types_supported":["code"],"grant_types_supported":["authorization_code","refresh_token"],
                    "token_endpoint_auth_methods_supported":["none"],"code_challenge_methods_supported":["S256"],"scopes_supported":["read","write","send","admin","offline_access"],
                    "client_id_metadata_document_supported":clients::cimd_enabled(env)?,"authorization_response_iss_parameter_supported":true}),
                );
            }
            "/oauth/authorize" => return authorize_page(&req, env, &canonical).await,
            _ => {}
        }
    }
    if !matches!(
        path.as_str(),
        "/oauth/token" | "/oauth/authorize" | "/oauth/revoke"
    ) {
        return Err(AppError::new("not_found", "Unknown OAuth route", 404));
    }
    if req.method() != worker::Method::Post {
        return Err(AppError::new("method_not_allowed", "Use POST", 405));
    }
    if req
        .headers()
        .get("Content-Type")?
        .as_deref()
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        != "application/x-www-form-urlencoded"
    {
        return oauth_error("invalid_request", "Use URL-encoded form data");
    }
    let body = bounded_body(&mut req, 16384).await?;
    let Ok(f) = form_fields(&body) else {
        return oauth_error("invalid_request", "Duplicate parameter");
    };
    match path.as_str() {
        "/oauth/authorize" => decision(&req, env, &canonical, &f).await,
        "/oauth/token" => token(&req, env, &canonical, &f).await,
        "/oauth/revoke" => revoke(env, &f).await,
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pkce_matches_rfc7636_appendix_b() {
        let v = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert!(pkce_verifier(v));
        assert_eq!(pkce(v), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        assert!(!pkce_verifier("short"));
        assert!(valid_challenge(&pkce(v)));
    }
    #[test]
    fn rejects_unknown_scope_and_duplicate_field() {
        assert!(parse_scopes("read system").is_none());
        assert!(form_fields(b"scope=read&scope=admin").is_err());
        assert_eq!(parse_scopes("read read").unwrap(), vec!["read"]);
    }
}
