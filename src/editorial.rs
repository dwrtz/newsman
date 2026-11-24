use std::{borrow::Cow, collections::HashSet};

use ammonia::{Builder, UrlRelative};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::DateTime;
use chrono_tz::Tz;
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use url::Url;
use worker::{Env, Fetch, Method, Request, RequestInit, RequestRedirect, Response};

use crate::{
    core::{
        Actor, AppError, Res, Tx, conf, expect_version, local, now, page_values, req_str, sha256,
        uid,
    },
    transfers::{checked_file, read_asset, result_job},
};

const MAX_SNAPSHOT: usize = 512 * 1024;
const BODY_SLOT: &str = "{{body}}";
const SLOT_ELEMENT: &str = "<newsman-body></newsman-body>";
const SLOT_TAG: &str = "newsman-body";

fn select(value: &Value, keys: &[&str]) -> Value {
    let mut out = json!({});
    for key in keys {
        if let Some(field) = value.get(key) {
            out[key] = field.clone();
        }
    }
    out
}

fn public_issue(value: &Value) -> Value {
    let mut out = select(
        value,
        &[
            "id",
            "version",
            "draft",
            "digest",
            "state",
            "archive_verified_at",
        ],
    );
    if let Some(id) = value.get("issue_id") {
        out["id"] = id.clone();
    }
    out
}

fn row(value: &Value) -> Value {
    json!({"id":value["id"], "version":value["version"], "title":value["draft"]["title"],
        "subject":value["draft"]["subject"], "updated_at":value["updated_at"], "state":value["state"]})
}

#[derive(serde::Serialize, serde::Deserialize)]
struct IssueCursor {
    revision: i64,
    updated_at: String,
    id: String,
    filter: String,
}

fn issue_filter(args: &Value) -> (String, Option<&str>, String) {
    let search = args["search"].as_str().unwrap_or("").trim().to_lowercase();
    let state = args["state"].as_str();
    let signature = sha256(json!([&search, state]).to_string().as_bytes());
    (search, state, signature)
}

fn issue_cursor(args: &Value, current: i64, filter: &str) -> Res<Option<IssueCursor>> {
    let Some(cursor) = args["cursor"].as_str() else {
        return Ok(None);
    };
    let invalid = || AppError::invalid("Invalid cursor for this message list.");
    let bytes = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| invalid())?;
    let cursor: IssueCursor = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if cursor.filter != filter
        || cursor.revision < 0
        || cursor.revision > current
        || cursor.id.is_empty()
        || DateTime::parse_from_rfc3339(&cursor.updated_at).is_err()
    {
        return Err(invalid());
    }
    Ok(Some(cursor))
}

fn issue_page(
    mut rows: Vec<Value>,
    args: &Value,
    after: Option<&IssueCursor>,
    revision: i64,
    filter: String,
) -> Res<Value> {
    // Every stored updated_at is canonical UTC with milliseconds. Equal instants use an ascending ID.
    let order = |a: &Value, b: &Value| {
        b["updated_at"]
            .as_str()
            .cmp(&a["updated_at"].as_str())
            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    };
    if let Some(c) = after {
        let last = json!({"updated_at":c.updated_at,"id":c.id});
        rows.retain(|row| order(row, &last).is_gt());
    }
    rows.sort_unstable_by(order);
    let limit = args["limit"].as_u64().unwrap_or(50).clamp(1, 100) as usize;
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    let cursor = if has_more {
        let last = rows.last().expect("positive limit");
        Some(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&IssueCursor {
            revision,
            updated_at: req_str(last, "updated_at")?.to_owned(),
            id: req_str(last, "id")?.to_owned(),
            filter,
        })?))
    } else {
        None
    };
    let mut result = json!({"items":rows});
    if let Some(c) = cursor {
        result["next_cursor"] = json!(c);
    }
    Ok(result)
}

async fn issue_list(tx: &Tx<'_>, args: &Value) -> Res<Value> {
    let (search, state, signature) = issue_filter(args);
    let cursor = issue_cursor(args, tx.revision, &signature)?;
    let cut = cursor.as_ref().map(|c| c.revision).unwrap_or(tx.revision);
    let mut records = tx.list("issue").await?;
    // If another command ran between pages, restore each edited record to the revision
    // visible on the first page. A newly created message has no earlier version.
    if cut < tx.revision {
        let mut snapshot = Vec::with_capacity(records.len());
        for mut record in records {
            let id = req_str(&record, "id")?.to_owned();
            loop {
                if record["store_revision"].as_i64().unwrap_or(0) <= cut {
                    record["id"] = json!(id);
                    snapshot.push(record);
                    break;
                }
                let version = record["version"].as_u64().unwrap_or(1);
                if version <= 1 {
                    break;
                }
                record = tx
                    .require("issue_revision", &revision_key(&id, version - 1))
                    .await?;
            }
        }
        records = snapshot;
    }
    let rows = records
        .into_iter()
        .filter(|v| {
            state.is_none_or(|s| v["state"] == s)
                && ["title", "subject"].iter().any(|k| {
                    v["draft"][k]
                        .as_str()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&search)
                })
        })
        .map(|r| row(&r))
        .collect();
    issue_page(rows, args, cursor.as_ref(), cut, signature)
}

fn revision_key(id: &str, version: u64) -> String {
    format!("{id}:{version:016}")
}

fn scrub(input: &str, template: bool) -> String {
    let mut cleaner = Builder::default();
    let allowed: HashSet<_> = [
        "a",
        "abbr",
        "b",
        "blockquote",
        "br",
        "code",
        "div",
        "em",
        "figure",
        "figcaption",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "hr",
        "i",
        "img",
        "li",
        "ol",
        "p",
        "pre",
        "s",
        "small",
        "span",
        "strong",
        "sub",
        "sup",
        "table",
        "tbody",
        "td",
        "th",
        "thead",
        "tr",
        "u",
        "ul",
    ]
    .into_iter()
    .collect();
    cleaner
        .tags(allowed)
        .generic_attributes(HashSet::new())
        .url_relative(UrlRelative::Deny)
        .url_schemes(["http", "https", "mailto"].into_iter().collect())
        .link_rel(Some("noopener noreferrer"))
        .add_tag_attributes("a", &["data-purpose"])
        .attribute_filter(|tag, name, value| {
            if tag == "a" && name == "data-purpose" && !matches!(value, "editorial" | "sponsor") {
                return None;
            }
            if (tag == "img" && name == "src") || (tag == "a" && name == "href") {
                let url = Url::parse(value).ok()?;
                if tag == "a" && url.scheme() == "mailto" {
                    return Some(Cow::Borrowed(value));
                }
                if !matches!(url.scheme(), "https" | "http")
                    || url.host().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                {
                    return None;
                }
            }
            Some(Cow::Borrowed(value))
        });
    if template {
        cleaner.add_tags(&[SLOT_TAG]);
    }
    cleaner.clean(input).to_string()
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn as_plain(html: &str) -> Res<String> {
    let text = html2text::from_read(html.as_bytes(), 92)
        .map_err(|_| AppError::invalid("Could not derive plain text from the message."))?;
    if text.trim().is_empty() {
        return Err(AppError::invalid(
            "Provide a readable plain-text alternative.",
        ));
    }
    Ok(text.trim().to_string())
}

fn bounded(v: &Value, key: &str, max_chars: usize, empty: bool) -> Res<String> {
    let value = req_str_allow_empty(v, key)?;
    if (!empty && value.trim().is_empty()) || value.chars().count() > max_chars {
        return Err(AppError::invalid(format!("Invalid length for {key}.")));
    }
    Ok(value.to_owned())
}
fn req_str_allow_empty<'a>(v: &'a Value, key: &str) -> Res<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::invalid(format!("Expected text for {key}.")))
}
fn required_header(v: &Value, key: &str, max: usize) -> Res<String> {
    let value = bounded(v, key, max, false)?;
    if value.contains(['\r', '\n', '\0']) {
        return Err(AppError::invalid(format!(
            "Invalid control character in {key}."
        )));
    }
    Ok(value)
}
fn valid_public_url(text: &str) -> Res<Url> {
    let url = Url::parse(text).map_err(|_| AppError::invalid("Expected an absolute HTTPS URL."))?;
    if !matches!(url.scheme(), "https" | "http")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(AppError::invalid(
            "Expected a URL without embedded credentials.",
        ));
    }
    Ok(url)
}

fn changed_fields_conflict() -> AppError {
    AppError::new(
        "revision_conflict",
        "A different revision exists. Read it and supply its current version.",
        409,
    )
}

pub async fn settings(tx: &Tx<'_>) -> Res<Value> {
    Ok(settings_entry(tx).await?["settings"].clone())
}
async fn settings_entry(tx: &Tx<'_>) -> Res<Value> {
    if let Some(v) = tx.get("settings", "publication").await? {
        return Ok(v);
    }
    let host = conf(tx.env, "PUBLICATION_ORIGIN")
        .or_else(|| conf(tx.env, "PUBLIC_ORIGIN"))
        .unwrap_or_else(|| "https://publication.example".into());
    let from = conf(tx.env, "FROM_ADDRESS").unwrap_or_else(|| "sender@publication.example".into());
    let reply = conf(tx.env, "REPLY_TO").unwrap_or_else(|| from.clone());
    let time_zone = conf(tx.env, "TIME_ZONE").unwrap_or_else(|| "UTC".into());
    if time_zone.parse::<Tz>().is_err() {
        return Err(AppError::invalid("TIME_ZONE must be a valid IANA name."));
    }
    let archive_days = conf(tx.env, "ARCHIVE_LAG_DAYS")
        .map(|s| {
            s.parse::<u16>()
                .ok()
                .filter(|days| *days <= 365)
                .ok_or_else(|| {
                    AppError::invalid(
                        "ARCHIVE_LAG_DAYS must be a whole number between zero and 365.",
                    )
                })
        })
        .transpose()?
        .unwrap_or(0);
    Ok(json!({"id":"publication","version":1,"settings":{
        "publication_name":conf(tx.env,"PUBLICATION_NAME").unwrap_or_else(||"newsman".into()),
        "time_zone":time_zone,"public_origin":host,"from_address":from,"reply_to":reply,
        "postal_footer":"","consent_version":"v1","archive_lag_days":archive_days,
        "click_tracking":false,"test_recipients":[],"send_budget":0}}))
}

fn check_settings(value: &Value, local_deployment: bool) -> Res<()> {
    let url = valid_public_url(req_str(value, "public_origin")?)?;
    if (url.scheme() != "https"
        && (!local_deployment
            || !matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))))
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AppError::invalid(
            "The public origin must be a site origin with HTTPS in production.",
        ));
    }
    let zone = req_str(value, "time_zone")?;
    zone.parse::<Tz>()
        .map_err(|_| AppError::invalid("Choose a valid IANA time zone."))?;
    for key in ["from_address", "reply_to"] {
        if crate::audience::email_key(&required_header(value, key, 254)?).is_none() {
            return Err(AppError::invalid(format!("Invalid {key}.")));
        }
    }
    if value["test_recipients"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|email| {
            email
                .as_str()
                .and_then(crate::audience::email_key)
                .is_none()
        })
    {
        return Err(AppError::invalid("Use valid test recipient addresses."));
    }
    Ok(())
}

fn prepare_template(a: &Value, id: String, version: u64) -> Res<Value> {
    let raw = req_str(a, "html")?;
    let plain = req_str(a, "plain_text")?;
    if raw.matches(BODY_SLOT).count() != 1
        || plain.matches(BODY_SLOT).count() != 1
        || raw.to_ascii_lowercase().contains(SLOT_TAG)
    {
        return Err(AppError::invalid(
            "Place exactly one {{body}} slot in the HTML and text template.",
        ));
    }
    let cleaned = scrub(&raw.replace(BODY_SLOT, SLOT_ELEMENT), true);
    if cleaned.matches(SLOT_ELEMENT).count() != 1 {
        return Err(AppError::invalid(
            "The HTML {{body}} slot belongs between elements.",
        ));
    }
    let normalized = cleaned.replace(SLOT_ELEMENT, BODY_SLOT);
    for t in [&*normalized, plain] {
        let remainder = t
            .replace(BODY_SLOT, "")
            .replace("{{publication_name}}", "")
            .replace("{{title}}", "");
        if remainder.contains("{{") || remainder.contains("}}") {
            return Err(AppError::invalid("The template contains an unknown field."));
        }
    }
    Ok(
        json!({"id":id,"version":version,"name":required_header(a,"name",160)?,"html":normalized,"plain_text":plain}),
    )
}

fn source_metadata(source: &Value) -> Res<Value> {
    let mut out = json!({
        "external_key":bounded(source,"external_key",128,false)?, "title":required_header(source,"title",160)?,
        "summary":bounded(source,"summary",500,true)?, "html":scrub(&bounded(source,"html",262144,false)?,false)
    });
    if let Some(url) = source.get("canonical_url") {
        valid_public_url(
            url.as_str()
                .ok_or_else(|| AppError::invalid("Invalid canonical URL."))?,
        )?;
        out["canonical_url"] = url.clone();
    }
    for key in ["edition_at", "archive_at"] {
        if let Some(v) = source.get(key) {
            let at = DateTime::parse_from_rfc3339(v.as_str().unwrap_or(""))
                .map_err(|_| AppError::invalid(format!("Invalid {key}.")))?;
            out[key] = json!(at.to_utc().to_rfc3339());
        }
    }
    for key in ["hero_asset_id", "hero_alt"] {
        if source.get(key).is_some() {
            out[key] = json!(bounded(
                source,
                key,
                if key == "hero_alt" { 500 } else { 128 },
                false
            )?);
        }
    }
    Ok(out)
}

fn normalize_draft(draft: &Value) -> Res<Value> {
    let html = scrub(&bounded(draft, "html", 262144, false)?, false);
    if html.trim().is_empty() {
        return Err(AppError::invalid(
            "The message contains no supported email markup.",
        ));
    }
    let plain = match draft.get("plain_text").and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => bounded(draft, "plain_text", 131072, false)?,
        _ => as_plain(&html)?,
    };
    let mut value = json!({"title":required_header(draft,"title",160)?,"subject":required_header(draft,"subject",180)?,
        "preheader":bounded(draft,"preheader",500,true)?,"html":html,"plain_text":plain});
    for key in ["template_id", "hero_asset_id", "hero_alt"] {
        if draft.get(key).is_some() {
            value[key] = json!(bounded(
                draft,
                key,
                if key == "hero_alt" { 500 } else { 128 },
                false
            )?);
        }
    }
    if let Some(source) = draft.get("source") {
        value["source"] = source_metadata(source)?;
    }
    if value.get("hero_asset_id").is_some()
        && value["hero_alt"]
            .as_str()
            .is_none_or(|s| s.trim().is_empty())
    {
        return Err(AppError::invalid(
            "Provide alternative text for the message image.",
        ));
    }
    if value.to_string().len() > MAX_SNAPSHOT {
        return Err(AppError::invalid(
            "Message content exceeds the 512 KiB revision limit.",
        ));
    }
    Ok(value)
}

fn substitutions(template: &str, title: &str, publication: &str, body: &str) -> String {
    // Substitute the template alone. User-supplied content can contain literal braces.
    let mut result = String::with_capacity(template.len() + body.len());
    let mut rest = template;
    while let Some(idx) = rest.find("{{") {
        result.push_str(&rest[..idx]);
        rest = &rest[idx..];
        if let Some(next) = rest.strip_prefix(BODY_SLOT) {
            result.push_str(body);
            rest = next;
        } else if let Some(next) = rest.strip_prefix("{{title}}") {
            result.push_str(title);
            rest = next;
        } else if let Some(next) = rest.strip_prefix("{{publication_name}}") {
            result.push_str(publication);
            rest = next;
        } else {
            result.push_str("{{");
            rest = &rest[2..];
        }
    }
    result.push_str(rest);
    result
}

fn media_mac(secret: &str, id: &str, digest: &str) -> Hmac<Sha256> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(b"newsman/media/v1\0");
    mac.update(id.as_bytes());
    mac.update(b"\0");
    mac.update(digest.as_bytes());
    mac
}

fn message_public_origin(env: &Env, configured: &Value) -> Res<String> {
    let url = conf(env, "PUBLIC_ORIGIN").unwrap_or_else(|| {
        configured["public_origin"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    });
    Ok(valid_public_url(&url)?.origin().ascii_serialization())
}

// A deployment delay applies only when the publisher has already supplied both a web
// destination and an edition time. An explicit release time always wins.
fn apply_archive_delay(draft: &mut Value, days: u64) -> Res<()> {
    let Some(source) = draft.get_mut("source") else {
        return Ok(());
    };
    let Some(edition) = source["edition_at"].as_str() else {
        return Ok(());
    };
    if days == 0 || source.get("canonical_url").is_none() || source.get("archive_at").is_some() {
        return Ok(());
    }
    let release = DateTime::parse_from_rfc3339(edition)
        .ok()
        .and_then(|t| t.checked_add_days(chrono::Days::new(days)))
        .ok_or_else(|| AppError::invalid("The optional archive release time is out of range."))?;
    source["archive_at"] = json!(release.to_utc().to_rfc3339());
    Ok(())
}

async fn save_revision(
    tx: &mut Tx<'_>,
    id: &str,
    version: u64,
    draft: &Value,
    previous: Option<&Value>,
) -> Res<Value> {
    let mut draft = normalize_draft(draft)?;
    if draft.get("source").is_some() {
        let prefs = settings(tx).await?;
        apply_archive_delay(&mut draft, prefs["archive_lag_days"].as_u64().unwrap_or(0))?;
    }
    let mut record =
        json!({"id":id,"version":version,"draft":draft,"state":"draft","updated_at":now()});
    if let Some(template_id) = draft["template_id"].as_str() {
        record["template_snapshot"] = tx.require("template", template_id).await?;
    }
    if let Some(asset_id) = draft["hero_asset_id"].as_str() {
        // A previously attached image is part of the publication draft. A second editor
        // can retain it without gaining access to the first editor's private uploads.
        let pinned = previous
            .filter(|p| p["draft"]["hero_asset_id"] == asset_id)
            .and_then(|p| p["hero_digest"].as_str());
        record["hero_digest"] = match pinned {
            Some(digest) => json!(digest),
            None => checked_file(tx, asset_id, "hero").await?["sha256"].clone(),
        };
    }
    for key in ["external_key", "source_digest"] {
        if let Some(v) = previous.and_then(|p| p.get(key)) {
            record[key] = v.clone();
        }
    }
    if let Some(source) = draft["source"]["external_key"].as_str() {
        if previous
            .and_then(|v| v["external_key"].as_str())
            .is_some_and(|key| key != source)
        {
            return Err(AppError::invalid(
                "An established source key cannot be reassigned.",
            ));
        }
        let mapping_id = sha256(source.as_bytes());
        if tx
            .get("issue_source", &mapping_id)
            .await?
            .is_some_and(|m| m["issue_id"] != id)
        {
            return Err(changed_fields_conflict());
        }
        tx.put(
            "issue_source",
            &mapping_id,
            json!({"id":mapping_id,"issue_id":id}),
        )?;
        record["external_key"] = json!(source);
    }
    let digest = sha256(&serde_json::to_vec(&json!([
        &record["draft"],
        &record["template_snapshot"],
        &record["hero_digest"]
    ]))?);
    record["digest"] = json!(digest);
    write_issue(tx, &record)?;
    Ok(record)
}

fn write_issue(tx: &mut Tx<'_>, record: &Value) -> Res<()> {
    let id = req_str(record, "id")?;
    let version = record["version"].as_u64().unwrap_or(1);
    let mut revision = record.clone();
    revision["store_revision"] = json!(tx.revision + 1);
    tx.put("issue", id, revision.clone())?;
    let history_id = revision_key(id, version);
    revision["id"] = json!(history_id);
    revision["issue_id"] = json!(id);
    tx.put("issue_revision", &history_id, revision)
}

pub async fn render(tx: &Tx<'_>, id: &str, version: u64) -> Res<Value> {
    let issue = tx
        .require("issue_revision", &revision_key(id, version))
        .await?;
    let prefs = settings(tx).await?;
    let hero = if let (Some(asset), Some(digest)) = (
        issue["draft"]["hero_asset_id"].as_str(),
        issue["hero_digest"].as_str(),
    ) {
        let cap = URL_SAFE_NO_PAD.encode(
            media_mac(&crate::audience::proof_secret(tx.env)?, asset, digest)
                .finalize()
                .into_bytes(),
        );
        Some(format!(
            "{}/media/{asset}/{cap}",
            message_public_origin(tx.env, &prefs)?
        ))
    } else {
        None
    };
    render_snapshot(&issue, &prefs, hero.as_deref())
}

fn render_snapshot(issue: &Value, prefs: &Value, hero: Option<&str>) -> Res<Value> {
    let draft = &issue["draft"];
    let title = req_str(draft, "title")?;
    let publication = req_str(prefs, "publication_name")?;
    let mut html = req_str(draft, "html")?.to_owned();
    let mut plain = req_str(draft, "plain_text")?.to_owned();
    if let Some(src) = hero {
        html = format!(
            "<figure><img src=\"{}\" alt=\"{}\"></figure>{html}",
            escape(src),
            escape(req_str(draft, "hero_alt")?)
        );
    }
    if let Some(template) = issue.get("template_snapshot") {
        html = scrub(
            &substitutions(
                req_str(template, "html")?,
                &escape(title),
                &escape(publication),
                &html,
            ),
            false,
        );
        plain = substitutions(req_str(template, "plain_text")?, title, publication, &plain);
    }
    let preheader = draft["preheader"].as_str().unwrap_or("");
    if !preheader.is_empty() {
        html = format!(
            "<div style=\"display:none;max-height:0;overflow:hidden;mso-hide:all\" aria-hidden=\"true\">{}</div>{html}",
            escape(preheader)
        );
    }
    let mut warnings = vec![];
    let footer = prefs["postal_footer"].as_str().unwrap_or("");
    if footer.trim().is_empty() {
        warnings.push("Add a postal footer before live dispatch.");
    } else {
        html.push_str(&format!("<hr><p>{}</p>", escape(footer)));
        plain.push_str(&format!("\n\n{footer}"));
    }
    if html.chars().count() > 262144
        || plain.chars().count() > 131072
        || html.len() + plain.len() > MAX_SNAPSHOT
    {
        return Err(AppError::invalid(
            "Rendered message exceeds its size limit.",
        ));
    }
    let mut view = json!({"subject":draft["subject"],"from_address":prefs["from_address"],"reply_to":prefs["reply_to"],
        "preheader":preheader,"html":html,"plain_text":plain,"warnings":warnings});
    let digest = sha256(&serde_json::to_vec(&view)?);
    view["digest"] = json!(digest);
    Ok(view)
}

pub async fn media(env: &Env, id: &str, token: &str) -> Res<Response> {
    let missing = || AppError::new("not_found", "Image not found.", 404);
    let tx = Tx::begin(env, Actor::system()).await?;
    let asset = tx.get("file", id).await?.ok_or_else(missing)?;
    if asset["kind"] != "hero" || asset["state"] != "complete" {
        return Err(missing());
    }
    let bytes = URL_SAFE_NO_PAD.decode(token).map_err(|_| missing())?;
    let mac = media_mac(
        &crate::audience::proof_secret(env)?,
        id,
        asset["sha256"].as_str().unwrap_or(""),
    );
    mac.verify_slice(&bytes).map_err(|_| missing())?;
    let mime = asset["mime"].as_str().unwrap_or("");
    if !matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    ) {
        return Err(missing());
    }
    let key = req_str(&asset, "object_key")?;
    let object = env
        .bucket("FILES")?
        .get(key)
        .execute()
        .await?
        .ok_or_else(missing)?;
    let mut response = Response::from_stream(object.body().ok_or_else(missing)?.stream()?)?;
    response.headers_mut().set("Content-Type", mime)?;
    response
        .headers_mut()
        .set("Cache-Control", "public, max-age=31536000, immutable")?;
    response
        .headers_mut()
        .set("X-Content-Type-Options", "nosniff")?;
    Ok(response)
}

async fn import_issue(tx: &mut Tx<'_>, a: &Value) -> Res<Value> {
    let bytes = read_asset(tx, req_str(a, "asset_id")?, "issue_bundle", MAX_SNAPSHOT).await?;
    let digest = sha256(&bytes);
    if digest != req_str(a, "source_digest")? {
        return Err(AppError::invalid(
            "The staged bundle digest does not match.",
        ));
    }
    let bundle: Value = serde_json::from_slice(&bytes)
        .map_err(|_| AppError::invalid("The issue bundle must be valid JSON."))?;
    if bundle["format_version"] != 1 {
        return Err(AppError::invalid(
            "Unsupported issue bundle format_version.",
        ));
    }
    let source = source_metadata(&bundle["source"])?;
    let external_key = req_str(a, "external_key")?;
    if source["external_key"] != external_key {
        return Err(AppError::invalid(
            "The external key differs from the bundle.",
        ));
    }
    let map_key = sha256(external_key.as_bytes());
    let prior = match tx.get("issue_source", &map_key).await? {
        Some(mapping) => Some(tx.require("issue", req_str(&mapping, "issue_id")?).await?),
        None => None,
    };
    if let Some(record) = &prior {
        if record["source_digest"] == digest {
            return Ok(public_issue(record));
        }
        if a["expected_version"].is_null() {
            return Err(changed_fields_conflict());
        }
        expect_version(record, a)?;
    } else if a.get("expected_version").is_some() {
        return Err(changed_fields_conflict());
    }
    let id = prior
        .as_ref()
        .and_then(|p| p["id"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| uid("issue"));
    let version = prior
        .as_ref()
        .and_then(|p| p["version"].as_u64())
        .unwrap_or(0)
        + 1;
    let fields = bundle.get("email").or_else(|| bundle.get("overrides"));
    let mut draft = json!({"source":source,"title":source["title"],"subject":source["title"],
        "preheader":source["summary"],"html":source["html"],"plain_text":""});
    for key in ["hero_asset_id", "hero_alt"] {
        if let Some(v) = source.get(key) {
            draft[key] = v.clone();
        }
    }
    for key in ["subject", "preheader", "plain_text"] {
        if let Some(v) = fields.and_then(|f| f.get(key)) {
            draft[key] = v.clone();
        }
    }
    let mut record = save_revision(tx, &id, version, &draft, prior.as_ref()).await?;
    record["source_digest"] = json!(digest);
    write_issue(tx, &record)?;

    Ok(public_issue(&record))
}

fn feed_has_canonical(xml: &str, canonical: &Url, base: &Url) -> bool {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return false;
    };
    doc.descendants()
        .filter(|n| n.is_element() && matches!(n.tag_name().name(), "entry" | "item"))
        .flat_map(|n| n.children())
        .filter(|e| e.is_element())
        .any(|node| {
            let value = match node.tag_name().name() {
                "id" | "guid" => node.text(),
                "link" if node.attribute("rel").is_none_or(|rel| rel == "alternate") => {
                    node.attribute("href").or_else(|| node.text())
                }
                _ => None,
            };
            value
                .and_then(|val| base.join(val.trim()).ok())
                .is_some_and(|u| u == *canonical)
        })
}

async fn verify_request(url: &Url) -> Res<Response> {
    let mut init = RequestInit::new();
    init.with_method(Method::Get)
        .with_redirect(RequestRedirect::Manual);
    let request = Request::new_with_init(url.as_str(), &init)?;
    Ok(Fetch::Request(request).send().await?)
}

async fn bounded_text(response: &mut Response, max: usize) -> Res<String> {
    let mut bytes = Vec::new();
    let mut stream = response.stream()?;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if bytes.len() + chunk.len() > max {
            return Err(AppError::invalid(
                "Archive feed exceeds the verification size limit.",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| AppError::invalid("The feed must be UTF-8."))
}

async fn verify_archive(tx: &mut Tx<'_>, a: &Value) -> Res<Value> {
    let id = req_str(a, "id")?;
    let mut issue = tx.require("issue", id).await?;
    expect_version(&issue, a)?;
    let prefs = settings(tx).await?;
    let origin = valid_public_url(req_str(&prefs, "public_origin")?)?;
    let source = &issue["draft"]["source"];
    let url = source["canonical_url"].as_str().ok_or_else(|| {
        AppError::invalid("Configure an archive URL on this message before verifying it.")
    })?;
    let url = valid_public_url(url)?;
    if url.origin() != origin.origin() || url.fragment().is_some() || url.scheme() != "https" {
        return Err(AppError::invalid(
            "The archive must use the configured public HTTPS origin.",
        ));
    }
    if let Some(t) = source["archive_at"].as_str() {
        let current = DateTime::parse_from_rfc3339(&now())
            .map_err(|_| AppError::invalid("Clock unavailable."))?;
        let release = DateTime::parse_from_rfc3339(t)
            .map_err(|_| AppError::invalid("Invalid archive time."))?;
        if release > current {
            return Err(AppError::invalid(
                "The requested public archive time is still in the future.",
            ));
        }
    }
    let response = verify_request(&url).await?;
    if !(200..300).contains(&response.status_code())
        || !response
            .headers()
            .get("content-type")?
            .unwrap_or_default()
            .to_ascii_lowercase()
            .starts_with("text/html")
    {
        return Err(AppError::invalid(
            "The canonical URL must serve a successful HTML response without redirecting.",
        ));
    }
    if let Some(feed) = conf(tx.env, "ARCHIVE_FEED_URL") {
        let feed = valid_public_url(&feed)?;
        if feed.origin() != origin.origin() {
            return Err(AppError::invalid(
                "Configure a feed on the same public origin.",
            ));
        }
        let mut response = verify_request(&feed).await?;
        if !(200..300).contains(&response.status_code()) {
            return Err(AppError::invalid("The configured feed is unavailable."));
        }
        if !feed_has_canonical(&bounded_text(&mut response, 1_048_576).await?, &url, &feed) {
            return Err(AppError::invalid(
                "The configured feed has no entry for the canonical URL.",
            ));
        }
    }
    let at = now();
    issue["version"] = json!(issue["version"].as_u64().unwrap_or(0) + 1);
    issue["archive_verified_at"] = json!(at);
    issue["updated_at"] = json!(at);
    write_issue(tx, &issue)?;
    let result =
        serde_json::to_vec(&json!({"canonical_url":url.as_str(),"verified_at":at,"issue_id":id}))?;
    result_job(
        tx,
        "archive_verify",
        "archive_receipt",
        result,
        vec![],
        "write",
    )
    .await
}

pub async fn run(tx: &mut Tx<'_>, name: &str, a: &Value) -> Res<Value> {
    match name {
        "settings_get" => Ok(select(&settings_entry(tx).await?, &["version", "settings"])),
        "settings_update" => {
            let entry = settings_entry(tx).await?;
            expect_version(&entry, a)?;
            check_settings(&a["settings"], local(tx.env))?;
            let version = entry["version"].as_u64().unwrap_or(1) + 1;
            tx.put(
                "settings",
                "publication",
                json!({"id":"publication","version":version,"settings":a["settings"]}),
            )?;
            Ok(json!({"id":"publication","version":version}))
        }
        "templates_list" => page_values(
            tx.list("template")
                .await?
                .iter()
                .map(|t| select(t, &["id", "version", "name"]))
                .collect(),
            a,
        ),
        "template_get" => tx.require("template", req_str(a, "id")?).await,
        "template_create" | "template_update" => {
            let (id, version) = if name == "template_update" {
                let id = req_str(a, "id")?;
                let old = tx.require("template", id).await?;
                expect_version(&old, a)?;
                (id.to_owned(), old["version"].as_u64().unwrap_or(0) + 1)
            } else {
                (uid("template"), 1)
            };
            let template = prepare_template(a, id.clone(), version)?;
            tx.put("template", &id, template.clone())?;
            Ok(template)
        }
        "issues_list" => issue_list(tx, a).await,
        "issue_get" => Ok(public_issue(&tx.require("issue", req_str(a, "id")?).await?)),
        "issue_create" => {
            let id = uid("issue");
            Ok(public_issue(
                &save_revision(tx, &id, 1, &a["draft"], None).await?,
            ))
        }
        "issue_update" | "issue_restore" => {
            let id = req_str(a, "id")?;
            let old = tx.require("issue", id).await?;
            expect_version(&old, a)?;
            let version = old["version"].as_u64().unwrap_or(0) + 1;
            if name == "issue_restore" {
                let rev = a["restore_version"]
                    .as_u64()
                    .ok_or_else(|| AppError::invalid("Invalid revision."))?;
                let mut previous = tx.require("issue_revision", &revision_key(id, rev)).await?;
                previous["id"] = json!(id);
                previous["version"] = json!(version);
                previous["state"] = json!("draft");
                previous["updated_at"] = json!(now());
                if let Some(o) = previous.as_object_mut() {
                    o.remove("issue_id");
                    o.remove("archive_verified_at");
                }
                write_issue(tx, &previous)?;
                return Ok(public_issue(&previous));
            }
            Ok(public_issue(
                &save_revision(tx, id, version, &a["draft"], Some(&old)).await?,
            ))
        }
        "issue_import" => import_issue(tx, a).await,
        "issue_versions" => {
            let id = req_str(a, "id")?;
            tx.require("issue", id).await?;
            let versions = tx
                .list("issue_revision")
                .await?
                .into_iter()
                .filter(|v| v["issue_id"] == id)
                .map(|v| row(&v))
                .collect();
            let mut page = page_values(versions, a)?;
            if let Some(items) = page["items"].as_array_mut() {
                for value in items {
                    value["id"] = json!(id);
                }
            }
            Ok(page)
        }
        "issue_version_get" | "issue_render" => {
            let id = req_str(a, "id")?;
            let version = a["version"]
                .as_u64()
                .ok_or_else(|| AppError::invalid("Invalid revision."))?;
            if name == "issue_render" {
                render(tx, id, version).await
            } else {
                Ok(public_issue(
                    &tx.require("issue_revision", &revision_key(id, version))
                        .await?,
                ))
            }
        }
        "issue_archive" => {
            let id = req_str(a, "id")?;
            let mut old = tx.require("issue", id).await?;
            expect_version(&old, a)?;
            old["version"] = json!(old["version"].as_u64().unwrap_or(0) + 1);
            old["state"] = json!("archived");
            old["updated_at"] = json!(now());
            write_issue(tx, &old)?;
            Ok(select(&old, &["id", "version"]))
        }
        "archive_bundle" => {
            let id = req_str(a, "id")?;
            let version = a["version"]
                .as_u64()
                .ok_or_else(|| AppError::invalid("Invalid revision."))?;
            let issue = tx
                .require("issue_revision", &revision_key(id, version))
                .await?;
            let draft = &issue["draft"];
            let result = json!({"format_version":1,"issue_id":id,"version":version,"digest":issue["digest"],
                "title":draft["title"],"html":draft["html"],"plain_text":draft["plain_text"],
                "source":draft.get("source"),"hero_asset_id":draft.get("hero_asset_id"),"hero_alt":draft.get("hero_alt")});
            result_job(
                tx,
                "archive_bundle",
                "archive_bundle",
                serde_json::to_vec_pretty(&result)?,
                vec![],
                "write",
            )
            .await
        }
        "archive_verify" => verify_archive(tx, a).await,
        _ => Err(AppError::new(
            "not_found",
            "Unknown editorial operation.",
            404,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn markup_preserves_prose_without_active_content() {
        let content = scrub(
            "<h3>Hello</h3><script>noise</script><p onclick='bad()'>World</p><a href='/relative'>x</a><a href='https://example.org/post' data-purpose='sponsor'>s</a>",
            false,
        );
        assert!(content.contains("<h3>Hello</h3>") && content.contains("data-purpose=\"sponsor\""));
        assert!(
            !content.contains("noise")
                && !content.contains("onclick")
                && !content.contains("href=\"/relative")
        );
        assert!(!content.contains("<script"));
    }
    #[test]
    fn template_body_slot_is_only_allowed_in_text() {
        let a = json!({"name":"Frame","html":"<p title='{{body}}'>Hi</p>","plain_text":"{{body}}"});
        assert!(prepare_template(&a, "t".into(), 1).is_err());
        let ok =
            json!({"name":"Frame","html":"<h1>{{title}}</h1>{{body}}","plain_text":"{{body}}"});
        assert!(prepare_template(&ok, "t".into(), 1).is_ok());
        assert_eq!(
            substitutions("{{title}}/{{body}}", "{{body}}", "Studio", "Hello"),
            "{{body}}/Hello"
        );
    }
    #[test]
    fn frozen_render_is_deterministic_and_accepts_independent_messages() {
        let d = normalize_draft(
            &json!({"title":"Service notice","subject":"Service notice","preheader":"Status",
            "html":"<p>Resolved.</p>","plain_text":""}),
        )
        .unwrap();
        assert_eq!(d["plain_text"], "Resolved.");
        let issue = json!({"draft":d});
        let config = json!({"publication_name":"Service desk","from_address":"mail@example.org","reply_to":"desk@example.org","postal_footer":"12 Main"});
        let a = render_snapshot(&issue, &config, None).unwrap();
        assert_eq!(a, render_snapshot(&issue, &config, None).unwrap());
        assert!(a["plain_text"].as_str().unwrap().contains("12 Main"));
        assert!(a["html"].as_str().unwrap().contains("Status"));
    }
    #[test]
    fn proof_is_scoped_to_file_and_digest() {
        let token = media_mac("local", "f1", "d1").finalize().into_bytes();
        assert!(media_mac("local", "f1", "d1").verify_slice(&token).is_ok());
        assert!(media_mac("local", "f1", "d2").verify_slice(&token).is_err());
    }
    #[test]
    fn optional_archive_delay_preserves_publisher_choices() {
        let mut draft = json!({"source":{"edition_at":"2027-01-10T14:00:00Z"}});
        apply_archive_delay(&mut draft, 3).unwrap();
        assert!(draft["source"].get("archive_at").is_none());
        draft["source"]["canonical_url"] = json!("https://example.org/notice");
        apply_archive_delay(&mut draft, 0).unwrap();
        assert!(draft["source"].get("archive_at").is_none());
        apply_archive_delay(&mut draft, 3).unwrap();
        assert_eq!(draft["source"]["archive_at"], "2027-01-13T14:00:00+00:00");
        apply_archive_delay(&mut draft, 7).unwrap();
        assert_eq!(draft["source"]["archive_at"], "2027-01-13T14:00:00+00:00");
    }

    #[test]
    fn newest_messages_page_by_timestamp_and_id() {
        let args = json!({"limit":2});
        let (_, _, filter) = issue_filter(&args);
        let rows = vec![
            ("z", "2026-09-01"),
            ("d", "2026-09-02"),
            ("b", "2026-09-02"),
            ("a", "2026-08-30"),
        ]
        .into_iter()
        .map(|(id, at)| json!({"id":id,"updated_at":format!("{at}T10:00:00.000Z")}))
        .collect::<Vec<_>>();
        let page = issue_page(rows.clone(), &args, None, 12, filter.clone()).unwrap();
        assert_eq!(page["items"][0]["id"], "b");
        assert_eq!(page["items"][1]["id"], "d");
        let args = json!({"limit":2,"cursor":page["next_cursor"]});
        let cursor = issue_cursor(&args, 14, &filter).unwrap().unwrap();
        assert_eq!(cursor.revision, 12);
        assert!(issue_cursor(&args, 14, "other-search").is_err());
        let page = issue_page(rows, &args, Some(&cursor), cursor.revision, filter).unwrap();
        assert_eq!(page["items"][0]["id"], "z");
        assert_eq!(page["items"][1]["id"], "a");
        assert!(page.get("next_cursor").is_none());
    }

    #[test]
    fn archive_matches_entries_and_resolves_relative_links() {
        let url = Url::parse("https://example.org/message").unwrap();
        let feed = Url::parse("https://example.org/feed").unwrap();
        assert!(feed_has_canonical(
            "<feed><entry><link href='/message'/></entry></feed>",
            &url,
            &feed
        ));
        assert!(!feed_has_canonical(
            "<feed><link href='/message'/></feed>",
            &url,
            &feed
        ));
    }
}
