//! Stateless MCP Streamable HTTP with version negotiation for hosted and local clients.
use crate::core::{Actor, AppError, Res};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use worker::{Env, Request, Response};

pub const VERSION: &str = "2025-11-25";
const MARCH: &str = "2025-03-26";
const VERSIONS: [&str; 3] = [VERSION, "2025-06-18", MARCH];

struct Endpoint<'a> {
    surface: &'a str,
    scopes: &'a [String],
    protected: bool,
    resource_metadata: &'a str,
}

#[derive(Debug)]
pub struct McpResponse {
    pub status: u16,
    pub json: Option<Value>,
    pub headers: BTreeMap<String, String>,
}

impl McpResponse {
    fn new(status: u16, body: Option<Value>) -> Self {
        Self {
            status,
            json: body,
            headers: BTreeMap::new(),
        }
    }
    fn protocol_error(status: u16, id: Value, code: i32, message: &str) -> Self {
        Self::new(
            status,
            Some(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})),
        )
    }
    fn result(id: Value, result: Value) -> Self {
        Self::new(200, Some(json!({"jsonrpc":"2.0","id":id,"result":result})))
    }
}

pub struct Call {
    pub id: Value,
    pub name: String,
    pub arguments: Value,
}

pub enum Incoming {
    Reply(McpResponse),
    Invoke(Call),
}

fn operation_list() -> &'static [Value] {
    crate::registry()["operations"]
        .as_array()
        .expect("operations")
}

fn tools(endpoint: &Endpoint, version: &str) -> Vec<Value> {
    static TOOLS: OnceLock<Vec<Value>> = OnceLock::new();
    let descriptors = TOOLS.get_or_init(|| operation_list().iter().map(|op| {
        json!({"name":op["name"],"description":op["description"],
              "inputSchema":crate::tool_schema(op,"inputSchema"),"outputSchema":crate::tool_schema(op,"outputSchema"),
              "annotations":{"readOnlyHint":op["mode"]=="query"}})
    }).collect());
    operation_list()
        .iter()
        .zip(descriptors)
        .filter(|(op, _)| {
            op["surface"] == endpoint.surface
                && (endpoint.surface == "reader"
                    || endpoint.scopes.iter().any(|s| op["scope"] == s.as_str()))
        })
        .map(|(op, descriptor)| {
            let mut tool = descriptor.clone();
            let schemes = if endpoint.protected {
                let scope = if endpoint.surface == "reader" {
                    "read"
                } else {
                    op["scope"].as_str().expect("operation scope")
                };
                json!([{"type":"oauth2","scopes":[scope]}])
            } else {
                json!([{"type":"noauth"}])
            };
            tool["securitySchemes"] = schemes.clone();
            tool["_meta"] = json!({"securitySchemes":schemes});
            if version == MARCH {
                tool.as_object_mut()
                    .expect("tool descriptor")
                    .remove("outputSchema");
            }
            tool
        })
        .collect()
}

fn accepts(header: &str, mime: &str) -> bool {
    header
        .split(',')
        .map(|v| v.trim().split(';').next().unwrap_or_default().trim())
        .any(|s| s.eq_ignore_ascii_case(mime))
}

/// Validate the transport before dispatching any message in the HTTP request.
fn decode(
    method: &str,
    content_type: &str,
    accept: &str,
    version: &str,
    body: &[u8],
) -> Result<Value, McpResponse> {
    if method != "POST" {
        let mut response = McpResponse::new(405, None);
        response.headers.insert("Allow".into(), "POST".into());
        return Err(response);
    }
    if !content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .eq_ignore_ascii_case("application/json")
    {
        return Err(McpResponse::protocol_error(
            415,
            Value::Null,
            -32600,
            "Use application/json",
        ));
    }
    if !accepts(accept, "application/json") || !accepts(accept, "text/event-stream") {
        return Err(McpResponse::protocol_error(
            406,
            Value::Null,
            -32600,
            "Accept application/json and text/event-stream",
        ));
    }
    if !VERSIONS.contains(&version) {
        return Err(McpResponse::protocol_error(
            400,
            Value::Null,
            -32600,
            "Unsupported MCP-Protocol-Version",
        ));
    }
    let message: Value = serde_json::from_slice(body)
        .map_err(|_| McpResponse::protocol_error(400, Value::Null, -32700, "Invalid JSON"))?;
    if let Some(batch) = message.as_array() {
        if version != MARCH || batch.is_empty() || batch.iter().any(|m| m["method"] == "initialize")
        {
            return Err(McpResponse::protocol_error(
                400,
                Value::Null,
                -32600,
                "Invalid batch for this protocol revision",
            ));
        }
        if batch.len() > 50 {
            return Err(McpResponse::protocol_error(
                413,
                Value::Null,
                -32600,
                "Batch exceeds 50 messages",
            ));
        }
    }
    Ok(message)
}

fn receive(message: &Value, version: &str, endpoint: &Endpoint) -> Incoming {
    let reply = Incoming::Reply;
    let id = message.get("id").cloned();
    if message["jsonrpc"] != "2.0"
        || !message["method"].is_string()
        || id
            .as_ref()
            .is_some_and(|v| !(v.is_string() || v.is_i64() || v.is_u64()))
    {
        return reply(McpResponse::protocol_error(
            400,
            Value::Null,
            -32600,
            "Invalid JSON-RPC request",
        ));
    }
    let rpc = message["method"].as_str().unwrap_or_default();
    let params = &message["params"];
    if id.is_none() {
        if !rpc.starts_with("notifications/") {
            return reply(McpResponse::protocol_error(
                400,
                Value::Null,
                -32600,
                "A request ID is required",
            ));
        }
        return reply(McpResponse::new(202, None));
    }
    let id = id.unwrap_or_default();
    match rpc {
        "initialize" => {
            if !params["protocolVersion"].is_string()
                || !params["capabilities"].is_object()
                || !params["clientInfo"]["name"].is_string()
                || !params["clientInfo"]["version"].is_string()
            {
                return reply(McpResponse::protocol_error(
                    200,
                    id,
                    -32602,
                    "Invalid initialization parameters",
                ));
            }
            reply(McpResponse::result(
                id,
                json!({"protocolVersion":params["protocolVersion"].as_str().filter(|v| VERSIONS.contains(v)).unwrap_or(VERSION),
                "capabilities":{"tools":{}},"serverInfo":{"name":"newsman","version":env!("CARGO_PKG_VERSION")}}),
            ))
        }
        "ping" => reply(McpResponse::result(id, json!({}))),
        "tools/list" => {
            if !params["cursor"].is_null() {
                return reply(McpResponse::protocol_error(
                    200,
                    id,
                    -32602,
                    "Invalid tools cursor",
                ));
            }
            reply(McpResponse::result(
                id,
                json!({"tools":tools(endpoint,version)}),
            ))
        }
        "tools/call" => {
            let Some(op) = operation_list()
                .iter()
                .find(|op| op["surface"] == endpoint.surface && op["name"] == params["name"])
            else {
                return reply(McpResponse::protocol_error(200, id, -32602, "Unknown tool"));
            };
            if endpoint.surface == "operator"
                && !endpoint.scopes.iter().any(|s| op["scope"] == s.as_str())
            {
                let challenge = format!(
                    r#"Bearer error="insufficient_scope", error_description="This tool requires an additional scope", scope="{}", resource_metadata="{}""#,
                    op["scope"].as_str().expect("operation scope"),
                    endpoint.resource_metadata
                );
                let mut response = McpResponse::result(
                    id,
                    json!({
                        "isError":true, "content":[{"type":"text","text":"Insufficient scope"}],
                        "_meta":{"mcp/www_authenticate":[challenge]}
                    }),
                );
                response.status = 403;
                response
                    .headers
                    .insert("WWW-Authenticate".into(), challenge);
                return reply(response);
            }
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if !arguments.is_object() {
                return reply(McpResponse::protocol_error(
                    200,
                    id,
                    -32602,
                    "Tool arguments must be an object",
                ));
            }
            Incoming::Invoke(Call {
                id,
                name: op["name"].as_str().unwrap_or_default().into(),
                arguments,
            })
        }
        _ => reply(McpResponse::protocol_error(
            200,
            id,
            -32601,
            "Method not found",
        )),
    }
}

fn completed(id: Value, result: Result<Value, Value>, version: &str) -> McpResponse {
    match result {
        Ok(value) if version == MARCH => McpResponse::result(
            id,
            json!({"content":[{"type":"text","text":value.to_string()}]}),
        ),
        Ok(value) => McpResponse::result(
            id,
            json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value}),
        ),
        Err(error) => McpResponse::result(
            id,
            json!({"isError":true,"content":[{"type":"text","text":error.to_string()}]}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn endpoint<'a>(surface: &'a str, scopes: &'a [String]) -> Endpoint<'a> {
        Endpoint {
            surface,
            scopes,
            protected: surface == "operator",
            resource_metadata: "https://news.example/metadata",
        }
    }
    #[test]
    fn both_surfaces_have_complete_independent_tool_sets() {
        let scope: Vec<String> = ["read", "write", "send", "admin"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(tools(&endpoint("operator", &scope), VERSION).len(), 59);
        assert_eq!(tools(&endpoint("reader", &[]), VERSION).len(), 5);
        assert!(
            tools(&endpoint("operator", &["send".into()]), VERSION)
                .iter()
                .all(|t| {
                    t["name"]
                        .as_str()
                        .is_some_and(|n| n.starts_with("send_") || n == "attempt_resolve")
                })
        );
        for d in tools(&endpoint("operator", &scope), VERSION) {
            for schema in [&d["inputSchema"], &d["outputSchema"]] {
                assert_eq!(schema["type"], "object");
            }
        }
    }
    #[test]
    fn notification_ack_and_reader_privilege() {
        let call = |b: Value| receive(&b, VERSION, &endpoint("reader", &[]));
        let Incoming::Reply(r) =
            call(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        else {
            panic!()
        };
        assert_eq!(r.status, 202);
        assert!(r.json.is_none());
        let Incoming::Reply(r) = call(
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"operators_list"}}),
        ) else {
            panic!()
        };
        assert_eq!(r.json.unwrap()["error"]["code"], -32602);
    }
}

async fn execute_message(
    incoming: Incoming,
    env: &Env,
    actor: &Actor,
    reader: bool,
    hint: &str,
    version: &str,
) -> McpResponse {
    match incoming {
        Incoming::Reply(reply) => reply,
        Incoming::Invoke(call) => {
            let result = if reader {
                crate::invoke(env, actor.clone(), &call.name, call.arguments, Some(hint)).await
            } else {
                crate::execute(env, actor.clone(), &call.name, call.arguments).await
            };
            completed(call.id, result.map_err(|e| json!({
                "code":e.code,"message":e.message,"retryable":e.status >= 500 || e.status == 429
            })), version)
        }
    }
}

fn as_http(reply: McpResponse) -> Res<Response> {
    let mut response = match reply.json {
        Some(body) => Response::from_json(&body)?,
        None => Response::empty()?,
    }
    .with_status(reply.status);
    response.headers_mut().set("Cache-Control", "no-store")?;
    for (key, value) in reply.headers {
        response.headers_mut().set(&key, &value)?;
    }
    Ok(response)
}

pub async fn http(mut req: Request, env: &Env, reader: bool) -> Res<Response> {
    let origin = crate::access::origin(&req, env)?;
    if let Some(value) = req.headers().get("Origin")? {
        if value != origin {
            return Err(AppError::new("permission_denied", "Unexpected Origin", 403));
        }
    }
    let meta = format!(
        "{origin}/.well-known/oauth-protected-resource{}",
        req.path()
    );
    let actor = if reader && !crate::email_login::enabled(env) {
        Actor::reader()
    } else {
        match crate::access::bearer(&req, env).await {
            Ok(actor) if reader && !actor.has("read") => {
                return Err(AppError::new(
                    "permission_denied",
                    "Reader MCP requires read access",
                    403,
                ));
            }
            Ok(actor) => actor,
            Err(e) if e.status == 401 || e.status == 403 => {
                let mut reply = McpResponse::protocol_error(
                    401,
                    Value::Null,
                    -32001,
                    "Authentication required",
                );
                let challenge = if req.headers().get("Authorization")?.is_some() {
                    format!(
                        r#"Bearer error="invalid_token", error_description="The bearer token is invalid for this resource", resource_metadata="{meta}""#
                    )
                } else {
                    format!(r#"Bearer resource_metadata="{meta}""#)
                };
                reply.headers.insert("WWW-Authenticate".into(), challenge);
                return as_http(reply);
            }
            Err(e) => return Err(e),
        }
    };
    let method = req.method().to_string();
    let version = req
        .headers()
        .get("MCP-Protocol-Version")?
        .unwrap_or_else(|| MARCH.to_owned());
    let content_type = req.headers().get("Content-Type")?.unwrap_or_default();
    let accept = req.headers().get("Accept")?.unwrap_or_default();
    let hint = req.headers().get("CF-Connecting-IP")?.unwrap_or_default();
    let bytes = if method == "POST" {
        crate::bounded_body(&mut req, 2_000_000).await?
    } else {
        Vec::new()
    };
    let message = match decode(&method, &content_type, &accept, &version, &bytes) {
        Ok(message) => message,
        Err(error) => return as_http(error),
    };
    let endpoint = Endpoint {
        surface: if reader { "reader" } else { "operator" },
        scopes: &actor.scopes,
        protected: !reader || crate::email_login::enabled(env),
        resource_metadata: &meta,
    };
    let dispatch = |message: &Value| {
        execute_message(
            receive(message, &version, &endpoint),
            env,
            &actor,
            reader,
            &hint,
            &version,
        )
    };
    if let Value::Array(batch) = message {
        let mut replies = Vec::new();
        for item in batch {
            if let Some(reply) = dispatch(&item).await.json {
                replies.push(reply);
            }
        }
        let response = if replies.is_empty() {
            McpResponse::new(202, None)
        } else {
            McpResponse::new(200, Some(Value::Array(replies)))
        };
        return as_http(response);
    }
    as_http(dispatch(&message).await)
}
