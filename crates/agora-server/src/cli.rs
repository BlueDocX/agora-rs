//! Keys for coding CLIs (Claude Code, Codex, `OpenCode`, Gemini CLI): each member mints
//! their own, one per tool, and the CLI sends it as `Authorization: Bearer agk_…` to
//! `/mcp`. A key acts as that member, so their errands, approvals and @mentions are
//! theirs. It is stored only as a hash, shown once, and can be revoked; it dies with
//! the member (a key whose member left finds no one).
//!
//! A CLI key works only at `/mcp` ([`only_at_mcp`]): every other route refuses it, so
//! an agent with a shell can't use it to approve calls, change roles or tools, read
//! the invite, or make more keys. What it may do is exactly the MCP tools.
//!
//! - `GET /api/cli/keys`: your keys (label, when made, when last used).
//! - `POST /api/cli/keys {label}`: a new key, answered once as `{id, key}`.
//! - `DELETE /api/cli/keys/{id}`: revoke one of yours.

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::company::{Member, hash_key};
use crate::store::{Op, Store};
use crate::{Hub, authorized, error, unauthorized};

/// What every CLI key starts with (so it is never mistaken for the office token).
pub(crate) const PREFIX: &str = "agk_";
const MAX_PER_MEMBER: usize = 6;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CliKey {
    pub id: String,
    pub member: String,
    pub label: String,
    /// SHA-256 of the key; the key itself is only ever in the member's hands.
    #[serde(skip_serializing_if = "String::is_empty", default)]
    hash: String,
    pub at_ms: u64,
    #[serde(default)]
    pub used_ms: u64,
}

pub(crate) struct CliKeys {
    list: Vec<CliKey>,
    store: Option<Store>,
}

pub(crate) type Shared = Mutex<CliKeys>;

impl CliKeys {
    pub fn new(loaded: &[(String, String)], store: Option<Store>) -> Self {
        let list = loaded.iter().find(|(k, _)| k == "cli_keys").and_then(|(_, v)| serde_json::from_str(v).ok()).unwrap_or_default();
        Self { list, store }
    }

    fn save(&self) {
        if let (Some(store), Ok(json)) = (&self.store, serde_json::to_string(&self.list)) {
            store.send(Op::Kv { key: "cli_keys".into(), value: json });
        }
    }

    /// The member id a CLI key belongs to, noting that it was used (kept at most every
    /// ten minutes, so "last used" survives a restart without a write per call).
    pub fn member_of(&mut self, key: &str) -> Option<String> {
        if !key.starts_with(PREFIX) {
            return None;
        }
        let hash = hash_key(key);
        let now = crate::now_ms();
        let k = self.list.iter_mut().find(|k| crate::same(k.hash.as_bytes(), hash.as_bytes()))?;
        let stale = now.saturating_sub(k.used_ms) > 10 * 60 * 1000;
        k.used_ms = now;
        let member = k.member.clone();
        if stale {
            self.save();
        }
        Some(member)
    }

    /// Drop the keys of `member` (who left). True if they had any.
    pub fn drop_member(&mut self, member: &str) -> bool {
        let before = self.list.len();
        self.list.retain(|k| k.member != member);
        let dropped = self.list.len() != before;
        if dropped {
            self.save();
        }
        dropped
    }

}

/// The CLI key a request carries, if any: `Authorization: Bearer agk_…`.
pub(crate) fn key_of(headers: &HeaderMap) -> Option<&str> {
    headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ").map(str::trim).filter(|k| k.starts_with(PREFIX))
}

/// Refuse a CLI key anywhere but `/mcp`.
pub(crate) async fn only_at_mcp(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    if key_of(request.headers()).is_some() && request.uri().path() != "/mcp" {
        return error(StatusCode::FORBIDDEN, "a CLI key works only at /mcp: approvals, tools and roles are for people in the office");
    }
    next.run(request).await
}

pub(crate) fn routes() -> axum::Router<Arc<Hub>> {
    use axum::routing::{delete, get};
    axum::Router::new().route("/api/cli/keys", get(list).post(mint)).route("/api/cli/keys/{id}", delete(revoke))
}

/// The member behind the browser's own key (never a CLI key).
fn browser_member(hub: &Hub, headers: &HeaderMap) -> Option<Member> {
    if key_of(headers).is_some() {
        return None;
    }
    hub.member(headers)
}

async fn list(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let Some(me) = browser_member(&hub, &headers) else { return error(StatusCode::FORBIDDEN, "join the office first") };
    let mine: Vec<CliKey> = hub.cli_keys().list.iter().filter(|k| k.member == me.id).map(|k| CliKey { hash: String::new(), ..k.clone() }).collect();
    Json(mine).into_response()
}

#[derive(Deserialize)]
struct Mint {
    label: String,
}

async fn mint(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Result<Json<Mint>, JsonRejection>) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let Some(me) = browser_member(&hub, &headers) else { return error(StatusCode::FORBIDDEN, "join the office first, from its page: a CLI key acts as you") };
    let Ok(Json(Mint { label })) = body else { return error(StatusCode::BAD_REQUEST, "send {\"label\": \"Claude Code\"}") };
    let label = label.trim().to_string();
    if label.is_empty() || label.chars().count() > 40 || label.chars().any(char::is_control) {
        return error(StatusCode::BAD_REQUEST, "a key's label is 1 to 40 characters on one line");
    }
    let mut keys = hub.cli_keys();
    if keys.list.iter().filter(|k| k.member == me.id).count() >= MAX_PER_MEMBER {
        return error(StatusCode::CONFLICT, &format!("at most {MAX_PER_MEMBER} CLI keys each: revoke one first"));
    }
    let key = format!("{PREFIX}{}", agora_engine::random_hex(24));
    let id = format!("cli-{}", agora_engine::random_hex(4));
    keys.list.push(CliKey { id: id.clone(), member: me.id, label, hash: hash_key(&key), at_ms: crate::now_ms(), used_ms: 0 });
    keys.save();
    (StatusCode::CREATED, Json(serde_json::json!({ "id": id, "key": key }))).into_response()
}

async fn revoke(State(hub): State<Arc<Hub>>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let Some(me) = browser_member(&hub, &headers) else { return error(StatusCode::FORBIDDEN, "join the office first") };
    let mut keys = hub.cli_keys();
    let before = keys.list.len();
    keys.list.retain(|k| !(k.id == id && k.member == me.id));
    if keys.list.len() == before {
        return error(StatusCode::NOT_FOUND, "you have no such key");
    }
    keys.save();
    let left = keys.list.iter().any(|k| k.member == me.id);
    drop(keys);
    if !left {
        hub.company().set_on_cli(&me.id, false);
    }
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_finds_its_member_and_only_a_cli_key_counts() {
        let key = format!("{PREFIX}abc");
        let mut keys = CliKeys { list: vec![CliKey { id: "cli-1".into(), member: "m1".into(), label: "Claude Code".into(), hash: hash_key(&key), at_ms: 0, used_ms: 0 }], store: None };
        assert_eq!(keys.member_of(&key).as_deref(), Some("m1"));
        assert!(keys.list[0].used_ms > 0, "use is noted");
        assert_eq!(keys.member_of(&format!("{PREFIX}abd")), None);
        assert_eq!(keys.member_of(&keys.list[0].hash.clone()), None, "the stored hash is not a key");
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer office-token".parse().unwrap());
        assert_eq!(key_of(&h), None);
        h.insert("authorization", format!("Bearer {key}").parse().unwrap());
        assert_eq!(key_of(&h), Some(key.as_str()));
        assert!(serde_json::to_string(&CliKey { hash: String::new(), ..keys.list[0].clone() }).unwrap().find("hash").is_none(), "a listing never shows the hash");
    }
}
