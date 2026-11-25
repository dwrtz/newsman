//! Self-hosted, script-free pages for readers opening a private email link.
use crate::audience::escape;
use crate::core::{Res, conf};
use serde_json::Value;
use worker::{Env, Response};

fn https(value: &str) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    (url.scheme() == "https"
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| escape(url.as_str()))
}

pub fn page(env: &Env, info: &Value, title: &str, lead: &str, action: &str) -> Res<Response> {
    let publication = info["publication_name"].as_str().unwrap_or("Newsletter");
    let wordmark = escape(&conf(env, "READER_WORDMARK").unwrap_or_else(|| publication.to_owned()));
    let masthead = match conf(env, "READER_SITE_URL").and_then(|s| https(&s)) {
        Some(url) => format!(r#"<a class="wordmark" href="{url}">{wordmark}</a>"#),
        None => format!(r#"<span class="wordmark">{wordmark}</span>"#),
    };
    let archive = info["archive_url"].as_str().and_then(https).map(|u| format!(
        r#"<a class="archive" href="{u}">Issue archive <span aria-hidden="true">↗</span></a>"#
    )).unwrap_or_default();
    let privacy = conf(env, "READER_PRIVACY_URL")
        .and_then(|s| https(&s))
        .map(|u| format!(r#"<a href="{u}">Privacy</a>"#))
        .unwrap_or_default();
    let illustration = conf(env,"READER_ART_ASSET")
        .filter(|s| s.starts_with("file_") && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        .map(|id| {
            let alt = escape(&conf(env,"READER_ART_ALT").unwrap_or_default());
            format!(r#"<figure><img src="/reader-art/{id}" alt="{alt}" width="1344" height="752"></figure>"#)
        }).unwrap_or_default();
    let pub_name = escape(publication);
    let title = escape(title);
    let lead = escape(lead);
    let mut response = Response::from_html(format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="robots" content="noindex,nofollow"><title>{title} · {pub_name}</title><link rel="stylesheet" href="/reader.css"></head>
<body><div class="sheet"><header>{masthead}{archive}</header><main><div class="copy"><h1>{title}</h1><p class="lead">{lead}</p>{action}</div>{illustration}</main>
<footer><span>{pub_name}</span>{privacy}</footer></div></body></html>"#
    ))?;
    let h = response.headers_mut();
    h.set("Cache-Control", "private, no-store")?;
    h.set("Referrer-Policy", "no-referrer")?;
    h.set("Content-Security-Policy", "default-src 'none'; style-src 'self'; font-src 'self'; img-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'")?;
    Ok(response)
}

pub fn confirmation_email(env: &Env, publication: &str, link: &str) -> String {
    let masthead = escape(&conf(env, "READER_WORDMARK").unwrap_or_else(|| publication.to_owned()));
    let publication = escape(publication);
    let link = escape(link);
    format!(
        r#"<!doctype html><html lang="en"><body style="margin:0;background:#fff;color:#161616">
<div style="max-width:540px;margin:0 auto;padding:32px 24px 44px">
<div style="font:32px Georgia,serif;letter-spacing:-1.5px;border-bottom:3px double #161616;padding:0 0 13px">{masthead}</div>
<h1 style="margin:32px 0 15px;font:normal 42px/1.1 Georgia,serif">Confirm your email.</h1>
<p style="font:16px/1.55 Arial,sans-serif">You signed up for {publication}. Open the link below, then choose “Confirm my email” to finish subscribing.</p>
<p style="margin:28px 0"><a href="{link}" style="display:inline-block;padding:14px 21px;background:#161616;color:#fff;text-decoration:none;font:bold 16px Arial,sans-serif">Review and confirm</a></p>
<p style="font:14px/1.55 Arial,sans-serif;color:#555">If someone else entered your address, you can ignore this email.</p></div></body></html>"#
    )
}
