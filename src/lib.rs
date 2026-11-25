mod access;
mod audience;
pub mod core;
mod delivery;
mod editorial;
mod email_login;
mod identity_ops;
mod mcp;
mod provider;
mod reporting;
mod schema;
mod transfers;
mod workload;

use core::{Actor, AppError, Res, Tx};
use futures_util::StreamExt;
pub use schema::{registry, tool_schema};
use serde_json::{Value, json};
use worker::{
    Context, Env, MessageBatch, Method, Request, Response, ScheduleContext, ScheduledEvent, event,
};

pub async fn bounded_body(req: &mut Request, limit: usize) -> Res<Vec<u8>> {
    if req
        .headers()
        .get("Content-Length")?
        .and_then(|n| n.parse::<usize>().ok())
        .is_some_and(|n| n > limit)
    {
        return Err(AppError::new(
            "too_large",
            "Request exceeds the size limit.",
            413,
        ));
    }
    if req.inner().body().is_none() {
        return Ok(Vec::new());
    }
    let mut stream = req.stream()?;
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(AppError::new(
                "too_large",
                "Request exceeds the size limit.",
                413,
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub async fn execute(env: &Env, actor: Actor, name: &str, a: Value) -> Res<Value> {
    invoke(env, actor, name, a, None).await
}
pub async fn execute_reader(env: &Env, name: &str, a: Value, hint: &str) -> Res<Value> {
    invoke(env, Actor::reader(), name, a, Some(hint)).await
}

async fn invoke(
    env: &Env,
    actor: Actor,
    name: &str,
    args: Value,
    reader_hint: Option<&str>,
) -> Res<Value> {
    let op = schema::operation(name)
        .ok_or_else(|| AppError::new("not_found", "Unknown operation.", 404))?;
    if !(actor.has(op["scope"].as_str().unwrap_or("unavailable"))
        || (op["scope"] == "reader" && actor.grant_id.is_some() && actor.has("read")))
    {
        return Err(AppError::new(
            "permission_denied",
            "This grant cannot run that operation.",
            403,
        ));
    }
    schema::validate(&op["inputSchema"], &args, "arguments")?;
    let command = op["mode"] == "command";
    let is_operator = op["surface"] == "operator";
    let digest = core::sha256(args.to_string().as_bytes());
    let receipt_key = args["key"]
        .as_str()
        .map(|key| core::sha256(format!("{}\0{name}\0{key}", actor.id).as_bytes()));
    for attempt in 0..4 {
        let mut tx = Tx::begin(env, actor.clone()).await?;
        if is_operator || actor.grant_id.is_some() {
            access::revalidate(&tx).await?;
        }
        if let Some(h) = reader_hint
            && matches!(name, "reader_subscribe" | "reader_resend")
        {
            audience::admit(&mut tx, h).await?;
        }
        if let Some(ref key) = receipt_key
            && let Some(prev) = tx.get("receipt", key).await?
        {
            transfers::authorize_receipt(&tx, name, &args).await?;
            if prev["digest"] != digest {
                return Err(AppError::new(
                    "idempotency_conflict",
                    "The key was already used with different arguments.",
                    409,
                ));
            }
            return Ok(prev["result"].clone());
        }
        let result = match op["view"].as_str().unwrap_or("") {
            "Overview" | "Reports" => reporting::run(&mut tx, name, &args).await?,
            "Issues" | "Templates" | "Archive" | "Settings" => {
                editorial::run(&mut tx, name, &args).await?
            }
            "Operators" | "Connections" => identity_ops::run(&mut tx, name, &args).await?,
            "Campaigns" | "Operations" => delivery::run(&mut tx, name, &args).await?,
            "Subscribe" | "Confirm" | "Unsubscribe" | "Audience" | "Segments" => {
                audience::run(&mut tx, name, &args).await?
            }
            "Jobs" | "Transfers" | "Migration" => transfers::run(&mut tx, name, &args).await?,
            "Audit" => {
                let rows = tx
                    .list("event")
                    .await?
                    .into_iter()
                    .filter(|r| {
                        args.get("object_id").is_none_or(|id| &r["object_id"] == id)
                            && args["since"]
                                .as_str()
                                .is_none_or(|ts| r["at"].as_str().unwrap_or("") >= ts)
                    })
                    .map(|r| {
                        let mut event = json!({});
                        for k in [
                            "id",
                            "at",
                            "kind",
                            "actor_id",
                            "object_id",
                            "source",
                            "reason",
                        ] {
                            if let Some(v) = r.get(k) {
                                event[k] = v.clone()
                            }
                        }
                        event
                    })
                    .collect();
                core::page_values(rows, &args)?
            }
            _ => return Err(AppError::new("not_found", "Unregistered operation.", 404)),
        };
        if command && is_operator {
            let id = core::uid("evt");
            let mut event = json!({"id":id,"at":core::now(),"kind":name,"actor_id":actor.id,"source":if actor.grant_id.is_some(){"mcp"}else{"web"}});
            if let Some(id) = result["id"].as_str().or_else(|| args["id"].as_str()) {
                event["object_id"] = json!(id)
            }
            tx.put("event", &id, event)?;
            if let Some(ref key) = receipt_key {
                tx.put(
                    "receipt",
                    key,
                    json!({"id":key,"digest":digest,"result":result,"at":core::now()}),
                )?;
            }
        }
        match tx.commit().await {
            Err(e) if e.code == "concurrent_update" && attempt < 3 => continue,
            other => {
                other?;
                return Ok(result);
            }
        }
    }
    Err(AppError::new(
        "concurrent_update",
        "Retry with the original key.",
        409,
    ))
}

fn error_response(err: AppError) -> worker::Result<Response> {
    Ok(Response::from_json(&json!({"error":{"code":err.code,"message":err.message,"retryable":err.status>=500||err.status==429}}))?.with_status(err.status))
}

fn form_with_origin(response: Response) -> Res<Response> {
    let headers = response.headers().clone();
    // Native form submissions need an Origin for CSRF checks. Only disclose the origin.
    headers.set("Referrer-Policy", "strict-origin")?;
    Ok(response.with_headers(headers))
}

async fn route(mut req: Request, env: &Env) -> Res<Response> {
    let url = req.url()?;
    let path = url.path();
    let method = req.method();
    if path.starts_with("/workload-identity/") {
        return workload::discovery(&req, env);
    }
    if email_login::enabled(env) {
        if path == "/login" || path.starts_with("/login/") {
            return email_login::http(req, env).await;
        }
        if matches!(path, "/" | "/index.html") {
            match access::web_actor(&req, env).await {
                Err(e) if e.status == 401 => return email_login::signin_redirect("/"),
                result => {
                    result?;
                }
            }
        }
    }
    if path == "/mcp" || path == "/reader/mcp" {
        return mcp::http(req, env, path == "/reader/mcp").await;
    }
    if path.starts_with("/oauth/") || path.starts_with("/.well-known/") {
        return access::http(req, env).await;
    }
    if path == "/webhooks/ses" {
        if method != Method::Post {
            return Ok(Response::error("Method not allowed", 405)?);
        }
        return provider::feedback(req, env).await;
    }
    if path.starts_with("/r/") {
        return delivery::redirect(req, env).await;
    }
    if let Some(params) = path.strip_prefix("/media/")
        && let Some((id, token)) = params.split_once('/')
    {
        return editorial::media(env, id, token).await;
    }
    if path.starts_with("/api/newsletter/")
        || matches!(path.trim_end_matches('/'), "/subscribe" | "/confirmed")
        || path.starts_with("/confirm/")
        || path.starts_with("/unsubscribe/")
    {
        return audience::http(req, env).await;
    }
    if path.starts_with("/api/") {
        let actor = access::web_actor(&req, env).await?;
        if path == "/api/session" && method == Method::Get {
            return Ok(Response::from_json(
                &json!({"email":actor.email,"scopes":actor.scopes,"csrf":access::csrf(&req,env,&actor)?,"local":core::local(env),"email_login":email_login::enabled(env)}),
            )?);
        }
        if path == "/api/registry" && method == Method::Get {
            return Ok(Response::from_json(registry())?);
        }
        if let Some(name) = path.strip_prefix("/api/ops/") {
            if method != Method::Post {
                return Ok(Response::error("Use POST", 405)?);
            }
            if !req
                .headers()
                .get("Content-Type")?
                .unwrap_or_default()
                .split(';')
                .next()
                .is_some_and(|s| s.trim() == "application/json")
            {
                return Err(AppError::new(
                    "unsupported_media_type",
                    "Expected JSON.",
                    415,
                ));
            }
            let op = schema::operation(name)
                .ok_or_else(|| AppError::new("not_found", "Unknown operation.", 404))?;
            if op["surface"] != "operator" {
                return Err(AppError::new("not_found", "Use the reader endpoint.", 404));
            }
            if op["mode"] == "command" {
                use subtle::ConstantTimeEq;
                let given = req.headers().get("X-CSRF-Token")?.unwrap_or_default();
                if req.headers().get("Origin")?.as_deref() != Some(&access::origin(&req, env)?)
                    || !bool::from(
                        given
                            .as_bytes()
                            .ct_eq(access::csrf(&req, env, &actor)?.as_bytes()),
                    )
                {
                    return Err(AppError::new(
                        "permission_denied",
                        "Cross-origin request or expired form token.",
                        403,
                    ));
                }
            }
            let args: Value = serde_json::from_slice(&bounded_body(&mut req, 1_500_000).await?)?;
            return Ok(Response::from_json(
                &execute(env, actor, name, args).await?,
            )?);
        }
        return Ok(Response::error("Not found", 404)?);
    }
    if method != Method::Get && method != Method::Head {
        return Ok(Response::error("Method not allowed", 405)?);
    }
    // Static code contains no account data. The API independently authorizes every read and write.
    let response = env.assets("ASSETS")?.fetch_request(req).await?;
    if email_login::enabled(env) && matches!(path, "/" | "/index.html") {
        return form_with_origin(response);
    }
    Ok(response)
}

#[event(fetch)]
pub async fn fetch(req: Request, env: Env, _: Context) -> worker::Result<Response> {
    let mut response = match route(req, &env).await {
        Ok(r) => r,
        Err(e) => error_response(e)?,
    };
    let headers = response.headers().clone();
    response = response.with_headers(headers);
    response
        .headers_mut()
        .set("X-Content-Type-Options", "nosniff")?;
    if response.headers().get("Referrer-Policy")?.is_none() {
        response
            .headers_mut()
            .set("Referrer-Policy", "no-referrer")?;
    }
    if response.headers().get("Cache-Control")?.is_none() {
        response.headers_mut().set("Cache-Control", "no-store")?;
    }
    Ok(response)
}
#[event(scheduled)]
pub async fn scheduled(_: ScheduledEvent, env: Env, _: ScheduleContext) {
    if let Err(e) = maintenance(&env).await {
        worker::console_error!("newsman scheduler: {}", e.code);
    }
}
#[event(queue)]
pub async fn queue(batch: MessageBatch<Value>, env: Env, _: Context) -> worker::Result<()> {
    match maintenance(&env).await {
        Ok(()) => batch.ack_all(),
        Err(_) => batch.retry_all(),
    };
    Ok(())
}

async fn maintenance(env: &Env) -> Res<()> {
    access::cleanup(env).await?;
    transfers::tick(env).await?;
    delivery::tick(env).await
}
