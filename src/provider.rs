//! Amazon SES v2 and SNS. No implicit provider retries: the caller persists a submission fence.
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use rsa::{RsaPublicKey, pkcs1v15, pkcs8::DecodePublicKey, signature::Verifier};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use url::Url;
use x509_cert::{
    Certificate,
    der::{DecodePem, Encode},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mail {
    pub from: String,
    pub reply_to: String,
    pub to: String,
    pub subject: String,
    pub text: String,
    pub html: String,
    pub unsubscribe: Option<String>,
    pub attempt_id: String,
    pub campaign_id: Option<String>,
}

/// Format one SES sender mailbox. RFC 2047 encoded words are at most 75 bytes.
pub fn sender_mailbox(name: Option<&str>, address: &str) -> String {
    let mut name = name.unwrap_or_default().trim();
    if name.is_empty() || name.chars().any(char::is_control) {
        return address.to_owned();
    }
    if name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'-'))
    {
        return format!("{name} <{address}>");
    }
    let mut words = Vec::new();
    while !name.is_empty() {
        let mut end = name.len().min(45);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        words.push(format!(
            "=?UTF-8?B?{}?=",
            STANDARD.encode(&name.as_bytes()[..end])
        ));
        name = &name[end..];
    }
    format!("{} <{address}>", words.join(" "))
}

pub fn ses_payload(mail: &Mail, configuration_set: &str) -> Value {
    let mut simple = json!({"Subject":{"Data":mail.subject,"Charset":"UTF-8"},
        "Body":{"Text":{"Data":mail.text,"Charset":"UTF-8"},"Html":{"Data":mail.html,"Charset":"UTF-8"}}});
    if let Some(url) = &mail.unsubscribe {
        simple["Headers"] = json!([
            {"Name":"List-Unsubscribe","Value":format!("<{url}>")},
            {"Name":"List-Unsubscribe-Post","Value":"List-Unsubscribe=One-Click"}
        ]);
    }
    let mut tags = vec![json!({"Name":"newsman_attempt","Value":mail.attempt_id})];
    if let Some(campaign) = &mail.campaign_id {
        tags.push(json!({"Name":"newsman_campaign","Value":campaign}));
    }
    json!({"FromEmailAddress":mail.from, "Destination":{"ToAddresses":[mail.to]},
        "ReplyToAddresses":[mail.reply_to],"ConfigurationSetName":configuration_set,
        "EmailTags":tags,"Content":{"Simple":simple}})
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submission {
    Accepted(String),
    Throttled,
    Rejected,
    Unknown,
}

pub fn classify_response(status: u16, body: &str) -> Submission {
    match status {
        200..=299 => serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v["MessageId"].as_str().map(str::to_owned))
            .filter(|id| !id.is_empty())
            .map(Submission::Accepted)
            .unwrap_or(Submission::Unknown),
        429 => Submission::Throttled,
        400..=499 if status != 408 => Submission::Rejected,
        _ => Submission::Unknown,
    }
}

#[derive(Clone)]
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac(key: &[u8], input: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(input.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

pub(crate) fn regional_host(region: &str, service: &str) -> Option<String> {
    if region.is_empty()
        || region.len() > 32
        || !region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return None;
    }
    let suffix = if region.starts_with("cn-") {
        "amazonaws.com.cn"
    } else {
        "amazonaws.com"
    };
    Some(format!("{service}.{region}.{suffix}"))
}

/// SigV4 for newsman's fixed SES JSON and SNS Query API paths. Query strings are always empty.
pub fn sign(
    credentials: &Credentials,
    region: &str,
    service: &str,
    method: &str,
    host: &str,
    path: &str,
    content_type: &str,
    body: &str,
    now: &DateTime<Utc>,
) -> BTreeMap<String, String> {
    let date = now.format("%Y%m%d").to_string();
    let timestamp = now.format("%Y%m%dT%H%M%SZ").to_string();
    let body_hash = hex(&Sha256::digest(body.as_bytes()));
    let mut headers = BTreeMap::from([
        ("content-type".to_owned(), content_type.to_owned()),
        ("host".to_owned(), host.to_owned()),
        ("x-amz-content-sha256".to_owned(), body_hash.clone()),
        ("x-amz-date".to_owned(), timestamp.clone()),
    ]);
    if let Some(token) = &credentials.session_token {
        headers.insert("x-amz-security-token".to_owned(), token.clone());
    }
    let names = headers.keys().cloned().collect::<Vec<_>>().join(";");
    let canonical_headers = headers
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v.trim()))
        .collect::<String>();
    let request = format!("{method}\n{path}\n\n{canonical_headers}\n{names}\n{body_hash}");
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
        hex(&Sha256::digest(request.as_bytes()))
    );
    let date_key = hmac(format!("AWS4{}", credentials.secret_key).as_bytes(), &date);
    let region_key = hmac(&date_key, region);
    let service_key = hmac(&region_key, service);
    let signature = hex(&hmac(&hmac(&service_key, "aws4_request"), &to_sign));
    headers.insert(
        "authorization".to_owned(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={names}, Signature={signature}",
            credentials.access_key
        ),
    );
    headers
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeKind {
    Notification,
    Confirm,
    Unsubscribe,
}

pub struct Envelope {
    pub kind: EnvelopeKind,
    pub id: String,
    pub certificate: String,
    pub signature: Vec<u8>,
    pub signed_text: String,
    pub message: String,
    pub token: Option<String>,
    pub timestamp: DateTime<Utc>,
}

pub fn sns_envelope(v: &Value, expected_topic: &str, region: &str) -> Option<Envelope> {
    if expected_topic.is_empty()
        || v["TopicArn"].as_str()? != expected_topic
        || v["SignatureVersion"] != "2"
    {
        return None;
    }
    let kind = match v["Type"].as_str()? {
        "Notification" => EnvelopeKind::Notification,
        "SubscriptionConfirmation" => EnvelopeKind::Confirm,
        "UnsubscribeConfirmation" => EnvelopeKind::Unsubscribe,
        _ => return None,
    };
    let fields: &[&str] = if kind == EnvelopeKind::Notification {
        if v.get("Subject").is_some() {
            &[
                "Message",
                "MessageId",
                "Subject",
                "Timestamp",
                "TopicArn",
                "Type",
            ]
        } else {
            &["Message", "MessageId", "Timestamp", "TopicArn", "Type"]
        }
    } else {
        &[
            "Message",
            "MessageId",
            "SubscribeURL",
            "Timestamp",
            "Token",
            "TopicArn",
            "Type",
        ]
    };
    let mut signed_text = String::new();
    for field in fields {
        signed_text.push_str(field);
        signed_text.push('\n');
        signed_text.push_str(v[field].as_str()?);
        signed_text.push('\n');
    }
    let cert = Url::parse(v["SigningCertURL"].as_str()?).ok()?;
    let expected_host = regional_host(region, "sns")?;
    if cert.scheme() != "https"
        || cert.host_str()? != expected_host
        || !cert.username().is_empty()
        || cert.password().is_some()
        || cert.port().is_some()
        || cert.query().is_some()
        || cert.fragment().is_some()
    {
        return None;
    }
    let id = cert
        .path()
        .strip_prefix("/SimpleNotificationService-")?
        .strip_suffix(".pem")?;
    if !(16..=128).contains(&id.len())
        || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return None;
    }
    let id = v["MessageId"].as_str()?;
    if id.is_empty() || id.len() > 128 {
        return None;
    }
    Some(Envelope {
        kind,
        id: id.to_owned(),
        certificate: cert.to_string(),
        signature: STANDARD.decode(v["Signature"].as_str()?).ok()?,
        signed_text,
        message: v["Message"].as_str()?.to_owned(),
        token: v["Token"].as_str().map(str::to_owned),
        timestamp: DateTime::parse_from_rfc3339(v["Timestamp"].as_str()?)
            .ok()?
            .to_utc(),
    })
}

/// Only accept a certificate downloaded by the Worker directly from the validated SNS service URL.
/// TLS establishes the AWS certificate distribution endpoint as the source of this public key.
pub fn verify_certificate(envelope: &Envelope, trusted_pem: &str) -> bool {
    let Ok(cert) = Certificate::from_pem(trusted_pem) else {
        return false;
    };
    let valid = &cert.tbs_certificate.validity;
    let signed_at = envelope.timestamp.timestamp();
    if signed_at < valid.not_before.to_unix_duration().as_secs() as i64
        || signed_at > valid.not_after.to_unix_duration().as_secs() as i64
    {
        return false;
    }
    let Ok(der) = cert.tbs_certificate.subject_public_key_info.to_der() else {
        return false;
    };
    let Ok(key) = RsaPublicKey::from_public_key_der(&der) else {
        return false;
    };
    let Ok(signature) = pkcs1v15::Signature::try_from(envelope.signature.as_slice()) else {
        return false;
    };
    pkcs1v15::VerifyingKey::<Sha256>::new(key)
        .verify(envelope.signed_text.as_bytes(), &signature)
        .is_ok()
}

#[derive(Debug)]
pub struct Feedback {
    pub kind: String,
    pub attempt_id: Option<String>,
    pub message_id: String,
    pub addresses: Vec<String>,
    pub at: String,
}

pub fn parse_feedback(message: &str, configuration_set: &str) -> Option<Feedback> {
    let event: Value = serde_json::from_str(message).ok()?;
    let mail = &event["mail"];
    if mail["tags"]["ses:configuration-set"].get(0)?.as_str()? != configuration_set {
        return None;
    }
    let event_type = event
        .get("eventType")
        .or_else(|| event.get("notificationType"))?
        .as_str()?;
    let (kind, recipients) = match event_type.to_ascii_lowercase().as_str() {
        "bounce" if event["bounce"]["bounceType"] == "Permanent" => (
            "hard_bounce",
            event["bounce"]["bouncedRecipients"].as_array(),
        ),
        "bounce" => ("transient_bounce", None),
        "complaint" => (
            "complaint",
            event["complaint"]["complainedRecipients"].as_array(),
        ),
        "delivery" => ("delivered", None),
        "reject" | "rendering failure" => ("rejected", None),
        "send" => ("accepted", None),
        "deliverydelay" => ("delayed", None),
        _ => return None,
    };
    let addresses = recipients
        .into_iter()
        .flatten()
        .filter_map(|r| r["emailAddress"].as_str().map(str::to_owned))
        .collect();
    let occurred = event
        .get(event_type.to_ascii_lowercase())
        .and_then(|v| v["timestamp"].as_str())
        .or_else(|| mail["timestamp"].as_str())?;
    let at = DateTime::parse_from_rfc3339(occurred)
        .ok()?
        .to_utc()
        .to_rfc3339();
    Some(Feedback {
        at,
        kind: kind.to_owned(),
        addresses,
        message_id: mail["messageId"].as_str()?.to_owned(),
        attempt_id: mail["tags"]["newsman_attempt"]
            .get(0)
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

use crate::core::{Actor, AppError, Res, Tx, conf, local, now};
use worker::{Env, Fetch, Headers, Method, Request, RequestInit, RequestRedirect, Response};

fn required(env: &Env, key: &str) -> Res<String> {
    conf(env, key).filter(|s| !s.is_empty()).ok_or_else(|| {
        AppError::new(
            "configuration_missing",
            format!("Set {key} before sending with SES"),
            503,
        )
    })
}

pub fn configuration(env: &Env) -> Res<()> {
    if local(env) {
        return Ok(());
    }
    for key in ["AWS_REGION", "SES_CONFIGURATION_SET", "SNS_TOPIC_ARN"] {
        required(env, key)?;
    }
    if conf(env, "AWS_ROLE_ARN").is_some() {
        required(env, "AWS_WORKLOAD_KEY")?;
    } else {
        required(env, "AWS_ACCESS_KEY_ID")?;
        required(env, "AWS_SECRET_ACCESS_KEY")?;
    }
    let region = required(env, "AWS_REGION")?;
    if regional_host(&region, "email").is_none() {
        return Err(AppError::invalid("Invalid AWS region"));
    }
    Ok(())
}

async fn aws(
    env: &Env,
    service: &str,
    method: Method,
    path: &str,
    content_type: &str,
    body: &str,
) -> Res<(u16, String)> {
    let region = required(env, "AWS_REGION")?;
    let host = regional_host(&region, if service == "ses" { "email" } else { service })
        .ok_or_else(|| AppError::invalid("Invalid AWS region"))?;
    let credentials = if let Some(role) = conf(env, "AWS_ROLE_ARN") {
        crate::workload::credentials(env, &role, &region).await?
    } else {
        Credentials {
            access_key: required(env, "AWS_ACCESS_KEY_ID")?,
            secret_key: required(env, "AWS_SECRET_ACCESS_KEY")?,
            session_token: conf(env, "AWS_SESSION_TOKEN"),
        }
    };
    let date = DateTime::parse_from_rfc3339(&now())
        .map_err(|_| AppError::invalid("Invalid clock"))?
        .to_utc();
    let signature = sign(
        &credentials,
        &region,
        service,
        method.as_ref(),
        &host,
        path,
        content_type,
        body,
        &date,
    );
    let headers = Headers::new();
    for (k, v) in signature {
        if k != "host" {
            headers.set(&k, &v)?;
        }
    }
    let mut init = RequestInit::new();
    init.with_headers(headers)
        .with_method(method)
        .with_redirect(RequestRedirect::Manual);
    if !body.is_empty() {
        init.with_body(Some(body.into()));
    }
    let request = Request::new_with_init(&format!("https://{host}{path}"), &init)?;
    let mut res = Fetch::Request(request).send().await?;
    Ok((res.status_code(), res.text().await?))
}

pub async fn submit(env: &Env, mail: &Mail) -> Submission {
    if !crate::email_login::pilot_permitted(env, &mail.to) {
        return Submission::Rejected;
    }
    let Ok(config) = required(env, "SES_CONFIGURATION_SET") else {
        return Submission::Rejected;
    };
    let data = ses_payload(mail, &config).to_string();
    match aws(
        env,
        "ses",
        Method::Post,
        "/v2/email/outbound-emails",
        "application/json",
        &data,
    )
    .await
    {
        Ok((code, body)) => classify_response(code, &body),
        Err(_) => Submission::Unknown,
    }
}

pub async fn quota(env: &Env) -> Res<Value> {
    if local(env) {
        return Ok(
            json!({"ProductionAccessEnabled":false,"SendingEnabled":true,"SendQuota":{"Max24HourSend":100000,"MaxSendRate":1000,"SentLast24Hours":0}}),
        );
    }
    let (status, body) = aws(
        env,
        "ses",
        Method::Get,
        "/v2/email/account",
        "application/json",
        "",
    )
    .await?;
    if status != 200 {
        return Err(AppError::new(
            "provider_unavailable",
            "SES account status is unavailable. Check ses:GetAccount for the sending region.",
            503,
        ));
    }
    let account: Value = serde_json::from_str(&body)?;
    Ok(
        json!({"ProductionAccessEnabled":account["ProductionAccessEnabled"],"SendingEnabled":account["SendingEnabled"],"SendQuota":account["SendQuota"]}),
    )
}

pub async fn feedback(mut req: Request, env: &Env) -> Res<Response> {
    if req.method() != Method::Post {
        return Ok(Response::error("Method not allowed", 405)?);
    }
    let topic = required(env, "SNS_TOPIC_ARN")?;
    let region = required(env, "AWS_REGION")?;
    let raw = crate::bounded_body(&mut req, 512 * 1024).await?;
    let event: Value =
        serde_json::from_slice(&raw).map_err(|_| AppError::invalid("Expected SNS envelope"))?;
    let envelope = sns_envelope(&event, &topic, &region)
        .ok_or_else(|| AppError::new("invalid_signature", "Invalid SNS envelope", 403))?;
    let mut init = RequestInit::new();
    init.with_redirect(RequestRedirect::Manual);
    let cert_req = Request::new_with_init(&envelope.certificate, &init)?;
    let mut cert = Fetch::Request(cert_req).send().await?;
    if cert.status_code() != 200 {
        return Err(AppError::new(
            "provider_unavailable",
            "SNS certificate unavailable",
            503,
        ));
    }
    let pem = cert.text().await?;
    if pem.len() > 65536 || !verify_certificate(&envelope, &pem) {
        return Err(AppError::new(
            "invalid_signature",
            "SNS verification failed",
            403,
        ));
    }
    if envelope.kind == EnvelopeKind::Confirm {
        let tx = Tx::begin(env, Actor::system()).await?;
        if tx.get("provider_receipt", &envelope.id).await?.is_some() {
            return Ok(Response::ok("OK")?);
        }
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("Action", "ConfirmSubscription")
            .append_pair("Version", "2010-03-31")
            .append_pair("TopicArn", &topic)
            .append_pair("Token", envelope.token.as_deref().unwrap_or_default())
            .finish();
        let (status, _) = aws(
            env,
            "sns",
            Method::Post,
            "/",
            "application/x-www-form-urlencoded",
            &body,
        )
        .await?;
        if status != 200 {
            return Err(AppError::new(
                "provider_unavailable",
                "Could not confirm SNS subscription",
                503,
            ));
        }
    }
    let feedback = if envelope.kind == EnvelopeKind::Notification {
        parse_feedback(&envelope.message, &required(env, "SES_CONFIGURATION_SET")?)
    } else {
        None
    };
    for _ in 0..4 {
        let mut tx = Tx::begin(env, Actor::system()).await?;
        if tx.get("provider_receipt", &envelope.id).await?.is_some() {
            return Ok(Response::ok("OK")?);
        }
        if let Some(record) = &feedback {
            crate::delivery::record_feedback(&mut tx, record).await?;
        }
        let at = now();
        tx.put(
            "provider_receipt",
            &envelope.id,
            json!({"id":envelope.id,"at":at}),
        )?;
        tx.put("provider_state", "feedback", json!({"id":"feedback","last_at":at,"subscribed":envelope.kind!=EnvelopeKind::Unsubscribe}))?;
        match tx.commit().await {
            Ok(()) => return Ok(Response::ok("OK")?),
            Err(e) if e.code == "concurrent_update" => continue,
            Err(e) => return Err(e),
        }
    }
    // SNS only retries HTTP 429 and 5xx. A lost write race must remain eligible for redelivery.
    Err(AppError::new(
        "feedback_busy",
        "Retry this notification.",
        503,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    const CERT: &str = include_str!("../test/fixtures/sns-certificate.pem");
    #[test]
    fn timeout_server_error_and_missing_message_id_stay_unknown() {
        for code in [301, 408, 500, 502, 504] {
            assert_eq!(classify_response(code, ""), Submission::Unknown);
        }
        assert_eq!(classify_response(200, "{}"), Submission::Unknown);
        assert_eq!(classify_response(429, "{}"), Submission::Throttled);
        assert_eq!(classify_response(400, "{}"), Submission::Rejected);
        assert_eq!(
            classify_response(200, r#"{"MessageId":"receipt"}"#),
            Submission::Accepted("receipt".into())
        );
    }

    #[test]
    fn fixed_ses_request_matches_hmac_vector() {
        let credentials = Credentials {
            access_key: "fictional-access".into(),
            secret_key: "fictional-newsletter-key".into(),
            session_token: None,
        };
        let t = DateTime::parse_from_rfc3339("2026-11-24T12:00:00Z")
            .unwrap()
            .to_utc();
        let headers = sign(
            &credentials,
            "us-west-2",
            "ses",
            "POST",
            "email.us-west-2.amazonaws.com",
            "/v2/email/outbound-emails",
            "application/json",
            "{}",
            &t,
        );
        assert!(headers["authorization"].ends_with(
            "Signature=c9c8a492f1ca5589dc364d73db3edd356930a21622c5db29356da1a432b4f9be"
        ));
    }

    #[test]
    fn authenticates_sns_fields_and_pins_exact_certificate_origin() {
        let topic = "arn:aws:sns:us-west-2:000000000000:newsman";
        let mut v: Value =
            serde_json::from_str(include_str!("../test/fixtures/sns-notification.json")).unwrap();
        let e = sns_envelope(&v, topic, "us-west-2").unwrap();
        assert!(verify_certificate(&e, CERT));
        v["Message"] = json!("different");
        assert!(!verify_certificate(
            &sns_envelope(&v, topic, "us-west-2").unwrap(),
            CERT
        ));
        for host in [
            "sns.us-west-2.amazonaws.com.example.org",
            "sns.us-east-1.amazonaws.com",
            "127.0.0.1",
        ] {
            v["SigningCertURL"] = json!(format!(
                "https://{host}/SimpleNotificationService-00000000000000000000.pem"
            ));
            assert!(sns_envelope(&v, topic, "us-west-2").is_none());
        }
    }

    #[test]
    fn feedback_requires_our_configuration_set_and_parses_permanent_bounces() {
        let at = "2026-11-25T00:00:00Z";
        let event = json!({"eventType":"Bounce","mail":{"timestamp":at,"messageId":"ses-receipt","tags":{"ses:configuration-set":["newsman"],"newsman_attempt":["ses_1"]}},
            "bounce":{"timestamp":at,"bounceType":"Permanent","bouncedRecipients":[{"emailAddress":"reader@example.org"}]}});
        assert!(parse_feedback(&event.to_string(), "other").is_none());
        let parsed = parse_feedback(&event.to_string(), "newsman").unwrap();
        assert_eq!(parsed.kind, "hard_bounce");
        assert_eq!(parsed.addresses, ["reader@example.org"]);
        assert_eq!(parsed.attempt_id.as_deref(), Some("ses_1"));
    }

    #[test]
    fn display_names_support_international_publications_and_legacy_values() {
        let name = "Éditions, 音楽 ".repeat(9);
        let result = sender_mailbox(Some(&name), "news@example.org");
        let encoded = result.strip_suffix(" <news@example.org>").unwrap();
        let mut decoded = String::new();
        for word in encoded.split(' ') {
            assert!(word.len() <= 75);
            let bytes = STANDARD
                .decode(
                    word.strip_prefix("=?UTF-8?B?")
                        .unwrap()
                        .strip_suffix("?=")
                        .unwrap(),
                )
                .unwrap();
            decoded.push_str(std::str::from_utf8(&bytes).unwrap());
        }
        assert_eq!(decoded, name.trim());
        assert_eq!(sender_mailbox(None, "news@example.org"), "news@example.org");
        assert_eq!(
            sender_mailbox(Some("News\r\nInjected"), "news@example.org"),
            "news@example.org"
        );
    }

    #[test]
    fn newsletter_uses_individual_destination_and_rfc8058_headers() {
        let mut mail = Mail {
            from: sender_mailbox(Some("AudioTech News"), "a@example.org"),
            reply_to: "b@example.org".into(),
            to: "c@example.org".into(),
            subject: "Issue".into(),
            text: "Text".into(),
            html: "<p>Text</p>".into(),
            unsubscribe: Some("https://example.org/unsubscribe/token".into()),
            attempt_id: "a".into(),
            campaign_id: Some("c".into()),
        };
        let payload = ses_payload(&mail, "newsman");
        assert_eq!(
            payload["FromEmailAddress"],
            "AudioTech News <a@example.org>"
        );
        assert_eq!(payload["ReplyToAddresses"], json!(["b@example.org"]));
        assert_eq!(
            payload["Destination"]["ToAddresses"],
            json!(["c@example.org"])
        );
        assert_eq!(
            payload["Content"]["Simple"]["Headers"][1]["Value"],
            "List-Unsubscribe=One-Click"
        );
        mail.unsubscribe = None;
        assert!(ses_payload(&mail, "newsman")["Content"]["Simple"]["Headers"].is_null());
    }
}
