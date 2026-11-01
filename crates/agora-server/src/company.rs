//! The company: the people in the office, each with a bot working under them, and
//! how they talk to each other.
//!
//! - `POST /api/company/join {"name", "area"?, "bot_role"?, "invite"?}` joins (at most
//!   [`MAX_MEMBERS`]). The first person in is the owner; everyone after needs the
//!   owner's invite code. The reply holds the member and a private `key` (shown once;
//!   the office keeps only its hash): sent as `X-Agora-Member`, or set as a cookie by
//!   `POST /api/session`, it says who is acting.
//! - `PATCH /api/company/members/{id} {"role"?, "area"?, "bot_role"?}`: the owner makes
//!   someone a lead of an area (or a member again); anyone may change their own area
//!   and bot. Nothing changes unless all of it is allowed.
//! - `DELETE /api/company/members/{id}`: leave (yourself), or the owner removes someone.
//! - `GET /api/company/whoami`: who the key belongs to, the channels, the call room,
//!   and (for the owner) the invite code.
//! - `POST /api/company/messages {"to", "text"}`: say something in a channel
//!   (`#general`, or `#<area>` for any area someone works in) or to one person
//!   (`@<member id>`). In a channel, `@Bot do this` gives that bot an errand; its
//!   checked report comes back to the channel.
//! - `GET /api/company/events` streams the company: a snapshot, then joins, changes,
//!   leaves, activity and the messages this person may see (channels, and their own
//!   direct messages). A stream opened by a member marks them online.
//!
//! An errand can be sent to an area: it runs on the bot of that area's lead.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::store::{Loaded, Op, Store};
use crate::{Hub, authorized, error, to_sse, unauthorized};

pub const MAX_MEMBERS: usize = 24;
const KEEP_ACTIVITY: usize = 60;
const KEEP_MESSAGES: usize = 500;
/// Messages a new stream starts with.
const SNAPSHOT_MESSAGES: usize = 300;
pub(crate) const MAX_MESSAGE_CHARS: usize = 1000;
const MEMBER_HEADER: &str = "x-agora-member";
pub(crate) const MEMBER_COOKIE: &str = "agora_member";
pub(crate) const GENERAL: &str = "#general";
/// Bots are named in join order.
pub(crate) const BOTS: [&str; MAX_MEMBERS] = [
    "Scout", "Atlas", "Nova", "Echo", "Pixel", "Sage", "Orbit", "Quill", "Juno", "Vega", "Ember", "Lumen", "Rook", "Tide", "Wren", "Cobalt",
    "Indigo", "Marlo", "Pico", "Sol", "Tango", "Umber", "Zephyr", "Kite",
];
/// Member colors, reused in order once all are taken (butter and purple stay the
/// Lead's and reviewer's).
const COLORS: [&str; 12] = ["#4285f4", "#34a853", "#ea4335", "#14b8a6", "#f97316", "#ec4899", "#64748b", "#a16207", "#0ea5e9", "#84cc16", "#e11d48", "#7c3aed"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Lead,
    Member,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Member {
    pub id: String,
    pub name: String,
    pub role: Role,
    /// The area they look after (leads) or work in; "" = none.
    pub area: String,
    pub color: String,
    /// Their bot's name: it runs their errands, and errands sent to their area.
    pub bot: String,
    /// The preset role their bot plays ("<team>/<agent>", e.g. "research-lab/fact-checker"),
    /// which brings that agent's role and skills to its errands. `None` = a plain assistant.
    pub bot_role: Option<BotRole>,
    pub online: bool,
    /// They have worked from a coding CLI (with a CLI key) since the office started.
    pub on_cli: bool,
    /// SHA-256 of their key: the key itself is only ever in their hands.
    #[serde(skip)]
    key_hash: String,
    #[serde(skip)]
    streams: usize,
}

/// A member as the store keeps them.
#[derive(Serialize, Deserialize)]
struct Stored {
    id: String,
    name: String,
    role: Role,
    area: String,
    color: String,
    bot: String,
    bot_role: Option<String>,
    key_hash: String,
}

/// A preset agent a bot plays, as people see it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct BotRole {
    pub preset: String,
    pub name: String,
    pub role: String,
    pub kit: Vec<String>,
    #[serde(skip)]
    pub agent: Box<agora_core::Agent>,
}

/// The preset agent `"<team>/<agent>"`, if there is one.
fn bot_role(preset: &str) -> Option<BotRole> {
    let (team, agent) = preset.split_once('/')?;
    let agent = agora_core::preset(team)?.agents.into_iter().find(|a| a.id == agent)?;
    Some(BotRole { preset: preset.to_string(), name: agent.name.clone(), role: agent.role.clone(), kit: agent.kit.clone(), agent: Box::new(agent) })
}

impl Member {
    /// What the bot's instructions say about who it is: its preset role and skills.
    #[must_use]
    pub(crate) fn persona(&self) -> String {
        self.bot_role.as_ref().map_or_else(String::new, |r| format!(" You work as their {}: {}.{}", r.name, r.role, r.agent.briefing()))
    }

    fn stored(&self) -> Stored {
        Stored {
            id: self.id.clone(),
            name: self.name.clone(),
            role: self.role,
            area: self.area.clone(),
            color: self.color.clone(),
            bot: self.bot.clone(),
            bot_role: self.bot_role.as_ref().map(|r| r.preset.clone()),
            key_hash: self.key_hash.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Activity {
    pub at_ms: u64,
    /// The member it is about.
    pub member: String,
    pub text: String,
    pub errand: Option<String>,
}

/// Something said in a channel or to one person.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Message {
    pub n: u64,
    pub at_ms: u64,
    /// The member who said it, or whose bot did.
    pub from: String,
    /// Who it shows as: the person, or their bot.
    pub name: String,
    /// Said by `from`'s bot (an errand's checked report).
    #[serde(default)]
    pub bot: bool,
    /// `#general`, `#<area>`, or `@<member id>`.
    pub to: String,
    pub text: String,
    #[serde(default)]
    pub errand: Option<String>,
}

impl Message {
    /// Whether `member` may see it: every member sees the channels; a direct message
    /// is for the two people in it.
    fn visible_to(&self, member: Option<&str>) -> bool {
        let Some(member) = member else { return false };
        match self.to.strip_prefix('@') {
            Some(to) => to == member || self.from == member,
            None => true,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum CompanyEvent {
    Snapshot { members: Vec<Member>, activity: Vec<Activity>, messages: Vec<Message> },
    Member { member: Member },
    Left { id: String },
    Activity { activity: Activity },
    Message { message: Message },
    /// These messages (bot reports from an errand) were withdrawn by a forget.
    Withdrawn { ns: Vec<u64>, text: String },
}

pub(crate) struct Company {
    members: Vec<Member>,
    activity: Vec<Activity>,
    messages: Vec<Message>,
    live: broadcast::Sender<CompanyEvent>,
    joined: usize,
    said: u64,
    /// What a newcomer must give to join once someone owns the office.
    invite: String,
    /// The call room's secret name (so only the company can find it).
    call_room: String,
    store: Option<Store>,
}

impl Default for Company {
    fn default() -> Self {
        Self::new(None, &Loaded::default())
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

pub(crate) fn hash_key(key: &str) -> String {
    use sha2::{Digest, Sha256};
    agora_engine::signing::to_hex(&Sha256::digest(key.as_bytes()))
}

fn clean(s: &str, what: &str, max: usize) -> Result<String, String> {
    let s = s.trim();
    if s.chars().count() > max || s.chars().any(char::is_control) || s.contains('@') || s.contains('#') {
        return Err(format!("{what} must be at most {max} characters on one line, without @ or #"));
    }
    Ok(s.to_string())
}

impl Company {
    /// The company as the store left it (or a new one).
    pub(crate) fn new(store: Option<Store>, loaded: &Loaded) -> Self {
        let kv = |key: &str| loaded.kv.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        let members: Vec<Member> = loaded
            .members
            .iter()
            .filter_map(|json| serde_json::from_str::<Stored>(json).ok())
            .map(|s| Member {
                bot_role: s.bot_role.as_deref().and_then(bot_role),
                id: s.id,
                name: s.name,
                role: s.role,
                area: s.area,
                color: s.color,
                bot: s.bot,
                online: false,
                on_cli: false,
                key_hash: s.key_hash,
                streams: 0,
            })
            .collect();
        let activity = loaded.activity.iter().filter_map(|j| serde_json::from_str(j).ok()).collect();
        let messages: Vec<Message> = loaded.messages.iter().filter_map(|j| serde_json::from_str(j).ok()).collect();
        let joined = kv("joined").and_then(|j| j.parse().ok()).unwrap_or(members.len());
        let mut company = Self {
            said: messages.last().map_or(0, |m| m.n),
            members,
            activity,
            messages,
            live: broadcast::channel(256).0,
            joined,
            invite: kv("invite").unwrap_or_else(|| agora_engine::random_hex(9)),
            call_room: kv("call_room").unwrap_or_else(|| agora_engine::random_hex(10)),
            store,
        };
        company.trim();
        company.save(Op::Kv { key: "invite".into(), value: company.invite.clone() });
        company.save(Op::Kv { key: "call_room".into(), value: company.call_room.clone() });
        company
    }

    fn save(&self, op: Op) {
        if let Some(store) = &self.store {
            store.send(op);
        }
    }

    fn save_member(&self, m: &Member) {
        if let Ok(json) = serde_json::to_string(&m.stored()) {
            self.save(Op::Member { id: m.id.clone(), json });
        }
    }

    fn trim(&mut self) {
        let extra = self.activity.len().saturating_sub(KEEP_ACTIVITY);
        self.activity.drain(..extra);
        let extra = self.messages.len().saturating_sub(KEEP_MESSAGES);
        self.messages.drain(..extra);
    }

    fn send(&self, event: CompanyEvent) {
        let _ = self.live.send(event);
    }

    /// The people, as anyone in the office may see them.
    pub(crate) fn members_view(&self) -> Vec<Member> {
        self.members.clone()
    }

    /// The newest `n` activity lines.
    pub(crate) fn recent(&self, n: usize) -> Vec<String> {
        self.activity.iter().rev().take(n).rev().map(|a| a.text.clone()).collect()
    }

    pub(crate) fn by_key(&self, key: &str) -> Option<&Member> {
        if key.is_empty() {
            return None;
        }
        let hash = hash_key(key);
        self.members.iter().find(|m| crate::same(m.key_hash.as_bytes(), hash.as_bytes()))
    }

    pub(crate) fn by_id(&self, id: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.id == id)
    }

    /// The member whose bot is called `bot` (any case).
    pub(crate) fn by_bot(&self, bot: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.bot.eq_ignore_ascii_case(bot.trim()))
    }

    /// The lead (or owner) looking after `area`, if anyone is.
    pub(crate) fn lead_of(&self, area: &str) -> Option<&Member> {
        let area = area.trim();
        self.members.iter().find(|m| m.role != Role::Member && !m.area.is_empty() && m.area.eq_ignore_ascii_case(area))
    }

    /// `#general` and one channel per area someone works in.
    pub(crate) fn channels(&self) -> Vec<String> {
        let mut channels = vec![GENERAL.to_string()];
        for m in &self.members {
            let c = format!("#{}", m.area.to_lowercase());
            if !m.area.is_empty() && !channels.contains(&c) {
                channels.push(c);
            }
        }
        channels
    }

    pub(crate) fn call_room(&self) -> &str {
        &self.call_room
    }

    pub(crate) fn invite(&self) -> &str {
        &self.invite
    }

    /// A new invite code: the old one stops working.
    pub(crate) fn rotate_invite(&mut self) {
        self.invite = agora_engine::random_hex(9);
        self.save(Op::Kv { key: "invite".into(), value: self.invite.clone() });
    }

    pub(crate) fn note(&mut self, member: &str, text: String, errand: Option<String>) {
        let activity = Activity { at_ms: now_ms(), member: member.to_string(), text, errand };
        self.send(CompanyEvent::Activity { activity: activity.clone() });
        if let Ok(json) = serde_json::to_string(&activity) {
            self.save(Op::Activity { json });
        }
        self.activity.push(activity);
        self.trim();
    }

    /// Post a message (already checked: see [`Company::check_message`]).
    pub(crate) fn say(&mut self, from: &Member, bot: bool, to: &str, text: String, errand: Option<String>) -> Message {
        self.said += 1;
        let name = if bot { from.bot.clone() } else { from.name.clone() };
        let message = Message { n: self.said, at_ms: now_ms(), from: from.id.clone(), name, bot, to: to.to_string(), text, errand };
        self.send(CompanyEvent::Message { message: message.clone() });
        if let Ok(json) = serde_json::to_string(&message) {
            self.save(Op::Message { n: message.n, json });
        }
        self.messages.push(message.clone());
        self.trim();
        message
    }

    /// Withdraw what bots posted from `errand` (a forget reached it). Answers the
    /// texts that were there, for the forget's scan, and how many.
    pub(crate) fn withdraw_errand(&mut self, errand: &str, text: &str) -> usize {
        let mut ns = Vec::new();
        for m in self.messages.iter_mut().filter(|m| m.bot && m.errand.as_deref() == Some(errand) && m.text != text) {
            m.text = text.to_string();
            ns.push(m.n);
        }
        for m in self.messages.iter().filter(|m| ns.contains(&m.n)) {
            if let Ok(json) = serde_json::to_string(m) {
                self.save(Op::Message { n: m.n, json });
            }
        }
        let count = ns.len();
        if count > 0 {
            self.send(CompanyEvent::Withdrawn { ns, text: text.to_string() });
        }
        count
    }

    /// Every message's text, for a forget's scan of what the office still holds.
    pub(crate) fn message_texts(&self) -> String {
        self.messages.iter().map(|m| m.text.as_str()).collect::<Vec<_>>().join("\n")
    }

    /// Where a message may go, and its text, or why not.
    fn check_message(&self, from: &Member, to: &str, text: &str) -> Result<(String, String), String> {
        let text = text.trim();
        if text.is_empty() || text.chars().count() > MAX_MESSAGE_CHARS {
            return Err(format!("a message is 1 to {MAX_MESSAGE_CHARS} characters"));
        }
        let to = to.trim().to_lowercase();
        let ok = match to.strip_prefix('@') {
            Some(id) => id != from.id && self.members.iter().any(|m| m.id == id),
            None => self.channels().contains(&to),
        };
        if !ok {
            return Err(format!("there is no channel or person \"{to}\" here"));
        }
        Ok((to, text.to_string()))
    }

    fn join(&mut self, name: &str, area: &str, preset: Option<&str>, invite: Option<&str>) -> Result<(Member, String), String> {
        if !self.members.is_empty() && !invite.is_some_and(|i| crate::same(i.trim().as_bytes(), self.invite.as_bytes())) {
            return Err("ask the owner for the invite code to join this office".into());
        }
        let bot_role = match preset.map(str::trim).filter(|p| !p.is_empty()) {
            Some(p) => Some(bot_role(p).ok_or_else(|| format!("there is no preset agent \"{p}\""))?),
            None => None,
        };
        let name = clean(name, "your name", 40)?;
        if name.is_empty() {
            return Err("say your name to join".into());
        }
        let area = clean(area, "the area", 40)?;
        if self.members.len() >= MAX_MEMBERS {
            return Err(format!("the office is full ({MAX_MEMBERS} people)"));
        }
        if self.members.iter().any(|m| m.name.eq_ignore_ascii_case(&name)) {
            return Err(format!("someone called {name} is already here"));
        }
        // Bots and colors not in use, in order.
        let bot = BOTS.iter().find(|b| !self.members.iter().any(|m| m.bot == **b)).copied().unwrap_or(BOTS[0]);
        let color = COLORS.iter().find(|c| !self.members.iter().any(|m| m.color == **c)).copied().unwrap_or(COLORS[self.joined % COLORS.len()]);
        self.joined += 1;
        let key = agora_engine::random_hex(24);
        let member = Member {
            id: format!("m{}", self.joined),
            name,
            role: if self.members.is_empty() { Role::Owner } else { Role::Member },
            area,
            color: color.to_string(),
            bot: bot.to_string(),
            bot_role,
            online: false,
            on_cli: false,
            key_hash: hash_key(&key),
            streams: 0,
        };
        self.members.push(member.clone());
        self.save_member(&member);
        self.save(Op::Kv { key: "joined".into(), value: self.joined.to_string() });
        self.send(CompanyEvent::Member { member: member.clone() });
        let role = if member.role == Role::Owner { " as the owner" } else { "" };
        self.note(&member.id, format!("{} joined{role}; their bot {} is at its desk", member.name, member.bot), None);
        Ok((member, key))
    }

    /// Apply a change if all of it is allowed; otherwise change nothing.
    fn change(&mut self, me: &Member, id: &str, change: &Change) -> Result<Member, (StatusCode, String)> {
        let refuse = |status, why: &str| Err((status, why.to_string()));
        let owner = me.role == Role::Owner;
        if !owner && (me.id != id || change.role.is_some()) {
            return refuse(StatusCode::FORBIDDEN, "only the owner can change someone's role, or anyone else's area");
        }
        if change.role == Some(Role::Owner) {
            return refuse(StatusCode::BAD_REQUEST, "there is one owner");
        }
        let Some(current) = self.members.iter().find(|m| m.id == id).cloned() else { return refuse(StatusCode::NOT_FOUND, "no such member") };
        if current.role == Role::Owner && change.role.is_some() {
            return refuse(StatusCode::BAD_REQUEST, "the owner stays the owner");
        }
        let area = match change.area.as_deref().map(|a| clean(a, "the area", 40)).transpose() {
            Ok(a) => a.unwrap_or_else(|| current.area.clone()),
            Err(why) => return refuse(StatusCode::BAD_REQUEST, &why),
        };
        let new_bot = match change.bot_role.as_deref().map(str::trim) {
            None => current.bot_role.clone(),
            Some("") => None,
            Some(preset) => match bot_role(preset) {
                Some(r) => Some(r),
                None => return refuse(StatusCode::BAD_REQUEST, &format!("there is no preset agent \"{preset}\"")),
            },
        };
        let role = change.role.unwrap_or(current.role);
        // One lead per area: errands for an area must have one place to go.
        if role != Role::Member
            && !area.is_empty()
            && let Some(other) = self.members.iter().find(|m| m.id != id && m.role != Role::Member && m.area.eq_ignore_ascii_case(&area))
        {
            return Err((StatusCode::CONFLICT, format!("{} already leads {}", other.name, other.area)));
        }
        let Some(target) = self.members.iter_mut().find(|m| m.id == id) else { return refuse(StatusCode::NOT_FOUND, "no such member") };
        target.role = role;
        target.area = area;
        target.bot_role = new_bot;
        let member = target.clone();
        self.save_member(&member);
        self.send(CompanyEvent::Member { member: member.clone() });
        if let Some(r) = member.bot_role.as_ref().filter(|_| change.bot_role.is_some()) {
            let text = format!("{}'s bot {} now works as a {} ({} skill{})", member.name, member.bot, r.name, r.kit.len(), if r.kit.len() == 1 { "" } else { "s" });
            self.note(&member.id, text, None);
        }
        if change.role == Some(Role::Lead) && !member.area.is_empty() {
            let text = format!("{} now leads {}: errands for {} go to their bot {}", member.name, member.area, member.area, member.bot);
            self.note(&member.id, text, None);
        }
        Ok(member)
    }

    fn leave(&mut self, id: &str) -> Option<Member> {
        let at = self.members.iter().position(|m| m.id == id)?;
        let gone = self.members.remove(at);
        self.save(Op::RemoveMember { id: gone.id.clone() });
        self.send(CompanyEvent::Left { id: gone.id.clone() });
        self.note(&gone.id, format!("{} left", gone.name), None);
        // Someone must own the office: the longest-standing member takes over.
        if gone.role == Role::Owner
            && let Some(next) = self.members.first_mut()
        {
            next.role = Role::Owner;
            let member = next.clone();
            self.save_member(&member);
            self.send(CompanyEvent::Member { member });
        }
        Some(gone)
    }

    /// `id` is working from a CLI (or, `false`, has no CLI key left).
    pub(crate) fn set_on_cli(&mut self, id: &str, on: bool) {
        if let Some(m) = self.members.iter_mut().find(|m| m.id == id)
            && m.on_cli != on
        {
            m.on_cli = on;
            let member = m.clone();
            self.send(CompanyEvent::Member { member });
        }
    }

    fn set_online(&mut self, id: &str, delta: isize) {
        if let Some(m) = self.members.iter_mut().find(|m| m.id == id) {
            m.streams = m.streams.saturating_add_signed(delta);
            let online = m.streams > 0;
            if online != m.online {
                m.online = online;
                let member = m.clone();
                self.send(CompanyEvent::Member { member });
            }
        }
    }
}

impl Hub {
    pub(crate) fn company(&self) -> std::sync::MutexGuard<'_, Company> {
        self.company.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The member acting on this request: by their CLI key, their key header, or the
    /// session cookie.
    pub(crate) fn member(&self, headers: &HeaderMap) -> Option<Member> {
        if let Some(key) = crate::cli::key_of(headers) {
            let id = self.cli_keys().member_of(key)?;
            let mut company = self.company();
            company.set_on_cli(&id, true);
            return company.by_id(&id).cloned();
        }
        let key = headers.get(MEMBER_HEADER).and_then(|v| v.to_str().ok()).map(str::to_string).or_else(|| crate::cookie(headers, MEMBER_COOKIE)).unwrap_or_default();
        self.company().by_key(&key).cloned()
    }
}

#[derive(Deserialize)]
pub(crate) struct Join {
    name: String,
    #[serde(default)]
    area: String,
    /// The preset agent their bot plays, "<team>/<agent>".
    #[serde(default)]
    bot_role: Option<String>,
    /// The owner's invite code (needed by everyone after the first).
    #[serde(default)]
    invite: Option<String>,
}

#[derive(Serialize)]
struct Joined {
    member: Member,
    key: String,
}

pub(crate) async fn join(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Result<Json<Join>, JsonRejection>) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("that isn't a valid join: {}", e.body_text())),
    };
    let mut company = hub.company();
    match company.join(&body.name, &body.area, body.bot_role.as_deref(), body.invite.as_deref()) {
        Ok((member, key)) => (StatusCode::CREATED, Json(Joined { member, key })).into_response(),
        Err(why) if why.contains("invite") => error(StatusCode::FORBIDDEN, &why),
        Err(why) => error(StatusCode::CONFLICT, &why),
    }
}

#[derive(Deserialize)]
pub(crate) struct Change {
    #[serde(default)]
    role: Option<Role>,
    #[serde(default)]
    area: Option<String>,
    /// A preset agent for their bot ("<team>/<agent>"), or "" for a plain assistant.
    #[serde(default)]
    bot_role: Option<String>,
}

pub(crate) async fn change(State(hub): State<Arc<Hub>>, Path(id): Path<String>, headers: HeaderMap, body: Result<Json<Change>, JsonRejection>) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("that isn't a valid change: {}", e.body_text())),
    };
    let Some(me) = hub.member(&headers) else { return error(StatusCode::FORBIDDEN, "join the office first") };
    match hub.company().change(&me, &id, &body) {
        Ok(member) => Json(member).into_response(),
        Err((status, why)) => error(status, &why),
    }
}

pub(crate) async fn leave(State(hub): State<Arc<Hub>>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let Some(me) = hub.member(&headers) else { return error(StatusCode::FORBIDDEN, "join the office first") };
    if me.id != id && me.role != Role::Owner {
        return error(StatusCode::FORBIDDEN, "only the owner can remove someone else");
    }
    let mut company = hub.company();
    match company.leave(&id) {
        Some(_) => {
            // Someone removed by the owner shouldn't walk back in with the old invite.
            if me.id != id {
                company.rotate_invite();
            }
            drop(company);
            hub.cli_keys().drop_member(&id); // their CLI keys go with them
            StatusCode::NO_CONTENT.into_response()
        }
        None => error(StatusCode::NOT_FOUND, "no such member"),
    }
}

/// What a member sees about themselves.
#[derive(Serialize)]
pub(crate) struct Me {
    member: Member,
    channels: Vec<String>,
    /// The company's call room (a Jitsi Meet room name only members are told).
    call_room: String,
    /// The invite code, for the owner to hand out.
    #[serde(skip_serializing_if = "Option::is_none")]
    invite: Option<String>,
}

/// Who the key (header or session cookie) belongs to.
/// The company's routes.
pub(crate) fn routes() -> axum::Router<Arc<Hub>> {
    use axum::routing::{get, patch, post};
    axum::Router::new()
        .route("/api/company/join", post(join))
        .route("/api/company/members/{id}", patch(change).delete(leave))
        .route("/api/company/events", get(events))
        .route("/api/company/whoami", get(whoami))
        .route("/api/company/messages", post(say))
}

pub(crate) async fn whoami(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let Some(member) = hub.member(&headers) else { return error(StatusCode::NOT_FOUND, "no one in the office has that key") };
    let company = hub.company();
    let invite = (member.role == Role::Owner).then(|| company.invite().to_string());
    Json(Me { channels: company.channels(), call_room: company.call_room().to_string(), invite, member }).into_response()
}

#[derive(Deserialize)]
pub(crate) struct Say {
    to: String,
    text: String,
}

/// `@Bot ...` at the start of a channel message: the bot's member and the errand.
/// The bot it addresses and the errand: a person's bot (its owner), or an added agent
/// (as its owner, under the agent's name, with the agent).
fn mention<'a>(company: &Company, agents: &[crate::agents::AgentBot], me: &Member, text: &'a str) -> Option<(Member, Option<crate::agents::AgentBot>, &'a str)> {
    let rest = text.strip_prefix('@')?;
    let end = rest.find(|c: char| !c.is_alphanumeric()).unwrap_or(rest.len());
    let name = &rest[..end];
    let brief = rest[end..].trim_start_matches([',', ':', ' ']).trim();
    if brief.is_empty() {
        return None;
    }
    if let Some(owner) = company.by_bot(name) {
        return Some((owner.clone(), None, brief));
    }
    let agent = agents.iter().find(|a| a.name.eq_ignore_ascii_case(name))?;
    let mut speaker = agent.owner.as_deref().and_then(|id| company.by_id(id)).unwrap_or(me).clone();
    speaker.bot.clone_from(&agent.name);
    Some((speaker, Some(agent.clone()), brief))
}

pub(crate) async fn say(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Result<Json<Say>, JsonRejection>) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("that isn't a valid message: {}", e.body_text())),
    };
    let Some(me) = hub.member(&headers) else { return error(StatusCode::FORBIDDEN, "join the office first") };
    let agents = hub.agents().all(); // before the company's lock: never both at once
    let (said, errand) = {
        let mut company = hub.company();
        let (to, text) = match company.check_message(&me, &body.to, &body.text) {
            Ok(ok) => ok,
            Err(why) => return error(StatusCode::BAD_REQUEST, &why),
        };
        let errand = if to.starts_with('#') { mention(&company, &agents, &me, &text).map(|(bot, agent, brief)| (bot, agent, brief.to_string(), to.clone())) } else { None };
        (company.say(&me, false, &to, text, None), errand)
    };
    // A bot asked in a channel gets the errand; its checked report comes back there.
    if let Some((bot, agent_bot, brief, channel)) = errand {
        // An agent with no owner yet is answered by whoever asked.
        let owner = agent_bot.as_ref().is_none_or(|a| a.owner.is_some()).then(|| bot.clone());
        let begin = crate::errands::Begin { brief, bot: owner, agent_bot, reply_to: Some(channel.clone()), ..Default::default() };
        let started = crate::errands::begin(&hub, Some(me.clone()), begin).await;
        let mut company = hub.company();
        match started {
            Ok(id) => {
                company.say(&bot, true, &channel, format!("On it, {}. I'll report back here, checked against my ledger.", me.name), Some(id));
            }
            Err((_, why)) => {
                company.say(&bot, true, &channel, format!("I can't take that on right now: {why}"), None);
            }
        }
    }
    (StatusCode::CREATED, Json(said)).into_response()
}

/// Marks a member online while their stream is open.
struct Presence {
    hub: Arc<Hub>,
    id: Option<String>,
}

impl Drop for Presence {
    fn drop(&mut self) {
        if let Some(id) = &self.id {
            self.hub.company().set_online(id, -1);
        }
    }
}

/// Whether a live event goes to the stream of `member`.
fn for_stream(event: &CompanyEvent, member: Option<&str>) -> bool {
    match event {
        CompanyEvent::Message { message } => message.visible_to(member),
        _ => true,
    }
}

pub(crate) async fn events(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let (snapshot, live, id) = {
        let me = hub.member(&headers);
        let mut company = hub.company();
        let id = me.map(|m| m.id);
        if let Some(id) = &id {
            company.set_online(id, 1);
        }
        let visible: Vec<Message> = company.messages.iter().filter(|m| m.visible_to(id.as_deref())).cloned().collect();
        let messages = visible[visible.len().saturating_sub(SNAPSHOT_MESSAGES)..].to_vec();
        let snapshot = CompanyEvent::Snapshot { members: company.members.clone(), activity: company.activity.clone(), messages };
        (snapshot, company.live.subscribe(), id)
    };
    let presence = Presence { hub: Arc::clone(&hub), id };
    let past = stream::iter([snapshot]).filter_map(|e| async move { to_sse(&e) });
    let live = stream::unfold((live, presence), |(mut rx, presence)| async move {
        loop {
            match rx.recv().await {
                Ok(e) if for_stream(&e, presence.id.as_deref()) => return Some((e, (rx, presence))),
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
    .filter_map(|e| async move { to_sse(&e) });
    Sse::new(past.chain(live)).keep_alive(KeepAlive::default()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(c: &mut Company, name: &str, area: &str) -> (Member, String) {
        let invite = c.invite.clone();
        c.join(name, area, None, Some(&invite)).unwrap()
    }

    #[test]
    fn the_first_in_owns_the_office_and_the_rest_need_the_invite() {
        let mut c = Company::default();
        let (asha, asha_key) = c.join("Asha", "", None, None).unwrap();
        assert!(c.join("Priya", "Finance", None, None).unwrap_err().contains("invite"));
        assert!(c.join("Priya", "Finance", None, Some("wrong")).unwrap_err().contains("invite"));
        let (priya, priya_key) = join(&mut c, "Priya", "Finance");
        assert_eq!((asha.role, priya.role), (Role::Owner, Role::Member));
        assert_eq!((asha.bot.as_str(), priya.bot.as_str()), ("Scout", "Atlas"));
        assert_ne!(asha_key, priya_key);
        assert_eq!(asha_key.len(), 48);
        assert!(c.join("priya", "", None, Some(&c.invite.clone())).is_err(), "names are unique, ignoring case");
        assert!(c.join("Bad@name", "", None, Some(&c.invite.clone())).is_err());
        assert!(c.by_key(&priya_key).is_some_and(|m| m.id == priya.id));
        assert!(c.by_key(&priya.key_hash).is_none(), "the stored hash is not a key");
        assert!(c.by_key("").is_none());
    }

    #[test]
    fn errands_for_an_area_go_to_its_lead() {
        let mut c = Company::default();
        let (asha, _) = c.join("Asha", "", None, None).unwrap();
        let (priya, _) = join(&mut c, "Priya", "Finance");
        assert!(c.lead_of("finance").is_none(), "working in an area isn't leading it");
        c.change(&asha, &priya.id, &Change { role: Some(Role::Lead), area: None, bot_role: None }).unwrap();
        assert_eq!(c.lead_of(" Finance ").map(|m| m.bot.as_str()), Some("Atlas"));
        assert_eq!(c.channels(), ["#general", "#finance"]);
    }

    #[test]
    fn a_change_that_is_partly_refused_changes_nothing() {
        let mut c = Company::default();
        let (asha, _) = c.join("Asha", "", None, None).unwrap();
        let (priya, _) = join(&mut c, "Priya", "Finance");
        let bad = Change { role: Some(Role::Lead), area: Some("Ops".into()), bot_role: Some("research-lab/nobody".into()) };
        assert_eq!(c.change(&asha, &priya.id, &bad).unwrap_err().0, StatusCode::BAD_REQUEST);
        let now = c.by_id(&priya.id).unwrap();
        assert_eq!((now.role, now.area.as_str()), (Role::Member, "Finance"));
        let theirs = Change { role: None, area: Some("Ops".into()), bot_role: None };
        assert_eq!(c.change(&priya, &asha.id, &theirs).unwrap_err().0, StatusCode::FORBIDDEN);
    }

    #[test]
    fn a_bot_can_play_a_preset_agent_with_its_skills() {
        let mut c = Company::default();
        let (m, _) = c.join("Asha", "", Some("research-lab/fact-checker"), None).unwrap();
        let role = m.bot_role.as_ref().unwrap();
        assert_eq!(role.name, "Fact-Checker");
        assert_ne!(role.kit.len(), 0);
        assert!(m.persona().contains("Fact-Checker") && m.persona().contains("Your skills"));
        assert!(c.join("Priya", "", Some("research-lab/nobody"), Some(&c.invite.clone())).is_err());
    }

    #[test]
    fn the_office_holds_at_most_twenty_four() {
        let mut c = Company::default();
        c.join("P0", "", None, None).unwrap();
        for i in 1..MAX_MEMBERS {
            join(&mut c, &format!("P{i}"), "");
        }
        assert!(c.join("One more", "", None, Some(&c.invite.clone())).unwrap_err().contains("full"));
        let bots: std::collections::HashSet<&str> = c.members.iter().map(|m| m.bot.as_str()).collect();
        assert_eq!(bots.len(), MAX_MEMBERS, "every bot has its own name");
    }

    #[test]
    fn direct_messages_are_for_the_two_people_in_them() {
        let mut c = Company::default();
        let (asha, _) = c.join("Asha", "", None, None).unwrap();
        let (priya, _) = join(&mut c, "Priya", "Finance");
        let (dev, _) = join(&mut c, "Dev", "");
        let (to, text) = c.check_message(&asha, "@m2", " hi ").unwrap();
        let dm = c.say(&asha, false, &to, text, None);
        assert!(dm.visible_to(Some(&priya.id)) && dm.visible_to(Some(&asha.id)));
        assert!(!dm.visible_to(Some(&dev.id)) && !dm.visible_to(None));
        let (to, text) = c.check_message(&dev, "#Finance", "numbers?").unwrap();
        assert!(c.say(&dev, false, &to, text, None).visible_to(Some(&asha.id)));
        assert!(c.check_message(&dev, "#nowhere", "x").is_err());
        assert!(c.check_message(&dev, &format!("@{}", dev.id), "x").is_err(), "not to yourself");
        assert!(c.check_message(&dev, "#general", "  ").is_err());
    }

    #[test]
    fn a_bot_is_asked_by_name_at_the_start_of_a_message() {
        let mut c = Company::default();
        c.join("Asha", "", None, None).unwrap();
        let asha = c.members_view()[0].clone();
        let muse = crate::agents::AgentBot {
            id: "agent-1".into(), name: "Muse".into(), owner: Some(asha.id.clone()), owner_name: Some("Asha".into()),
            preset: None, model: None, tools: vec![], color: "#8b5cf6".into(), at_ms: 0,
        };
        let agents = [muse];
        let (bot, agent, brief) = mention(&c, &agents, &asha, "@scout, text Dev about Wednesday").unwrap();
        assert_eq!((bot.bot.as_str(), agent.is_none(), brief), ("Scout", true, "text Dev about Wednesday"));
        // An added agent speaks under its own name, as its owner.
        let (bot, agent, _) = mention(&c, &agents, &asha, "@muse check the docs").unwrap();
        assert_eq!((bot.bot.as_str(), bot.id.as_str(), agent.map(|a| a.name)), ("Muse", asha.id.as_str(), Some("Muse".to_string())));
        assert!(mention(&c, &agents, &asha, "@Nobody do it").is_none());
        assert!(mention(&c, &agents, &asha, "@Scout").is_none(), "no errand without a brief");
        assert!(mention(&c, &agents, &asha, "hey @Scout do it").is_none());
    }

    #[test]
    fn the_office_comes_back_from_its_store() {
        let mut c = Company::default();
        let (asha, key) = c.join("Asha", "Ops", Some("research-lab/fact-checker"), None).unwrap();
        let (to, text) = c.check_message(&asha, "#general", "hello").unwrap();
        c.say(&asha, false, &to, text, None);
        let loaded = Loaded {
            kv: vec![("invite".into(), c.invite.clone()), ("call_room".into(), c.call_room.clone()), ("joined".into(), "1".into())],
            members: c.members.iter().map(|m| serde_json::to_string(&m.stored()).unwrap()).collect(),
            activity: c.activity.iter().map(|a| serde_json::to_string(a).unwrap()).collect(),
            messages: c.messages.iter().map(|m| serde_json::to_string(m).unwrap()).collect(),
            errands: Vec::new(),
            pruned: Vec::new(),
        };
        let back = Company::new(None, &loaded);
        let m = back.by_key(&key).unwrap();
        assert_eq!((m.name.as_str(), m.role, m.bot_role.as_ref().map(|r| r.name.as_str())), ("Asha", Role::Owner, Some("Fact-Checker")));
        assert_eq!((back.invite.as_str(), back.messages.len(), back.said), (c.invite.as_str(), 1, 1));
    }
}
