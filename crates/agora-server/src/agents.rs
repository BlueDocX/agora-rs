//! Agents people add to the office, beyond the one bot each person gets: a name, a
//! preset role (with its skills), a model, and the connected tools it may use.
//!
//! An agent runs on Agora's executor like every bot, so every call it makes is
//! ledgered, screened and checked by the guard; it never holds credentials. It has an
//! owner, who answers its approvals. Errands and `@mentions` reach it by name, like a
//! person's bot.
//!
//! - `GET /api/agents`: every agent.
//! - `POST /api/agents {name, preset?, model?, tools?}`: add one (you own it).
//! - `DELETE /api/agents/{id}`: its owner, or the office's owner.

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::company::Role;
use crate::store::{Op, Store};
use crate::{Hub, authorized, error, unauthorized};

pub(crate) const MAX_AGENTS: usize = 24;
const MAX_PER_PERSON: usize = 4;
const COLORS: [&str; 6] = ["#8b5cf6", "#0ea5e9", "#14b8a6", "#f97316", "#ec4899", "#84cc16"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AgentBot {
    pub id: String,
    pub name: String,
    /// The member who added it (they answer its approvals); `None` before anyone joined.
    pub owner: Option<String>,
    pub owner_name: Option<String>,
    /// The preset role it plays (`"<team>/<agent>"`), which brings its skills.
    pub preset: Option<String>,
    /// Its model; `None` = the office's.
    pub model: Option<String>,
    /// Connected tools it may use (`server.tool`); the rest are hidden from it.
    pub tools: Vec<String>,
    pub color: String,
    pub at_ms: u64,
}

impl AgentBot {
    /// What its instructions say about who it is.
    pub(crate) fn persona(&self) -> String {
        let Some((team, agent)) = self.preset.as_deref().and_then(|p| p.split_once('/')) else { return String::new() };
        agora_core::preset(team).and_then(|t| t.agents.into_iter().find(|a| a.id == agent)).map_or_else(String::new, |a| format!(" You work as {}: {}.{}", a.name, a.role, a.briefing()))
    }
}

pub(crate) struct Agents {
    list: Vec<AgentBot>,
    store: Option<Store>,
}

impl Agents {
    pub fn new(loaded: &[(String, String)], store: Option<Store>) -> Self {
        let list = loaded.iter().find(|(k, _)| k == "agents").and_then(|(_, v)| serde_json::from_str(v).ok()).unwrap_or_default();
        Self { list, store }
    }

    fn save(&self) {
        if let (Some(store), Ok(json)) = (&self.store, serde_json::to_string(&self.list)) {
            store.send(Op::Kv { key: "agents".into(), value: json });
        }
    }

    /// The agent called `name` (any case).
    pub fn by_name(&self, name: &str) -> Option<&AgentBot> {
        self.list.iter().find(|a| a.name.eq_ignore_ascii_case(name.trim()))
    }

    pub fn all(&self) -> Vec<AgentBot> {
        self.list.clone()
    }
}

pub(crate) type Shared = Mutex<Agents>;

/// The agent routes.
pub(crate) fn routes() -> axum::Router<Arc<Hub>> {
    use axum::routing::{delete, get};
    axum::Router::new().route("/api/agents", get(list).post(add)).route("/api/agents/{id}", delete(remove))
}

async fn list(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    Json(hub.agents().all()).into_response()
}

#[derive(Deserialize)]
struct Add {
    name: String,
    #[serde(default)]
    preset: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    tools: Vec<String>,
}

async fn add(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Result<Json<Add>, JsonRejection>) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("that isn't a valid agent: {}", e.body_text())),
    };
    let me = hub.member(&headers);
    if me.is_none() && !hub.company().members_view().is_empty() {
        return error(StatusCode::FORBIDDEN, "join the office first: an agent needs an owner");
    }
    let name = body.name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() || name.chars().count() > 24 || !name.chars().all(|c| c.is_alphanumeric() || c == ' ' || c == '-') {
        return error(StatusCode::BAD_REQUEST, "an agent's name is 1 to 24 letters, digits, spaces or dashes");
    }
    if hub.company().by_bot(&name).is_some() || hub.agents().by_name(&name).is_some() {
        return error(StatusCode::CONFLICT, &format!("there is already a bot called {name}"));
    }
    let preset = body.preset.map(|p| p.trim().to_string()).filter(|p| !p.is_empty());
    if let Some(p) = &preset {
        let known = p.split_once('/').and_then(|(team, agent)| agora_core::preset(team).filter(|t| t.agents.iter().any(|a| a.id == agent)));
        if known.is_none() {
            return error(StatusCode::BAD_REQUEST, &format!("there is no preset role {p}"));
        }
    }
    let model = body.model.map(|m| m.trim().to_string()).filter(|m| !m.is_empty());
    if let Some(m) = &model
        && let Err(why) = hub.config.check_model(m)
    {
        return error(StatusCode::BAD_REQUEST, &why);
    }
    // Only tools the office has connected and turned on.
    let on = hub.connected_tool_ids();
    if let Some(bad) = body.tools.iter().find(|t| !on.contains(t)) {
        return error(StatusCode::BAD_REQUEST, &format!("{bad} isn't a connected tool that's on"));
    }
    let mut agents = hub.agents();
    let owner = me.as_ref().map(|m| m.id.clone());
    if agents.list.len() >= MAX_AGENTS || agents.list.iter().filter(|a| a.owner == owner).count() >= MAX_PER_PERSON {
        return error(StatusCode::CONFLICT, &format!("at most {MAX_PER_PERSON} agents each, {MAX_AGENTS} in the office"));
    }
    let agent = AgentBot {
        id: format!("agent-{}", agora_engine::random_hex(6)),
        color: COLORS[agents.list.len() % COLORS.len()].to_string(),
        name,
        owner,
        owner_name: me.map(|m| m.name),
        preset,
        model,
        tools: body.tools,
        at_ms: crate::now_ms(),
    };
    agents.list.push(agent.clone());
    agents.save();
    drop(agents);
    hub.company().note("office", format!("{} added the agent {}", agent.owner_name.as_deref().unwrap_or("Someone"), agent.name), None);
    (StatusCode::CREATED, Json(agent)).into_response()
}

async fn remove(State(hub): State<Arc<Hub>>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let me = hub.member(&headers);
    let mut agents = hub.agents();
    let Some(i) = agents.list.iter().position(|a| a.id == id) else { return error(StatusCode::NOT_FOUND, "no such agent") };
    let mine = |a: &AgentBot| a.owner.is_none() || me.as_ref().is_some_and(|m| Some(&m.id) == a.owner.as_ref() || m.role == Role::Owner);
    if !mine(&agents.list[i]) {
        return error(StatusCode::FORBIDDEN, "only its owner or the office's owner can remove it");
    }
    agents.list.remove(i);
    agents.save();
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_with_a_preset_brings_its_role_and_skills() {
        let a = AgentBot { id: "a".into(), name: "Muse".into(), owner: None, owner_name: None, preset: Some("research-lab/fact-checker".into()), model: None, tools: vec![], color: COLORS[0].into(), at_ms: 0 };
        let persona = a.persona();
        assert!(persona.starts_with(" You work as Fact-Checker:"), "{persona}");
        assert!(persona.contains("Your skills"));
        assert_eq!(AgentBot { preset: None, ..a }.persona(), "");
    }
}
