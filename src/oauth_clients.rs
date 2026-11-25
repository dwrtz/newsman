//! Registration for explicitly configured OAuth clients and HTTPS client ID metadata documents.
use super::{
    AppError, Env, Request, Res, Url, conf, config_error, local_request, loopback, seconds,
};
use futures_util::StreamExt;
use serde::Deserialize;
use std::{cell::RefCell, collections::BTreeMap};
use worker::{Fetch, RequestInit, RequestRedirect};

type Registrations = BTreeMap<String, Vec<String>>;
const MAX_DOCUMENT_BYTES: usize = 32 * 1024;

#[derive(Deserialize)]
struct Document {
    client_id: String,
    redirect_uris: Vec<String>,
    token_endpoint_auth_method: Option<String>,
    token_endpoint_auth_methods_supported: Option<Vec<String>>,
}

impl Document {
    fn allows(&self, id: &str, callback: &str) -> bool {
        let public = match &self.token_endpoint_auth_methods_supported {
            Some(methods) => methods.iter().any(|m| m == "none"),
            None => self.token_endpoint_auth_method.as_deref() == Some("none"),
        };
        public && self.client_id == id && self.redirect_uris.iter().any(|u| u == callback)
    }
}

thread_local! {
    static DOCUMENTS: RefCell<BTreeMap<String, (i64, Document)>> = const { RefCell::new(BTreeMap::new()) };
}

fn configured(env: &Env, key: &str) -> Res<Registrations> {
    match conf(env, key) {
        Some(raw) => serde_json::from_str(&raw).map_err(|_| config_error()),
        None => Ok(BTreeMap::new()),
    }
}

pub(super) fn cimd_enabled(env: &Env) -> Res<bool> {
    Ok(!configured(env, "OAUTH_CIMD_CLIENTS")?.is_empty())
}

fn cache_seconds(header: &str) -> i64 {
    let directives: Vec<_> = header.split(',').map(str::trim).collect();
    if directives.iter().any(|d| {
        matches!(
            d.to_ascii_lowercase().as_str(),
            "no-store" | "no-cache" | "private"
        )
    }) {
        return 0;
    }
    directives
        .iter()
        .filter_map(|d| d.split_once('='))
        .find(|(k, _)| k.eq_ignore_ascii_case("max-age"))
        .and_then(|(_, v)| v.trim_matches('"').parse::<i64>().ok())
        .unwrap_or(0)
        .clamp(0, 300)
}

fn unavailable() -> AppError {
    AppError::new(
        "client_metadata_unavailable",
        "Could not verify the registered OAuth client metadata.",
        503,
    )
}

async fn document_allows(id: &str, callback: &str) -> Res<bool> {
    let at = seconds();
    if let Some(allowed) = DOCUMENTS.with(|docs| {
        docs.borrow()
            .get(id)
            .filter(|(until, _)| *until > at)
            .map(|(_, doc)| doc.allows(id, callback))
    }) {
        return Ok(allowed);
    }
    let url = Url::parse(id).map_err(|_| config_error())?;
    if url.scheme() != "https"
        || url.path() == "/"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
        || !matches!(url.host(), Some(url::Host::Domain(_)))
        || loopback(&url)
    {
        return Err(config_error());
    }
    let mut init = RequestInit::new();
    init.with_redirect(RequestRedirect::Manual);
    let mut req = Request::new_with_init(id, &init)?;
    req.headers_mut()?.set("Accept", "application/json")?;
    let mut res = Fetch::Request(req)
        .send()
        .await
        .map_err(|_| unavailable())?;
    let content_type = res.headers().get("Content-Type")?.unwrap_or_default();
    if res.status_code() != 200
        || !content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("application/json")
    {
        return Err(unavailable());
    }
    let ttl = cache_seconds(&res.headers().get("Cache-Control")?.unwrap_or_default());
    let mut bytes = Vec::new();
    let mut chunks = res.stream().map_err(|_| unavailable())?;
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| unavailable())?;
        if chunk.len() > MAX_DOCUMENT_BYTES - bytes.len() {
            return Err(unavailable());
        }
        bytes.extend_from_slice(&chunk);
    }
    let doc: Document = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
    let allowed = doc.allows(id, callback);
    if ttl > 0 && doc.client_id == id {
        DOCUMENTS.with(|docs| {
            docs.borrow_mut().insert(id.to_owned(), (at + ttl, doc));
        });
    }
    Ok(allowed)
}

pub(super) async fn registered(
    env: &Env,
    req: &Request,
    client: &str,
    callback: &str,
) -> Res<bool> {
    if client.is_empty() || client.len() > 512 || client.bytes().any(|b| b.is_ascii_control()) {
        return Ok(false);
    }
    let Ok(url) = Url::parse(callback) else {
        return Ok(false);
    };
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || (url.scheme() == "http" && loopback(&url)))
    {
        return Ok(false);
    }
    // Only deployment-owned configuration can select a URL for an outbound request.
    if let Some(callbacks) = configured(env, "OAUTH_CIMD_CLIENTS")?.get(client) {
        if !callbacks.iter().any(|c| c == callback) {
            return Ok(false);
        }
        return document_allows(client, callback).await;
    }
    if conf(env, "OAUTH_CLIENTS").is_some() {
        return Ok(configured(env, "OAUTH_CLIENTS")?
            .get(client)
            .is_some_and(|uris| uris.iter().any(|c| c == callback)));
    }
    Ok(local_request(env, &req.url()?)
        && client == "newsman-local"
        && callback == "http://127.0.0.1:9876/callback")
}
