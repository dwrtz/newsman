use crate::audience::{
    address_digest, consent_event, eligible, email_key, lookup, matches, new_subscriber,
    persist_subscriber, proof_secret,
};
use crate::core::{AppError, Res, Tx, expect_version, now, page_values, req_str, sha256, uid};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use worker::Range;

const CHUNK: usize = 49_152;
const MAX_FILE: usize = 25 * 1024 * 1024;
const MAX_PLAN: usize = 25 * 1024 * 1024;
const BATCH: usize = 20;

fn invalid(s: impl Into<String>) -> AppError {
    AppError::invalid(s)
}
fn forbidden() -> AppError {
    AppError::new(
        "permission_denied",
        "This private resource is unavailable to this actor or grant",
        403,
    )
}
fn timestamp() -> i64 {
    chrono::DateTime::parse_from_rfc3339(&now())
        .expect("UTC clock")
        .timestamp()
}
fn utc_at(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .expect("application timestamp")
        .to_rfc3339()
}
fn object_view(v: &Value, keys: &[&str]) -> Value {
    let mut out = json!({});
    for k in keys {
        if !v[*k].is_null() {
            out[*k] = v[*k].clone();
        }
    }
    out
}
fn file_view(v: &Value) -> Value {
    object_view(v, &["id", "kind", "size", "version", "sha256", "state"])
}
fn job_view(v: &Value) -> Value {
    object_view(
        v,
        &[
            "id",
            "kind",
            "version",
            "state",
            "counts",
            "result_asset_id",
            "error",
        ],
    )
}
fn metric(label: &str, value: usize) -> Value {
    json!({"label":label,"value":value,"unit":"count"})
}
fn sensitive(kind: &str) -> bool {
    !matches!(kind, "hero" | "issue_bundle" | "archive_bundle")
}
fn allowed(tx: &Tx<'_>, v: &Value) -> bool {
    v["owner"] == tx.actor.id && v["scope"].as_str().is_some_and(|s| tx.actor.has(s))
}
fn check_access(tx: &Tx<'_>, v: &Value) -> Res<()> {
    if allowed(tx, v) {
        Ok(())
    } else {
        Err(forbidden())
    }
}
fn fresh(f: &Value) -> Res<()> {
    if f["expires"].as_i64().is_some_and(|t| t <= timestamp()) {
        return Err(AppError::new("file_expired", "The file has expired", 410));
    }
    Ok(())
}
async fn owned_file(tx: &Tx<'_>, id: &str) -> Res<Value> {
    let f = tx.require("file", id).await?;
    check_access(tx, &f)?;
    fresh(&f)?;
    Ok(f)
}
pub async fn checked_file(tx: &Tx<'_>, id: &str, kind: &str) -> Res<Value> {
    let f = owned_file(tx, id).await?;
    if f["kind"] != kind || f["state"] != "complete" {
        return Err(invalid(
            "The file must be sealed and match the expected purpose",
        ));
    }
    Ok(f)
}
async fn bytes_for(tx: &Tx<'_>, key: &str, range: Option<(u64, u64)>) -> Res<Vec<u8>> {
    if matches!(range, Some((_, 0))) {
        return Ok(Vec::new());
    }
    let bucket = tx.env.bucket("FILES")?;
    let mut get = bucket.get(key);
    if let Some((offset, length)) = range {
        get = get.range(Range::OffsetWithLength { offset, length });
    }
    let obj = get.execute().await?.ok_or_else(|| {
        AppError::new("storage_unavailable", "Private object is unavailable", 503)
    })?;
    Ok(obj
        .body()
        .ok_or_else(|| invalid("Object has no body"))?
        .bytes()
        .await?)
}
pub async fn read_asset(tx: &Tx<'_>, id: &str, kind: &str, max: usize) -> Res<Vec<u8>> {
    let f = checked_file(tx, id, kind).await?;
    if f["size"].as_u64().unwrap_or(u64::MAX) > max as u64 {
        return Err(invalid(
            "The file exceeds the size limit for this operation",
        ));
    }
    let data = bytes_for(tx, req_str(&f, "object_key")?, None).await?;
    if f["sha256"] != sha256(&data) {
        return Err(invalid("Private object checksum mismatch"));
    }
    Ok(data)
}
fn result_scope(tx: &Tx<'_>, kind: &str) -> Res<&'static str> {
    let scope = if sensitive(kind) { "admin" } else { "write" };
    if tx.actor.has(scope) {
        Ok(scope)
    } else {
        Err(forbidden())
    }
}
async fn result_file(tx: &mut Tx<'_>, kind: &str, data: Vec<u8>, scope: &str) -> Res<Value> {
    if data.len() > MAX_FILE {
        return Err(invalid("Result exceeds 25 MiB; use a narrower selection"));
    }
    let hash = sha256(&data);
    let id = uid("file");
    let key = format!("assets/{id}/{hash}");
    let size = data.len();
    tx.env.bucket("FILES")?.put(&key, data).execute().await?;
    let f = json!({"id":id,"version":1,"kind":kind,"size":size,"sha256":hash,"state":"complete","scope":scope,"owner":tx.actor.id,"object_key":key,"parts_pruned":true,"expires":timestamp()+86400});
    tx.put("file", &id, f.clone())?;
    Ok(f)
}
pub async fn result_job(
    tx: &mut Tx<'_>,
    kind: &str,
    file_kind: &str,
    bytes: Vec<u8>,
    counts: Vec<Value>,
    scope: &str,
) -> Res<Value> {
    let f = result_file(tx, file_kind, bytes, scope).await?;
    let id = uid("job");
    let j = json!({"id":id,"kind":kind,"version":1,"state":"succeeded","owner":tx.actor.id,"scope":scope,"counts":counts,"result_asset_id":f["id"]});
    tx.put("job", &id, j.clone())?;
    Ok(job_view(&j))
}

fn image_meta(bytes: &[u8]) -> Option<(&'static str, u32, u32)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        && bytes.get(12..16) == Some(b"IHDR")
        && bytes.len() >= 33
    {
        return Some((
            "image/png",
            u32::from_be_bytes(bytes[16..20].try_into().ok()?),
            u32::from_be_bytes(bytes[20..24].try_into().ok()?),
        ));
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        let mut p = 2usize;
        while p + 4 <= bytes.len() && bytes[p] == 0xff {
            while p < bytes.len() && bytes[p] == 0xff {
                p += 1;
            }
            let m = *bytes.get(p)?;
            p += 1;
            if m == 0xd9 || m == 0xda {
                return None;
            }
            let n = u16::from_be_bytes(bytes.get(p..p + 2)?.try_into().ok()?) as usize;
            if n < 2 || p + n > bytes.len() {
                return None;
            }
            if matches!(m,0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) && n >= 8 {
                let h = u16::from_be_bytes(bytes[p + 3..p + 5].try_into().ok()?);
                let w = u16::from_be_bytes(bytes[p + 5..p + 7].try_into().ok()?);
                return Some(("image/jpeg", w as u32, h as u32));
            }
            p += n;
        }
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") && bytes.len() >= 30 {
        let s = &bytes[12..16];
        let b = &bytes[20..];
        if s == b"VP8X" {
            let w = 1 + u32::from_le_bytes([b[4], b[5], b[6], 0]);
            let h = 1 + u32::from_le_bytes([b[7], b[8], b[9], 0]);
            return Some(("image/webp", w, h));
        }
        if s == b"VP8L" && b[0] == 0x2f {
            let v = u32::from_le_bytes(b[1..5].try_into().ok()?);
            return Some(("image/webp", 1 + (v & 0x3fff), 1 + ((v >> 14) & 0x3fff)));
        }
        if s == b"VP8 " && b.get(3..6) == Some(&[0x9d, 0x01, 0x2a]) {
            return Some((
                "image/webp",
                u32::from(u16::from_le_bytes([b[6], b[7]]) & 0x3fff),
                u32::from(u16::from_le_bytes([b[8], b[9]]) & 0x3fff),
            ));
        }
    }
    None
}

async fn files(tx: &mut Tx<'_>, name: &str, a: &Value) -> Res<Value> {
    if name == "file_begin" {
        let size = a["size"]
            .as_u64()
            .ok_or_else(|| invalid("Expected file size"))?;
        if size > MAX_FILE as u64 {
            return Err(invalid("File exceeds 25 MiB"));
        }
        let kind = req_str(a, "kind")?;
        let scope = result_scope(tx, kind)?;
        let id = uid("file");
        let f = json!({"id":id,"version":1,"size":size,"sha256":a["sha256"],"state":"open","kind":kind,"owner":tx.actor.id,"scope":scope,"next":0,"expires":timestamp()+86400});
        tx.put("file", &id, f.clone())?;
        return Ok(file_view(&f));
    }
    let id = req_str(a, "id")?;
    let mut f = owned_file(tx, id).await?;
    let size = f["size"].as_u64().unwrap_or(0);
    let offset = a["offset"].as_u64().unwrap_or(0);
    match name {
        "file_write" => {
            let payload = STANDARD
                .decode(req_str(a, "base64")?)
                .map_err(|_| invalid("Invalid base64 chunk"))?;
            if payload.is_empty()
                || payload.len() > CHUNK
                || offset.saturating_add(payload.len() as u64) > size
            {
                return Err(invalid("Invalid chunk size or offset"));
            }
            let hash = sha256(&payload);
            if a["chunk_sha256"] != hash {
                return Err(invalid("Chunk checksum mismatch"));
            }
            let end = offset + payload.len() as u64;
            let part_id = format!("{id}.{offset:016}");
            if let Some(part) = tx.get("file_part", &part_id).await? {
                if part["sha256"] == hash {
                    return Ok(json!({"id":id,"next_offset":end}));
                }
                return Err(invalid("Chunk conflicts with existing bytes"));
            }
            if f["state"] == "complete" {
                let sealed = bytes_for(
                    tx,
                    req_str(&f, "object_key")?,
                    Some((offset, payload.len() as u64)),
                )
                .await?;
                if sha256(&sealed) == hash {
                    return Ok(json!({"id":id,"next_offset":end}));
                }
                return Err(invalid("Chunk conflicts with the sealed file"));
            }
            if f["state"] != "open" || f["next"].as_u64() != Some(offset) {
                return Err(invalid(
                    "Chunk offset must match the next byte of an open upload",
                ));
            }
            let key = format!("staging/{id}/{offset}/{hash}");
            tx.env.bucket("FILES")?.put(&key, payload).execute().await?;
            tx.put(
                "file_part",
                &part_id,
                json!({"id":part_id,"file":id,"key":key,"end":end,"sha256":hash}),
            )?;
            f["next"] = json!(end);
            f["version"] = json!(f["version"].as_u64().unwrap_or(1) + 1);
            tx.put("file", id, f)?;
            Ok(json!({"id":id,"next_offset":end}))
        }
        "file_finish" => {
            if f["state"] == "complete" {
                return Ok(file_view(&f));
            }
            if f["next"].as_u64() != Some(size) {
                return Err(invalid("Upload is incomplete"));
            }
            let mut payload = Vec::with_capacity(size as usize);
            let mut next = 0;
            while next < size {
                let part = tx.require("file_part", &format!("{id}.{next:016}")).await?;
                let data = bytes_for(tx, req_str(&part, "key")?, None).await?;
                let end = next + data.len() as u64;
                if end <= next
                    || end > size
                    || part["end"] != end
                    || part["sha256"] != sha256(&data)
                {
                    return Err(invalid("Stored part mismatch"));
                }
                payload.extend(data);
                next = end;
            }
            if f["sha256"] != sha256(&payload) {
                return Err(invalid("Final file checksum mismatch"));
            }
            if f["kind"] == "hero" {
                let (mime, w, h) = image_meta(&payload)
                    .filter(|(_, w, h)| {
                        *w > 0
                            && *h > 0
                            && *w <= 10000
                            && *h <= 10000
                            && (*w as u64) * (*h as u64) <= 40000000
                    })
                    .ok_or_else(|| {
                        invalid("Expected a PNG, JPEG or WebP image with supported dimensions")
                    })?;
                f["mime"] = json!(mime);
                f["width"] = json!(w);
                f["height"] = json!(h);
                f.as_object_mut().unwrap().remove("expires");
            } else {
                let content =
                    std::str::from_utf8(&payload).map_err(|_| invalid("Expected UTF-8 input"))?;
                if f["kind"] == "issue_bundle" {
                    let _: Value = serde_json::from_str(content)
                        .map_err(|_| invalid("Issue bundle must contain JSON"))?;
                    f["mime"] = json!("application/json");
                } else {
                    f["mime"] = json!("text/plain");
                }
            }
            let key = format!("assets/{id}/{}", req_str(&f, "sha256")?);
            tx.env.bucket("FILES")?.put(&key, payload).execute().await?;
            f["object_key"] = json!(key);
            f["state"] = json!("complete");
            f["version"] = json!(f["version"].as_u64().unwrap_or(1) + 1);
            tx.put("file", id, f.clone())?;
            Ok(file_view(&f))
        }
        "file_read" => {
            if f["state"] != "complete" || offset > size {
                return Err(invalid("Invalid read position or incomplete file"));
            }
            let length = a["length"]
                .as_u64()
                .unwrap_or(CHUNK as u64)
                .min(CHUNK as u64)
                .min(size - offset);
            let data = bytes_for(tx, req_str(&f, "object_key")?, Some((offset, length))).await?;
            if data.len() as u64 != length {
                return Err(invalid("Truncated object range"));
            }
            let mut out = json!({"id":id,"offset":offset,"base64":STANDARD.encode(data),"size":size,"sha256":f["sha256"]});
            if offset + length < size {
                out["next_offset"] = json!(offset + length);
            }
            Ok(out)
        }
        _ => Err(invalid("Unknown transfer operation")),
    }
}

fn field(row: &Value, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|k| row.get(k))
        .find_map(|v| match v {
            Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}
fn normalize_key(s: &str) -> String {
    s.trim_start_matches('\u{feff}')
        .trim()
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}
fn import_rows(data: &[u8]) -> Res<Vec<Value>> {
    let text = std::str::from_utf8(data)
        .map_err(|_| invalid("Expected UTF-8 rows"))?
        .trim_start_matches('\u{feff}');
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    if text.trim_start().starts_with('{') {
        return text
            .lines()
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                let v: Value =
                    serde_json::from_str(s).map_err(|_| invalid("Malformed JSONL input"))?;
                let obj = v
                    .as_object()
                    .ok_or_else(|| invalid("Expected JSONL objects"))?;
                let mut out = Map::new();
                for (key, v) in obj {
                    if out.insert(normalize_key(key), v.clone()).is_some() {
                        return Err(invalid("Ambiguous JSONL keys"));
                    }
                }
                Ok(Value::Object(out))
            })
            .collect();
    }
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::Headers)
        .from_reader(text.as_bytes());
    let keys = reader
        .headers()
        .map_err(|_| invalid("Invalid CSV headers"))?
        .iter()
        .map(normalize_key)
        .collect::<Vec<_>>();
    if keys.iter().any(|s| s.is_empty()) || keys.iter().collect::<BTreeSet<_>>().len() != keys.len()
    {
        return Err(invalid("CSV headers must be unique"));
    }
    let mut out = Vec::new();
    for (i, r) in reader.records().enumerate() {
        let row = r.map_err(|_| invalid(format!("Invalid CSV row {}", i + 2)))?;
        out.push(Value::Object(
            keys.iter()
                .cloned()
                .zip(row.iter().map(|s| json!(s)))
                .collect(),
        ));
    }
    Ok(out)
}
fn clean_time(v: String) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(&v)
        .ok()
        .map(|t| t.to_utc().to_rfc3339())
}
fn row_disposition(row: &Value, kind: &str, beehiiv: bool) -> (&'static str, &'static str) {
    let status = normalize_key(&field(row, &["status", "state", "subscription_status"]));
    let reason = normalize_key(&field(row, &["suppression_reason", "reason"]));
    let test = field(row, &["is_qa", "test_only"]).eq_ignore_ascii_case("true");
    if status == "erased" || reason == "erased" {
        return ("erased", "erased");
    }
    if ["complained", "complaint", "spam_complaint"].contains(&status.as_str())
        || reason.contains("complaint")
    {
        return ("suppressed", "complaint");
    }
    if ["bounced", "hard_bounce", "invalid"].contains(&status.as_str()) || reason.contains("bounce")
    {
        return ("suppressed", "hard_bounce");
    }
    if ["unsubscribed", "inactive_unsubscribed", "opted_out"].contains(&status.as_str())
        || reason == "reader_opt_out"
        || (beehiiv && clean_time(field(row, &["unsubscribed_on", "unsubscribed_at"])).is_some())
    {
        return ("unsubscribed", "reader_opt_out");
    }
    if kind == "suppressions" || status == "suppressed" {
        return ("suppressed", "administrative");
    }
    if test || status == "test_only" {
        return ("test_only", "test_only");
    }
    if matches!(status.as_str(), "pending" | "validating" | "unconfirmed") {
        return ("pending", "import_pending");
    }
    let source = normalize_key(&field(
        row,
        &[
            "evidence_kind",
            "consent_evidence",
            "consent_source",
            "evidence",
        ],
    ));
    let verified = clean_time(field(
        row,
        &[
            "confirmed_at",
            "first_confirmed_at",
            "subscription_confirmed_at",
        ],
    ))
    .is_some()
        || matches!(
            source.as_str(),
            "double_opt_in" | "verified" | "provider_active_opt_in" | "native_double_opt_in"
        )
        || (beehiiv
            && status == "active"
            && field(row, &["native_double_opt_in"]).eq_ignore_ascii_case("true"));
    if status == "active" && verified {
        return ("active", "imported_consent");
    }
    ("held", "unverified_import")
}
fn rank(status: &Value, reason: &Value) -> u8 {
    match status.as_str().unwrap_or("") {
        "erased" => 100,
        "suppressed" if reason == "complaint" => 95,
        "suppressed" => 90,
        "unsubscribed" => 80,
        "test_only" => 70,
        "held" => 60,
        "active" => 50,
        _ => 40,
    }
}
fn tags_from(row: &Value) -> Vec<String> {
    let tags = if let Some(values) = row["tags"].as_array() {
        values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>()
    } else {
        field(row, &["tags"])
            .split('|')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .collect()
    };
    tags.into_iter()
        .filter(|v| v.chars().count() <= 64)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(50)
        .collect()
}
fn mapped_row(row: &Value, kind: &str, beehiiv: bool) -> Res<Value> {
    let email = email_key(&field(row, &["email", "email_address", "subscriber_email"]))
        .ok_or_else(|| invalid("missing or invalid email"))?;
    let (state, reason) = row_disposition(row, kind, beehiiv);
    let mut touch = json!({});
    for key in ["utm_source", "utm_medium", "utm_campaign", "utm_content"] {
        let value = row["first_touch"][key]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| field(row, &[key]));
        if !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            touch[key] = json!(value);
        }
    }
    let mut mapped = json!({"email":email,"status":state,"reason":reason,"tags":tags_from(row),"first_touch":touch,"external":field(row,&["external_id","subscription_id","subscriber_id","id"])});
    for (key, aliases) in [
        (
            "created_at",
            &[
                "created_at",
                "subscription_created_at",
                "subscribed_at",
                "subscribed_on",
            ][..],
        ),
        (
            "confirmed_at",
            &[
                "confirmed_at",
                "first_confirmed_at",
                "subscription_confirmed_at",
            ][..],
        ),
    ] {
        if let Some(t) = clean_time(field(row, aliases)) {
            mapped[key] = json!(t);
        }
    }
    Ok(mapped)
}

fn mapping_version(value: &str) -> Res<bool> {
    match value {
        "portable-v1" | "v1" | "1" | "newsman-v1" => Ok(false),
        "beehiiv-v1" | "beehiiv-csv-v1" => Ok(true),
        _ => Err(invalid("Choose portable-v1 or beehiiv-v1")),
    }
}

async fn staged(
    tx: &Tx<'_>,
    files: &Value,
    beehiiv: bool,
) -> Res<(Vec<Value>, Vec<Value>, Vec<Value>, Vec<Value>, usize)> {
    let ids = files
        .as_array()
        .ok_or_else(|| invalid("Choose staged files"))?;
    if ids.is_empty() {
        return Err(invalid("Choose at least one staged file"));
    }
    let mut unique = BTreeSet::new();
    let mut grouped = BTreeMap::<String, Vec<Value>>::new();
    let mut extras = Vec::new();
    let mut diag = Vec::new();
    let mut inputs = Vec::new();
    let mut rows = 0;
    let mut bytes = 0usize;
    let mut external = BTreeMap::<String, BTreeSet<String>>::new();
    for v in ids {
        let id = v
            .as_str()
            .ok_or_else(|| invalid("Invalid file reference"))?;
        if !unique.insert(id) {
            return Err(invalid("Input file appears twice"));
        }
        let f = owned_file(tx, id).await?;
        let kind = req_str(&f, "kind")?;
        if !matches!(
            kind,
            "audience_import"
                | "suppressions"
                | "send_history"
                | "historical_metrics"
                | "consent_history"
        ) {
            return Err(invalid("Unsupported import file purpose"));
        }
        bytes += f["size"].as_u64().unwrap_or(0) as usize;
        if bytes > MAX_PLAN {
            return Err(invalid("Import totals exceed 25 MiB"));
        }
        let raw = read_asset(tx, id, kind, MAX_PLAN).await?;
        inputs.push(json!({"id":id,"sha256":f["sha256"],"kind":kind}));
        for (index, row) in import_rows(&raw)?.into_iter().enumerate() {
            if matches!(
                kind,
                "send_history" | "historical_metrics" | "consent_history"
            ) {
                extras.push(json!({"kind":kind,"data":row}));
                continue;
            }
            rows += 1;
            match mapped_row(&row, kind, beehiiv) {
                Ok(mapped) => {
                    let email = req_str(&mapped, "email")?.to_owned();
                    if let Some(id) = mapped["external"].as_str().filter(|v| !v.is_empty()) {
                        external.entry(id.into()).or_default().insert(email.clone());
                    }
                    grouped.entry(email).or_default().push(mapped);
                }
                Err(e) => diag.push(json!({"file":id,"row":index+2,"reason":e.message})),
            }
        }
    }
    let ambiguous = external
        .values()
        .filter(|v| v.len() > 1)
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut records = Vec::new();
    for (mail, mut values) in grouped {
        values.sort_by(|a, b| {
            rank(&b["status"], &b["reason"])
                .cmp(&rank(&a["status"], &a["reason"]))
                .then_with(|| a.to_string().cmp(&b.to_string()))
        });
        let mut chosen = values[0].clone();
        let ids = values
            .iter()
            .filter_map(|v| v["external"].as_str())
            .filter(|s| !s.is_empty())
            .collect::<BTreeSet<_>>();
        if (ambiguous.contains(&mail) || ids.len() > 1)
            && rank(&chosen["status"], &chosen["reason"]) < 80
        {
            chosen["status"] = json!("held");
            chosen["reason"] = json!("identity_conflict");
        }
        records.push(chosen);
    }
    records.sort_by(|a, b| {
        rank(&b["status"], &b["reason"])
            .cmp(&rank(&a["status"], &a["reason"]))
            .then_with(|| a["email"].as_str().cmp(&b["email"].as_str()))
    });
    Ok((records, extras, diag, inputs, rows))
}
fn digest_set(keys: &BTreeSet<String>) -> String {
    sha256(
        serde_json::to_vec(keys)
            .expect("serializable keys")
            .as_slice(),
    )
}
async fn eligibility(tx: &Tx<'_>) -> Res<BTreeSet<String>> {
    Ok(tx
        .list("subscriber")
        .await?
        .into_iter()
        .filter(eligible)
        .filter_map(|s| s["address_digest"].as_str().map(str::to_owned))
        .collect())
}

async fn prepare(tx: &mut Tx<'_>, a: &Value) -> Res<Value> {
    let mapping = req_str(a, "mapping_version")?;
    let (records, extras, diag, inputs, rows) =
        staged(tx, &a["files"], mapping_version(mapping)?).await?;
    let mut eligible_set = eligibility(tx).await?;
    let mut counts = BTreeMap::<String, usize>::new();
    let secret = proof_secret(tx.env)?;
    let previous = tx
        .list("subscriber")
        .await?
        .into_iter()
        .filter_map(|v| Some((v["address_digest"].as_str()?.to_owned(), v)))
        .collect::<BTreeMap<_, _>>();
    for r in &records {
        let key = address_digest(&secret, req_str(r, "email")?);
        let mut status = req_str(r, "status")?;
        if let Some(s) = previous.get(&key) {
            if protected_status(s)
                && rank(&s["status"], &s["suppression_reason"]) >= rank(&r["status"], &r["reason"])
            {
                status = req_str(s, "status")?;
            } else if s["status"] == "active"
                && (status == "held" || status == "pending")
                && r["reason"] != "identity_conflict"
            {
                status = "active";
            }
        }
        *counts.entry(status.into()).or_default() += 1;
        if status == "active" {
            eligible_set.insert(key);
        } else {
            eligible_set.remove(&key);
        }
    }
    let mut metrics = vec![
        metric("input_rows", rows),
        metric("identities", records.len()),
        metric(
            "duplicates",
            rows.saturating_sub(records.len() + diag.len()),
        ),
        metric("rejected", diag.len()),
        metric("history_rows", extras.len()),
    ];
    metrics.extend(counts.into_iter().map(|(k, n)| metric(&k, n)));
    let id = uid("import");
    let eligible_digest = digest_set(&eligible_set);
    let payload = serde_json::to_vec(&json!({"rows":records,"history":extras}))?;
    let hash = sha256(&payload);
    let digest = sha256(&serde_json::to_vec(
        &json!({"mapping":mapping,"cutoff":a["cutoff"],"sources":inputs,"payload":hash,"eligibility":eligible_digest}),
    )?);
    let object_key = format!("plans/{id}/{hash}");
    tx.env
        .bucket("FILES")?
        .put(&object_key, payload)
        .execute()
        .await?;
    let detail = result_file(
        tx,
        "import_diagnostics",
        serde_json::to_vec(&diag)?,
        "admin",
    )
    .await?;
    let expiry = timestamp() + 86400;
    let plan = json!({"id":id,"version":1,"digest":digest,"eligible_digest":eligible_digest,"counts":metrics,"diagnostics_asset_id":detail["id"],
        "expires_at":utc_at(expiry),"expires":expiry,"owner":tx.actor.id,"scope":"admin","sources":inputs,"payload_key":object_key,"payload_hash":hash,"cutoff":a["cutoff"]});
    tx.put("import_plan", &id, plan.clone())?;
    Ok(object_view(
        &plan,
        &[
            "id",
            "digest",
            "eligible_digest",
            "counts",
            "diagnostics_asset_id",
            "expires_at",
        ],
    ))
}

fn protected_status(v: &Value) -> bool {
    matches!(
        v["status"].as_str(),
        Some("suppressed" | "erased" | "unsubscribed" | "test_only")
    )
}
async fn apply_row(tx: &mut Tx<'_>, r: &Value) -> Res<()> {
    let email = req_str(r, "email")?;
    let existing = lookup(tx, email).await?;
    let status = req_str(r, "status")?;
    let mut s = existing.clone().unwrap_or_else(|| new_subscriber(email));
    if let Some(old) = existing.as_ref() {
        if (protected_status(old)
            && rank(&old["status"], &old["suppression_reason"]) >= rank(&r["status"], &r["reason"]))
            || (old["status"] == "active"
                && matches!(status, "held" | "pending")
                && r["reason"] != "identity_conflict")
        {
            return Ok(());
        }
    }
    let before = s.clone();
    s["status"] = r["status"].clone();
    let id = req_str(&s, "id")?.to_owned();
    if status == "erased" {
        let version = s["version"].as_u64().unwrap_or(1) + u64::from(existing.is_some());
        return crate::audience::erase_record(tx, &s, version).await;
    }
    if matches!(status, "suppressed" | "unsubscribed") {
        s["suppression_reason"] = r["reason"].clone();
        for k in ["challenge_nonce", "challenge_until"] {
            s.as_object_mut().unwrap().remove(k);
        }
    }
    if existing.is_none() {
        s["tags"] = r["tags"].clone();
        s["first_touch"] = r["first_touch"].clone();
        if !r["created_at"].is_null() {
            s["created_at"] = r["created_at"].clone();
        }
    }
    if status == "active" {
        s["first_confirmation_recorded"] = json!(true);
        if s["confirmed_at"].is_null() && r["confirmed_at"].is_string() {
            s["confirmed_at"] = r["confirmed_at"].clone();
        }
    }
    if existing.is_none() || before["status"] != s["status"] {
        s["evidence_kind"] = r["reason"].clone();
    }
    if existing.is_some() && before == s {
        return Ok(());
    }
    if existing.is_some() {
        s["version"] = json!(s["version"].as_u64().unwrap_or(0) + 1);
    }
    persist_subscriber(tx, &s)?;
    consent_event(
        tx,
        &id,
        "migration_observed",
        "import",
        r["reason"].as_str(),
    )
}

async fn advance(tx: &mut Tx<'_>, j: &mut Value, p: &Value) -> Res<()> {
    let raw = bytes_for(tx, req_str(p, "payload_key")?, None).await?;
    if p["payload_hash"] != sha256(&raw) {
        return Err(invalid("Reviewed import data changed"));
    }
    let data: Value = serde_json::from_slice(&raw)?;
    let rows = data["rows"]
        .as_array()
        .ok_or_else(|| invalid("Invalid import plan"))?;
    let history = data["history"]
        .as_array()
        .ok_or_else(|| invalid("Invalid history plan"))?;
    let start = j["offset"].as_u64().unwrap_or(0) as usize;
    let total = rows.len() + history.len();
    let end = (start + BATCH).min(total);
    for index in start..end {
        if index < rows.len() {
            apply_row(tx, &rows[index]).await?;
        } else {
            let r = &history[index - rows.len()];
            let kind = match r["kind"].as_str() {
                Some("send_history") => "historical_send",
                Some("historical_metrics") => "historical_metric",
                _ => "historical_consent",
            };
            let mut body = r["data"].clone();
            let id = sha256(&serde_json::to_vec(r)?);
            body["id"] = json!(id);
            body["source"] = json!("import");
            tx.put(kind, &id, body)?;
        }
    }
    j["offset"] = json!(end);
    j["state"] = json!(if end == total { "succeeded" } else { "running" });
    j["version"] = json!(j["version"].as_u64().unwrap_or(0) + 1);
    if end == total {
        tx.delete("import_guard", "active")?;
        let actual = eligibility(tx).await?;
        let mid = req_str(p, "id")?;
        tx.put("migration_baseline",mid,json!({"id":mid,"counts":p["counts"],"cutoff":p["cutoff"],"eligible_digest":digest_set(&actual),"plan_digest":p["eligible_digest"],"job_id":j["id"],"at":now()}))?;
    }
    let jid = req_str(j, "id")?.to_owned();
    tx.put("job", &jid, j.clone())
}
async fn commit_import(tx: &mut Tx<'_>, a: &Value) -> Res<Value> {
    let id = req_str(a, "plan_id")?;
    let mut p = tx.require("import_plan", id).await?;
    check_access(tx, &p)?;
    if p["digest"] != a["plan_digest"] {
        return Err(invalid("Import digest does not match the reviewed plan"));
    }
    if let Some(job) = p["job_id"].as_str() {
        return Ok(job_view(&tx.require("job", job).await?));
    }
    if p["expires"].as_i64().unwrap_or(0) <= timestamp() {
        return Err(AppError::new(
            "expired_plan",
            "Prepare a fresh import plan",
            409,
        ));
    }
    if tx.get("import_guard", "active").await?.is_some() {
        return Err(AppError::new(
            "import_in_progress",
            "Finish the active import first",
            409,
        ));
    }
    let jid = uid("job");
    let mut j = json!({"id":jid,"kind":"import","state":"running","scope":"admin","owner":tx.actor.id,"version":1,"counts":p["counts"],"plan_id":id,"offset":0,"result_asset_id":p["diagnostics_asset_id"]});
    tx.put(
        "import_guard",
        "active",
        json!({"id":"active","job_id":jid}),
    )?;
    p["job_id"] = json!(jid);
    tx.put("import_plan", id, p.clone())?;
    advance(tx, &mut j, &p).await?;
    Ok(job_view(&j))
}

// Large imports resume from a durable cursor. Record failures in a fresh transaction
// so partially applied changes from a failed batch cannot escape.
async fn advance_import(env: &worker::Env) -> Res<()> {
    let mut tx = Tx::begin(env, crate::core::Actor::system()).await?;
    let Some(guard) = tx.get("import_guard", "active").await? else {
        return Ok(());
    };
    let jid = req_str(&guard, "job_id")?.to_owned();
    let mut job = tx.require("job", &jid).await?;
    if job["state"] != "running" {
        return Ok(());
    }
    let plan = tx.require("import_plan", req_str(&job, "plan_id")?).await?;
    match advance(&mut tx, &mut job, &plan).await {
        Ok(()) => tx.commit().await,
        Err(e) => {
            let mut failure = Tx::begin(env, crate::core::Actor::system()).await?;
            let mut latest = failure.require("job", &jid).await?;
            if latest["state"] != "running" || latest["version"] != job["version"] {
                return Ok(());
            }
            let count = latest["failures"].as_u64().unwrap_or(0) + 1;
            latest["failures"] = json!(count);
            latest["error"] = json!(e.message);
            latest["version"] = json!(latest["version"].as_u64().unwrap_or(0) + 1);
            if count >= 3 || e.status < 500 {
                latest["state"] = json!("failed");
                // A repaired or restored private object can resume at the last
                // successfully committed batch, including after manual repair.
                latest["retryable"] = json!(true);
            }
            failure.put("job", &jid, latest)?;
            failure.commit().await
        }
    }
}

async fn prune(env: &worker::Env) -> Res<()> {
    let mut tx = Tx::begin(env, crate::core::Actor::system()).await?;
    let t = timestamp();
    let expired = |v: &Value| v["expires"].as_i64().is_some_and(|n| n <= t);
    let bucket = env.bucket("FILES")?;
    for record in tx
        .list("reader_rate")
        .await?
        .into_iter()
        .filter(|v| v["expires_at"].as_i64().is_some_and(|n| n <= t))
        .take(24)
    {
        tx.delete("reader_rate", req_str(&record, "id")?)?;
    }
    // Expired object metadata blocks access even while physical deletion is in progress.
    let mut parts = 24;
    for record in tx.list("file").await? {
        let id = req_str(&record, "id")?;
        let remove_file = expired(&record);
        if !remove_file && (record["state"] != "complete" || record["parts_pruned"] == true) {
            continue;
        }
        let chunks = tx
            .list("file_part")
            .await?
            .into_iter()
            .filter(|p| p["file"] == id)
            .collect::<Vec<_>>();
        let all = chunks.len() <= parts;
        for part in chunks.into_iter().take(parts) {
            bucket.delete(req_str(&part, "key")?).await?;
            tx.delete("file_part", req_str(&part, "id")?)?;
            parts -= 1;
        }
        if all {
            if remove_file {
                if let Some(key) = record["object_key"].as_str() {
                    bucket.delete(key).await?;
                }
                tx.delete("file", id)?;
            } else {
                let mut sealed = record.clone();
                sealed["parts_pruned"] = json!(true);
                tx.put("file", id, sealed)?;
            }
        }
        if parts == 0 {
            break;
        }
    }
    let active = tx.get("import_guard", "active").await?;
    for mut plan in tx
        .list("import_plan")
        .await?
        .into_iter()
        .filter(|p| expired(p) && p["payload_pruned"] != true)
        .take(12)
    {
        if active
            .as_ref()
            .is_some_and(|g| g["job_id"] == plan["job_id"])
        {
            continue;
        }
        bucket.delete(req_str(&plan, "payload_key")?).await?;
        plan["payload_pruned"] = json!(true);
        let id = req_str(&plan, "id")?.to_owned();
        tx.put("import_plan", &id, plan)?;
    }
    tx.commit().await
}

pub async fn tick(env: &worker::Env) -> Res<()> {
    advance_import(env).await?;
    prune(env).await
}

/// The router calls this after grant validation before serving an idempotency receipt.
pub async fn authorize_receipt(tx: &Tx<'_>, name: &str, a: &Value) -> Res<()> {
    match name {
        "file_begin" => {
            result_scope(tx, req_str(a, "kind")?)?;
        }
        "file_write" | "file_finish" | "file_read" => {
            owned_file(tx, req_str(a, "id")?).await?;
        }
        "import_prepare" | "migration_reconcile" => {
            if !tx.actor.has("admin") {
                return Err(forbidden());
            }
            for v in a["files"].as_array().into_iter().flatten() {
                owned_file(tx, v.as_str().ok_or_else(|| invalid("Invalid file id"))?).await?;
            }
        }
        "import_commit" => check_access(
            tx,
            &tx.require("import_plan", req_str(a, "plan_id")?).await?,
        )?,
        "job_retry" | "job_get" => check_access(tx, &tx.require("job", req_str(a, "id")?).await?)?,
        _ => {}
    }
    Ok(())
}

fn text_cell(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        _ => v.to_string(),
    }
}
fn serialize_rows(rows: &[Value], format: &str, columns: &[&str]) -> Res<Vec<u8>> {
    if format == "jsonl" {
        let mut out = Vec::new();
        for r in rows {
            serde_json::to_writer(&mut out, r)?;
            out.push(b'\n');
        }
        return Ok(out);
    }
    let headers: Vec<String> = if !columns.is_empty() {
        columns.iter().map(|v| v.to_string()).collect()
    } else {
        rows.iter()
            .filter_map(Value::as_object)
            .flat_map(|obj| obj.keys().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    let err = |_: csv::Error| invalid("Could not encode export");
    let mut w = csv::Writer::from_writer(Vec::new());
    if !headers.is_empty() {
        w.write_record(&headers).map_err(err)?;
    }
    for r in rows {
        w.write_record(headers.iter().map(|h| text_cell(&r[h])))
            .map_err(err)?;
    }
    w.into_inner()
        .map_err(|_| invalid("Could not finish export"))
}
fn date_allowed(v: &Value, from: &Value, to: &Value) -> bool {
    if from.is_null() && to.is_null() {
        return true;
    }
    let Some(time) = v
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    else {
        return false;
    };
    let from = from
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
    let to = to
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
    from.is_none_or(|v| time >= v) && to.is_none_or(|v| time < v)
}
async fn export(tx: &mut Tx<'_>, a: &Value) -> Res<Value> {
    let kind = req_str(a, "kind")?;
    let target = a["subscriber_id"].as_str();
    let filter = &a["filter"];
    let from = &a["from"];
    let to = &a["to"];
    let mut rows = Vec::new();
    let mut columns = Vec::new();
    match kind {
        "subscribers" | "suppressions" | "migration_baseline" => {
            columns = vec![
                "id",
                "email",
                "status",
                "tags",
                "created_at",
                "confirmed_at",
                "suppression_reason",
                "evidence_kind",
                "utm_source",
                "utm_medium",
                "utm_campaign",
                "utm_content",
            ];
            for s in tx.list("subscriber").await? {
                if !matches(&s, filter)
                    || target.is_some_and(|t| s["id"] != t)
                    || !date_allowed(&s["created_at"], from, to)
                {
                    continue;
                }
                if kind == "suppressions"
                    && !matches!(
                        s["status"].as_str(),
                        Some("unsubscribed" | "suppressed" | "erased")
                    )
                {
                    continue;
                }
                let mut out = object_view(&s, &columns[..8]);
                if a["format"] == "csv" {
                    out["tags"] = json!(
                        s["tags"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("|")
                    );
                }
                for t in &columns[8..] {
                    if !s["first_touch"][*t].is_null() {
                        out[*t] = s["first_touch"][*t].clone();
                    }
                }
                rows.push(out);
            }
        }
        "subscriber_history" | "deliveries" | "audit" => {
            let (entity, date_key, person) = match kind {
                "subscriber_history" => ("consent_event", "at", "object_id"),
                "deliveries" => ("attempt", "updated_at", "subscriber_id"),
                _ => ("event", "at", "object_id"),
            };
            let allowed = match kind {
                "deliveries" => vec![
                    "id",
                    "version",
                    "subscriber_id",
                    "campaign_id",
                    "state",
                    "provider_id",
                    "updated_at",
                    "accepted_at",
                    "delivered_at",
                ],
                _ => vec![
                    "id",
                    "at",
                    "kind",
                    "actor_id",
                    "object_id",
                    "source",
                    "reason",
                ],
            };
            columns = allowed.clone();
            let permitted_ids = if !filter.is_null() {
                Some(
                    tx.list("subscriber")
                        .await?
                        .iter()
                        .filter(|s| matches(s, filter))
                        .filter_map(|s| s["id"].as_str().map(str::to_owned))
                        .collect::<BTreeSet<_>>(),
                )
            } else {
                None
            };
            for v in tx.list(entity).await? {
                if target.is_some_and(|t| v[person] != t)
                    || permitted_ids
                        .as_ref()
                        .is_some_and(|ids| v[person].as_str().is_none_or(|s| !ids.contains(s)))
                    || !date_allowed(&v[date_key], from, to)
                {
                    continue;
                }
                rows.push(object_view(&v, &allowed));
            }
        }
        "links" | "acquisition" | "cohort" => {
            if target.is_some() || !filter.is_null() {
                return Err(invalid("Aggregate report exports support time ranges"));
            }
            rows = crate::reporting::report(tx, a).await?;
        }
        _ => return Err(invalid("Unsupported export")),
    }
    let count = rows.len();
    let data = serialize_rows(&rows, req_str(a, "format")?, &columns)?;
    result_job(
        tx,
        "export",
        &format!("export_{kind}"),
        data,
        vec![metric("rows", count)],
        "admin",
    )
    .await
}

async fn migration_status(tx: &Tx<'_>) -> Res<Value> {
    let actual = eligibility(tx).await?;
    let digest = digest_set(&actual);
    let mut records = tx.list("migration_baseline").await?;
    records.extend(tx.list("migration_watermark").await?);
    records.sort_by(|a, b| b["at"].as_str().cmp(&a["at"].as_str()));
    let marks: Vec<Value> = records
        .iter()
        .take(100)
        .map(|v| json!({"id":v["id"],"at":v["at"],"kind":"source_watermark","reason":v["cutoff"]}))
        .collect();
    let newest = records.first();
    let mut metrics = newest
        .and_then(|r| r["counts"].as_array())
        .cloned()
        .unwrap_or_default();
    metrics.push(metric("currently_eligible", actual.len()));
    let check = if tx.get("import_guard", "active").await?.is_some() {
        ("warning", "Import in progress; live submissions are held")
    } else if let Some(v) = newest {
        if v["discrepancies"].as_u64().unwrap_or(0) > 0 {
            (
                "warning",
                "The latest delta contains discrepancies. Inspect the reconciliation result.",
            )
        } else {
            (
                "ready",
                "Recorded a source watermark; compare subsequent provider changes before dispatch.",
            )
        }
    } else {
        ("unknown", "No source baseline has been committed")
    };
    Ok(
        json!({"counts":metrics,"eligible_digest":digest,"watermarks":marks,"checks":[{"name":"migration","state":check.0,"detail":check.1}]}),
    )
}
async fn reconcile(tx: &mut Tx<'_>, a: &Value) -> Res<Value> {
    let baseline_id = req_str(a, "baseline_id")?;
    let base = if let Some(b) = tx.get("migration_baseline", baseline_id).await? {
        b
    } else {
        let j = tx.require("job", baseline_id).await?;
        check_access(tx, &j)?;
        tx.require("migration_baseline", req_str(&j, "plan_id")?)
            .await?
    };
    let cutoff = clean_time(req_str(a, "watermark")?.to_owned())
        .ok_or_else(|| invalid("Invalid watermark"))?;
    if let Some(old) = base["cutoff"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    {
        if chrono::DateTime::parse_from_rfc3339(&cutoff).unwrap() < old {
            return Err(invalid("Watermark precedes baseline"));
        }
    }
    let (rows, _, mut diag, _, _) = staged(tx, &a["files"], true).await?;
    let count = rows.len();
    for r in rows {
        let mail = req_str(&r, "email")?;
        let actual = lookup(tx, mail).await?;
        if actual.as_ref().is_none_or(|s| s["status"] != r["status"]) {
            diag.push(json!({"source":r["status"],"email":mail,"destination":actual.as_ref().map(|s|&s["status"])}));
        }
    }
    let n = diag.len();
    let counts = vec![metric("compared", count), metric("discrepancies", n)];
    let j = result_job(
        tx,
        "reconciliation",
        "migration_discrepancies",
        serialize_rows(&diag, "jsonl", &[])?,
        counts.clone(),
        "admin",
    )
    .await?;
    let id = req_str(&j, "id")?;
    tx.put("migration_watermark",id,json!({"id":id,"cutoff":cutoff,"at":now(),"counts":counts,"discrepancies":n,"baseline":base["id"]}))?;
    Ok(j)
}

pub async fn run(tx: &mut Tx<'_>, name: &str, a: &Value) -> Res<Value> {
    match name {
        "file_begin" | "file_write" | "file_finish" | "file_read" => files(tx, name, a).await,
        "export_create" => export(tx, a).await,
        "import_prepare" => prepare(tx, a).await,
        "import_commit" => commit_import(tx, a).await,
        "migration_status" => migration_status(tx).await,
        "migration_reconcile" => reconcile(tx, a).await,
        "jobs_list" => page_values(
            tx.list("job")
                .await?
                .into_iter()
                .filter(|j| allowed(tx, j))
                .map(|j| job_view(&j))
                .collect(),
            a,
        ),
        "job_get" => {
            let j = tx.require("job", req_str(a, "id")?).await?;
            check_access(tx, &j)?;
            Ok(job_view(&j))
        }
        "job_retry" => {
            let id = req_str(a, "id")?;
            let mut j = tx.require("job", id).await?;
            check_access(tx, &j)?;
            expect_version(&j, a)?;
            if j["kind"] != "import" || j["state"] != "failed" || j["retryable"] != true {
                return Err(invalid("This job has no retryable checkpoint"));
            }
            let guard = tx.get("import_guard", "active").await?;
            if guard.as_ref().is_some_and(|v| v["job_id"] != id) {
                return Err(AppError::new(
                    "import_in_progress",
                    "Other import work is running",
                    409,
                ));
            }
            let p = tx.require("import_plan", req_str(&j, "plan_id")?).await?;
            tx.put("import_guard", "active", json!({"id":"active","job_id":id}))?;
            j["state"] = json!("running");
            j.as_object_mut().unwrap().remove("error");
            j["failures"] = json!(0);
            advance(tx, &mut j, &p).await?;
            Ok(job_view(&j))
        }
        _ => Err(AppError::new(
            "not_found",
            "Unknown transfer operation",
            404,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn portable_consent() {
        assert_eq!(
            row_disposition(
                &json!({"status":"inactive","unsubscribed_on":"2026-11-24T08:00:00Z"}),
                "audience_import",
                true
            ),
            ("unsubscribed", "reader_opt_out")
        );

        assert_eq!(
            row_disposition(&json!({"status":"active"}), "audience_import", false).0,
            "held"
        );
        assert_eq!(
            row_disposition(
                &json!({"status":"active","confirmed_at":"2025-06-01T12:00:00Z"}),
                "audience_import",
                false
            )
            .0,
            "active"
        );
        assert_eq!(
            row_disposition(
                &json!({"status":"active","suppression_reason":"complaint"}),
                "audience_import",
                false
            ),
            ("suppressed", "complaint")
        );
    }
    #[test]
    fn csv_and_jsonl() {
        let v = import_rows(b"Email Address,Status\r\n\"a@example.test\",pending\r\n").unwrap();
        assert_eq!(v[0]["email_address"], "a@example.test");
        let round = serialize_rows(&v, "jsonl", &[]).unwrap();
        assert_eq!(import_rows(&round).unwrap(), v);
        assert!(import_rows(b"Email,email\nx,y").is_err());
    }
    #[test]
    fn image_dimensions() {
        let mut p = vec![0; 33];
        p[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        p[12..16].copy_from_slice(b"IHDR");
        p[19] = 8;
        p[23] = 9;
        assert_eq!(image_meta(&p), Some(("image/png", 8, 9)));
        assert!(image_meta(b"<svg/>").is_none());
    }
}
