use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use wasm_bindgen::JsValue;
use worker::{D1Database, Env};

#[derive(Debug, Clone, Serialize)]
pub struct AppError {
    pub code: String,
    pub message: String,
    #[serde(skip)]
    pub status: u16,
}

pub type Res<T> = Result<T, AppError>;

impl AppError {
    pub fn new(code: &str, message: impl Into<String>, status: u16) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
            status,
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_input", message, 422)
    }
}
impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for AppError {}
impl From<worker::Error> for AppError {
    fn from(_: worker::Error) -> Self {
        Self::new(
            "service_unavailable",
            "A required service could not complete the request.",
            503,
        )
    }
}
impl From<serde_json::Error> for AppError {
    fn from(_: serde_json::Error) -> Self {
        Self::invalid("Invalid JSON.")
    }
}

#[derive(Clone, Debug)]
pub struct Actor {
    pub id: String,
    pub email: String,
    pub scopes: Vec<String>,
    pub grant_id: Option<String>,
}
impl Actor {
    pub fn has(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
    pub fn reader() -> Self {
        Self {
            id: "reader".into(),
            email: String::new(),
            scopes: vec!["reader".into()],
            grant_id: None,
        }
    }
    pub fn system() -> Self {
        Self {
            id: "system".into(),
            email: String::new(),
            scopes: ["read", "write", "send", "admin"]
                .map(String::from)
                .to_vec(),
            grant_id: None,
        }
    }
}

pub fn uid(prefix: &str) -> String {
    format!("{}_{}", prefix, uuid::Uuid::new_v4().simple())
}
pub fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
pub fn conf(env: &Env, key: &str) -> Option<String> {
    env.var(key)
        .ok()
        .map(|v| v.to_string())
        .or_else(|| env.secret(key).ok().map(|v| v.to_string()))
        .filter(|s| !s.trim().is_empty())
}
pub fn local(env: &Env) -> bool {
    conf(env, "NEWSMAN_LOCAL").as_deref() == Some("yes")
        && ["ACCESS_AUD", "ACCESS_TEAM_DOMAIN", "AWS_ACCESS_KEY_ID"]
            .iter()
            .all(|k| conf(env, k).is_none())
}
pub fn req_str<'a>(v: &'a Value, k: &str) -> Res<&'a str> {
    v.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::invalid(format!("Missing {k}.")))
}
pub fn expect_version(doc: &Value, args: &Value) -> Res<()> {
    let expected = args.get("expected_version").and_then(Value::as_u64);
    if expected.is_none() || doc.get("version").and_then(Value::as_u64) != expected {
        return Err(AppError::new(
            "revision_conflict",
            "This record changed. Reload it and retry.",
            409,
        ));
    }
    Ok(())
}
pub fn page_values(mut items: Vec<Value>, args: &Value) -> Res<Value> {
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 100) as usize;
    let key = |v: &Value| {
        v.get("id")
            .or_else(|| v.get("label"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    items.sort_by_key(key);
    if let Some(cursor) = args.get("cursor").and_then(Value::as_str) {
        let after = String::from_utf8(
            URL_SAFE_NO_PAD
                .decode(cursor)
                .map_err(|_| AppError::invalid("Invalid cursor."))?,
        )
        .map_err(|_| AppError::invalid("Invalid cursor."))?;
        items.retain(|v| key(v) > after);
    }
    let has_more = items.len() > limit;
    items.truncate(limit);
    let mut result = json!({"items":items});
    if has_more {
        result["next_cursor"] = json!(URL_SAFE_NO_PAD.encode(key(items.last().unwrap())));
    }
    Ok(result)
}

#[derive(Deserialize)]
struct Record {
    id: String,
    body: String,
}

/// A read-your-writes unit of work. The D1 batch either commits the comparison
/// and every document (including receipts), or rolls the entire operation back.
pub struct Tx<'a> {
    pub env: &'a Env,
    pub actor: Actor,
    db: D1Database,
    pub revision: i64,
    writes: BTreeMap<(String, String), Option<Value>>,
}
impl<'a> Tx<'a> {
    pub async fn begin(env: &'a Env, actor: Actor) -> Res<Self> {
        let db = env.d1("DB")?;
        let revision = db
            .prepare("SELECT revision FROM newsman_state WHERE id=1")
            .first::<i64>(Some("revision"))
            .await?
            .ok_or_else(|| {
                AppError::new("setup_required", "Apply the database migrations.", 503)
            })?;
        Ok(Self {
            env,
            actor,
            db,
            revision,
            writes: BTreeMap::new(),
        })
    }
    pub async fn get(&self, kind: &str, id: &str) -> Res<Option<Value>> {
        if let Some(v) = self.writes.get(&(kind.into(), id.into())) {
            return Ok(v.clone());
        }
        let s = self
            .db
            .prepare("SELECT body FROM newsman_records WHERE kind=? AND id=?")
            .bind(&[kind.into(), id.into()])?
            .first::<String>(Some("body"))
            .await?;
        Ok(s.map(|x| serde_json::from_str(&x)).transpose()?)
    }
    pub async fn require(&self, kind: &str, id: &str) -> Res<Value> {
        self.get(kind, id)
            .await?
            .ok_or_else(|| AppError::new("not_found", format!("No matching {kind} record."), 404))
    }
    pub async fn page(&self, kind: &str, after: &str, limit: usize) -> Res<Vec<Value>> {
        if limit == 0 {
            return Ok(vec![]);
        }
        let pending = self
            .writes
            .iter()
            .filter(|((k, i), _)| k == kind && i.as_str() > after);
        let cap = limit.saturating_add(pending.clone().filter(|(_, v)| v.is_none()).count());
        let rows = self
            .db
            .prepare(
                "SELECT id,body FROM newsman_records WHERE kind=? AND id>? ORDER BY id LIMIT ?",
            )
            .bind(&[
                kind.into(),
                after.into(),
                JsValue::from_f64(cap.min(i32::MAX as usize) as f64),
            ])?
            .all()
            .await?
            .results::<Record>()?;
        let mut map = BTreeMap::new();
        for row in rows {
            map.insert(row.id, serde_json::from_str(&row.body)?);
        }
        for ((_, id), v) in pending {
            match v {
                Some(x) => {
                    map.insert(id.clone(), x.clone());
                }
                None => {
                    map.remove(id);
                }
            }
        }
        Ok(map.into_values().take(limit).collect())
    }
    pub async fn list(&self, kind: &str) -> Res<Vec<Value>> {
        // Keep D1 result pages bounded even for installations with long send histories.
        let mut after = String::new();
        let mut map = BTreeMap::new();
        loop {
            let rows = self.db.prepare(
                "SELECT id,body FROM newsman_records WHERE kind=? AND id>? ORDER BY id LIMIT 100",
            ).bind(&[kind.into(), after.as_str().into()])?.all().await?.results::<Record>()?;
            let done = rows.len() < 100;
            for row in rows {
                after = row.id.clone();
                map.insert(row.id, serde_json::from_str(&row.body)?);
            }
            if done {
                break;
            }
        }
        for ((k, id), v) in &self.writes {
            if k == kind {
                match v {
                    Some(x) => {
                        map.insert(id.clone(), x.clone());
                    }
                    None => {
                        map.remove(id);
                    }
                }
            }
        }
        Ok(map.into_values().collect())
    }
    pub fn put(&mut self, kind: &str, id: &str, body: Value) -> Res<()> {
        if kind.is_empty() || id.is_empty() {
            return Err(AppError::invalid("A stored record requires an ID."));
        }
        self.writes.insert((kind.into(), id.into()), Some(body));
        Ok(())
    }
    pub fn delete(&mut self, kind: &str, id: &str) -> Res<()> {
        self.writes.insert((kind.into(), id.into()), None);
        Ok(())
    }
    pub async fn commit(self) -> Res<()> {
        if self.writes.is_empty() {
            return Ok(());
        }
        let mut batch=vec![self.db.prepare("UPDATE newsman_state SET revision=CASE WHEN revision=? THEN revision+1 ELSE -1 END WHERE id=1").bind(&[(self.revision as f64).into()])?];
        for ((kind, id), body) in self.writes {
            let stmt = if let Some(v) = body {
                self.db.prepare("INSERT INTO newsman_records(kind,id,body) VALUES(?,?,?) ON CONFLICT(kind,id) DO UPDATE SET body=excluded.body").bind(&[kind.into(),id.into(),v.to_string().into()])?
            } else {
                self.db
                    .prepare("DELETE FROM newsman_records WHERE kind=? AND id=?")
                    .bind(&[kind.into(), id.into()])?
            };
            batch.push(stmt);
        }
        match self.db.batch(batch).await {
            Ok(_) => Ok(()),
            Err(error) if format!("{error:?}").contains("CHECK constraint") => Err(AppError::new(
                "concurrent_update",
                "Another command committed first. Retry with the same key.",
                409,
            )),
            Err(error) => Err(error.into()),
        }
    }
}
