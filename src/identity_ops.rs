//! Memberships and delegated grants use the same transactional command path as other operator actions.
use crate::core::{AppError, Res, Tx, expect_version, page_values, req_str, uid};
use serde_json::{Value, json};

pub(crate) fn normalized_email(raw: &str) -> Res<String> {
    let email = raw.trim().to_lowercase();
    let (left, right) = email.split_once('@').unwrap_or_default();
    if email.len() > 254
        || left.is_empty()
        || right.is_empty()
        || right.contains('@')
        || email
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return Err(AppError::invalid("A verified email address is required"));
    }
    Ok(email)
}

pub(crate) fn public_operator(r: &Value) -> Value {
    json!({"id":r["id"],"email":r["email"],"version":r["version"],"state":r["state"]})
}

fn public_grant(r: &Value) -> Value {
    let mut value = json!({"id":r["id"],"operator_id":r["operator_id"],"client":r["client"],"scopes":r["scopes"],
        "active":r["active"]==true && r["expires_s"].as_i64().is_some_and(|t|t>chrono::Utc::now().timestamp()),"version":r["version"]});
    if let Some(exp) = r.get("expires_at") {
        value["expires_at"] = exp.clone();
    }
    value
}

pub(crate) fn invitation(email: &str) -> Value {
    json!({"id":uid("op"),"email":email,"version":1,"state":"invited"})
}

fn bump(r: &mut Value) {
    r["version"] = json!(r["version"].as_u64().unwrap_or_default() + 1);
}

pub async fn run(tx: &mut Tx<'_>, name: &str, a: &Value) -> Res<Value> {
    match name {
        "operators_list" => page_values(
            tx.list("operator")
                .await?
                .iter()
                .map(public_operator)
                .collect(),
            a,
        ),
        "grants_list" => page_values(
            tx.list("grant").await?.iter().map(public_grant).collect(),
            a,
        ),
        "operators_add" => {
            let email = normalized_email(req_str(a, "email")?)?;
            if !crate::email_login::permitted(tx.env, &email) {
                return Err(crate::access::denied());
            }
            if let Some(existing) = tx
                .list("operator")
                .await?
                .iter()
                .find(|r| r["state"] != "removed" && r["email"] == email)
            {
                return Ok(public_operator(existing));
            }
            let item = invitation(&email);
            let id = req_str(&item, "id")?.to_owned();
            tx.put("operator", &id, item.clone())?;
            Ok(public_operator(&item))
        }
        "operators_remove" => {
            let id = req_str(a, "id")?;
            let mut r = tx.require("operator", id).await?;
            expect_version(&r, a)?;
            if r["state"] == "active"
                && !tx
                    .list("operator")
                    .await?
                    .iter()
                    .any(|x| x["state"] == "active" && x["id"] != id)
            {
                return Err(AppError::new(
                    "permission_denied",
                    "Keep at least one bound operator",
                    409,
                ));
            }
            if r["state"] != "removed" {
                r["state"] = json!("removed");
                bump(&mut r);
                for mut grant in tx.list("grant").await? {
                    if grant["operator_id"] == id && grant["active"] == true {
                        grant["active"] = json!(false);
                        bump(&mut grant);
                        let gid = req_str(&grant, "id")?.to_owned();
                        tx.put("grant", &gid, grant)?;
                    }
                }
                tx.put("operator", id, r.clone())?;
            }
            Ok(json!({"id":id,"version":r["version"]}))
        }
        "grants_revoke" => {
            let id = req_str(a, "id")?;
            let mut r = tx.require("grant", id).await?;
            expect_version(&r, a)?;
            if r["active"] == true {
                r["active"] = json!(false);
                bump(&mut r);
                tx.put("grant", id, r.clone())?;
            }
            Ok(json!({"id":id,"version":r["version"]}))
        }
        _ => Err(AppError::new(
            "not_found",
            "Unknown identity operation",
            404,
        )),
    }
}
