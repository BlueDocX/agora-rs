//! The office chat over HTTP.
//!
//! - `POST /api/chat {"text": "...", "name"?: "...", "team"?: ..., "run"?: "<run id>"}`
//!   posts a message; the team answers (see `agora_engine::chat`), knowing how the
//!   given run (the one the person is watching) stands. One conversation at a time:
//!   while the team is still answering, another post gets 429.
//! - `GET /api/chat/events` streams the chat as Server-Sent Events: the kept history,
//!   then each message, `typing` and `idle` live. It never ends on its own.
//!
//! Replies are paid from one chat budget for the life of the server
//! (`HubConfig::chat_cap_usd`), separate from the runs' budgets.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use agora_core::Team;
use agora_engine::RunEvent;
use agora_engine::chat::{self, Addressed, ChatError, ChatMessage, Kind, LEAD_COLOR, Voice};
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::{Hub, TeamChoice, authorized, error, resolve, to_sse, unauthorized};

pub const MAX_CHAT_CHARS: usize = 1000;
/// Messages kept for people who open the page later; older ones are dropped.
const KEEP_MESSAGES: usize = 200;
const PERSON_COLOR: &str = "#17150f";

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ChatEvent {
    Message { message: ChatMessage },
    /// Someone on the team is writing a reply.
    Typing { from: String, agent: Option<String>, color: String },
    /// The team has finished answering; the chat takes a new message.
    Idle,
}

pub(crate) struct ChatState {
    messages: Vec<ChatMessage>,
    next_id: u64,
    busy: bool,
    live: broadcast::Sender<ChatEvent>,
}

impl Default for ChatState {
    fn default() -> Self {
        Self { messages: Vec::new(), next_id: 1, busy: false, live: broadcast::channel(256).0 }
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl Hub {
    fn chat(&self) -> std::sync::MutexGuard<'_, ChatState> {
        self.chat.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Add a message and tell every watcher, under one lock (so nobody misses one).
    fn post(&self, kind: Kind, from: &str, agent: Option<String>, color: &str, text: String) {
        let mut chat = self.chat();
        let message = ChatMessage { id: chat.next_id, kind, from: from.to_string(), agent, color: color.to_string(), text, at_ms: now_ms() };
        chat.next_id += 1;
        let _ = chat.live.send(ChatEvent::Message { message: message.clone() });
        chat.messages.push(message);
        if chat.messages.len() > KEEP_MESSAGES {
            let extra = chat.messages.len() - KEEP_MESSAGES;
            chat.messages.drain(..extra);
        }
    }

    fn announce(&self, event: ChatEvent) {
        let _ = self.chat().live.send(event);
    }

    /// Where the run the person is watching stands, so the team can talk about it.
    fn run_context(&self, run: Option<&str>) -> String {
        let Some(room) = run.and_then(|id| self.room(id)) else { return String::new() };
        summarize(&room.brief, room.done(), &room.history())
    }
}

/// The brief and where each task stands, in a few lines.
fn summarize(brief: &str, done: bool, history: &[RunEvent]) -> String {
    type Row = (String, String, Option<String>, &'static str); // id, title, agent, state
    fn set(tasks: &mut [Row], id: &str, state: &'static str) {
        if let Some(t) = tasks.iter_mut().find(|t| t.0 == id) {
            t.3 = state;
        }
    }
    let mut agents: HashMap<String, String> = HashMap::new();
    let mut tasks: Vec<Row> = Vec::new();
    for event in history {
        match event {
            RunEvent::Staffed { team } => {
                agents = team.agents.iter().map(|a| (a.id.clone(), a.name.clone())).collect();
            }
            RunEvent::Planned { tasks: planned, .. } => {
                tasks = planned.iter().map(|t| (t.id.clone(), t.title.clone(), t.agent.clone(), "scheduled")).collect();
            }
            RunEvent::TaskStarted { task } => set(&mut tasks, task, "working"),
            RunEvent::Drafted { task, .. } => set(&mut tasks, task, "in review"),
            RunEvent::TaskAccepted { task, .. } => set(&mut tasks, task, "done"),
            RunEvent::TaskFailed { task, .. } => set(&mut tasks, task, "needs a person"),
            RunEvent::TaskSkipped { task } => set(&mut tasks, task, "skipped"),
            _ => {}
        }
    }
    let mut out = format!("Brief ({}): {}\n", if done { "finished" } else { "in progress" }, brief.chars().take(600).collect::<String>());
    for (_, title, agent, state) in &tasks {
        let who = agent.as_ref().and_then(|a| agents.get(a)).map_or(String::new(), |n| format!(" ({n})"));
        let _ = writeln!(out, "- {title}{who}: {state}");
    }
    out
}

#[derive(Deserialize)]
pub(crate) struct Post {
    text: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    team: Option<TeamChoice>,
    /// The run the person has open, if any.
    #[serde(default)]
    run: Option<String>,
}

pub(crate) async fn post(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Result<Json<Post>, JsonRejection>) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("that isn't a valid chat message: {}", e.body_text())),
    };
    let text = body.text.trim().to_string();
    if text.is_empty() || text.chars().count() > MAX_CHAT_CHARS {
        return error(StatusCode::BAD_REQUEST, &format!("a message is 1 to {MAX_CHAT_CHARS} characters"));
    }
    // A display name is a label, nothing more: short, one line.
    let name: String = body.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).unwrap_or("You").chars().filter(|c| !c.is_control()).take(30).collect();
    let team = match resolve(body.team) {
        Ok(Some(team)) => team,
        Ok(None) => agora_core::preset("agora").expect("the default preset exists"),
        Err(why) => return error(StatusCode::BAD_REQUEST, &why),
    };
    // People can't take the names the office speaks with.
    let taken = |n: &str| n.eq_ignore_ascii_case("lead") || n.eq_ignore_ascii_case("office") || team.agents.iter().any(|a| a.name.trim().eq_ignore_ascii_case(n));
    if taken(&name) {
        return error(StatusCode::BAD_REQUEST, &format!("\"{name}\" is a teammate's name; pick another display name"));
    }
    let Some(budget) = hub.chat_budget.clone() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "chat is off: the model has no published price to budget against");
    };
    {
        let mut chat = hub.chat();
        if chat.busy {
            return error(StatusCode::TOO_MANY_REQUESTS, "the team is still answering; send this when they're done");
        }
        chat.busy = true;
    }
    let addressed = chat::addressees(&text, &team);
    // Worked out once: every reply in this conversation talks about the same run.
    let context = hub.run_context(body.run.as_deref());
    hub.post(Kind::Person, &name, None, PERSON_COLOR, text);
    let worker = Arc::clone(&hub);
    tokio::spawn(async move {
        // Whatever happens in the conversation, the chat is freed at the end.
        let conversation = tokio::spawn(converse(Arc::clone(&worker), team, addressed, budget, context));
        if conversation.await.is_err() {
            worker.post(Kind::Notice, "Office", None, PERSON_COLOR, "The team stopped answering (internal error).".into());
        }
        // Freed and announced under one lock, so a new conversation's events can't
        // come before this Idle.
        let mut chat = worker.chat();
        chat.busy = false;
        let _ = chat.live.send(ChatEvent::Idle);
    });
    StatusCode::ACCEPTED.into_response()
}

/// Everyone who was asked answers in turn, each seeing the replies before theirs.
async fn converse(hub: Arc<Hub>, team: Team, addressed: Addressed, budget: Arc<agora_engine::Budget>, context: String) {
    let voices: Vec<Voice<'_>> = match &addressed {
        Addressed::All => team.agents.iter().map(Voice::Agent).chain([Voice::Lead { summing_up: true }]).collect(),
        Addressed::Agents(ids) => ids.iter().map(|&i| Voice::Agent(&team.agents[i])).collect(),
        Addressed::Lead => vec![Voice::Lead { summing_up: false }],
    };
    for voice in voices {
        let model = match voice {
            Voice::Agent(a) => a.model.clone().unwrap_or_else(|| hub.config.model.clone()),
            Voice::Lead { .. } => hub.config.model.clone(),
        };
        let (kind, agent, color) = match voice {
            Voice::Agent(a) => (Kind::Agent, Some(a.id.clone()), a.color.clone()),
            Voice::Lead { .. } => (Kind::Lead, None, LEAD_COLOR.to_string()),
        };
        hub.announce(ChatEvent::Typing { from: voice.name().to_string(), agent: agent.clone(), color: color.clone() });
        let history = hub.chat().messages.clone();
        let request = chat::request(&model, &team, voice, &history, &context);
        let Some(client) = hub.client_for(&model) else {
            hub.post(Kind::Notice, "Office", None, PERSON_COLOR, format!("{} couldn't answer: no host for model \"{model}\"", voice.name()));
            continue;
        };
        match chat::reply(client, &budget, &request).await {
            Ok(text) if !text.is_empty() => hub.post(kind, voice.name(), agent, &color, text),
            Ok(_) => {}
            Err(e) => {
                let stop = matches!(e, ChatError::Budget(_));
                hub.post(Kind::Notice, "Office", None, PERSON_COLOR, format!("{} couldn't answer: {e}", voice.name()));
                if stop {
                    break;
                }
            }
        }
    }
}

pub(crate) async fn events(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let (history, live) = {
        let chat = hub.chat();
        let mut history: Vec<ChatEvent> = chat.messages.iter().map(|m| ChatEvent::Message { message: m.clone() }).collect();
        if chat.busy {
            history.push(ChatEvent::Typing { from: "The team".into(), agent: None, color: LEAD_COLOR.into() });
        }
        (history, chat.live.subscribe())
    };
    let past = stream::iter(history).filter_map(|e| async move { to_sse(&e) });
    // A watcher that falls 256 events behind is dropped; its browser reconnects and
    // gets the history again.
    let live = stream::unfold(live, |mut rx| async move { rx.recv().await.ok().map(|e| (e, rx)) }).filter_map(|e| async move { to_sse(&e) });
    Sse::new(past.chain(live)).keep_alive(KeepAlive::default()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_engine::TaskView;

    #[test]
    fn the_run_context_names_who_is_doing_what() {
        let team = agora_core::preset("creative-studio").expect("preset");
        let task = |id: &str, agent: &str| TaskView { id: id.into(), title: format!("Write {id}"), deps: vec![], agent: Some(agent.into()) };
        let history = vec![
            RunEvent::Staffed { team },
            RunEvent::Planned { tasks: vec![task("tagline", "copywriter"), task("brief", "director")], attempts: 1 },
            RunEvent::TaskStarted { task: "tagline".into() },
            RunEvent::TaskAccepted { task: "brief".into(), drafts: 1, reviews: 1 },
        ];
        let text = summarize("a launch", false, &history);
        assert!(text.starts_with("Brief (in progress): a launch"));
        assert!(text.contains("- Write tagline (Copywriter): working"));
        assert!(text.contains("- Write brief (Creative Director): done"));
    }
}
