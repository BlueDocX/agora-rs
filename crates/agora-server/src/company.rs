//! The company: the people in the office, each with a bot working under them.
//!
//! - `POST /api/company/join {"name", "area"?}` joins (at most [`MAX_MEMBERS`]). The
//!   first person in is the owner. The reply holds the member and a private `key`:
//!   sent as `X-Agora-Member`, it says who is acting (starting errands, editing).
//! - `PATCH /api/company/members/{id} {"role"?, "area"?}`: the owner makes someone a
//!   lead of an area (or a member again); anyone may change their own area.
//! - `DELETE /api/company/members/{id}`: leave (yourself), or the owner removes someone.
//! - `GET /api/company/whoami` (with the key header) says who the key belongs to.
//! - `GET /api/company/events?member=<key>` streams the company: a snapshot, then
//!   joins, changes, leaves and activity (who asked whose bot to do what, and how its
//!   account checked out). A stream opened with a key marks that person online.
//!
//! An errand can be sent to an area: it runs on the bot of that area's lead.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::{Hub, authorized, error, to_sse, unauthorized};

pub const MAX_MEMBERS: usize = 8;
const KEEP_ACTIVITY: usize = 60;
const MEMBER_HEADER: &str = "x-agora-member";
/// Bots are named in join order.
const BOTS: [&str; MAX_MEMBERS] = ["Scout", "Atlas", "Nova", "Echo", "Pixel", "Sage", "Orbit", "Quill"];
/// One color per member, as in the office (butter and purple stay the Lead's and reviewer's).
const COLORS: [&str; MAX_MEMBERS] = ["#4285f4", "#34a853", "#ea4335", "#14b8a6", "#f97316", "#ec4899", "#64748b", "#a16207"];

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
    pub online: bool,
    #[serde(skip)]
    key: String,
    #[serde(skip)]
    streams: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Activity {
    pub at_ms: u64,
    /// The member it is about.
    pub member: String,
    pub text: String,
    pub errand: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum CompanyEvent {
    Snapshot { members: Vec<Member>, activity: Vec<Activity> },
    Member { member: Member },
    Left { id: String },
    Activity { activity: Activity },
}

pub(crate) struct Company {
    members: Vec<Member>,
    activity: Vec<Activity>,
    live: broadcast::Sender<CompanyEvent>,
    joined: usize,
}

impl Default for Company {
    fn default() -> Self {
        Self { members: Vec::new(), activity: Vec::new(), live: broadcast::channel(256).0, joined: 0 }
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A random-enough secret from the clock and the process's hasher seed.
fn secret() -> String {
    use std::hash::{BuildHasher, Hasher};
    (0..4).fold(String::new(), |mut s, i| {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()) + i);
        let _ = write!(s, "{:016x}", h.finish());
        s
    })
}

fn clean(s: &str, what: &str, max: usize) -> Result<String, String> {
    let s = s.trim();
    if s.chars().count() > max || s.chars().any(char::is_control) || s.contains('@') {
        return Err(format!("{what} must be at most {max} characters on one line, without @"));
    }
    Ok(s.to_string())
}

impl Company {
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
        (!key.is_empty()).then(|| self.members.iter().find(|m| crate::same(m.key.as_bytes(), key.as_bytes()))).flatten()
    }

    /// The lead (or owner) looking after `area`, if anyone is.
    pub(crate) fn lead_of(&self, area: &str) -> Option<&Member> {
        let area = area.trim();
        self.members.iter().find(|m| m.role != Role::Member && !m.area.is_empty() && m.area.eq_ignore_ascii_case(area))
    }

    pub(crate) fn note(&mut self, member: &str, text: String, errand: Option<String>) {
        let activity = Activity { at_ms: now_ms(), member: member.to_string(), text, errand };
        self.send(CompanyEvent::Activity { activity: activity.clone() });
        self.activity.push(activity);
        if self.activity.len() > KEEP_ACTIVITY {
            let extra = self.activity.len() - KEEP_ACTIVITY;
            self.activity.drain(..extra);
        }
    }

    fn join(&mut self, name: &str, area: &str) -> Result<Member, String> {
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
        let bot = BOTS.iter().find(|b| !self.members.iter().any(|m| m.bot == **b)).copied().unwrap_or("Scout");
        let color = COLORS.iter().find(|c| !self.members.iter().any(|m| m.color == **c)).copied().unwrap_or(COLORS[0]);
        self.joined += 1;
        let member = Member {
            id: format!("m{}", self.joined),
            name,
            role: if self.members.is_empty() { Role::Owner } else { Role::Member },
            area,
            color: color.to_string(),
            bot: bot.to_string(),
            online: false,
            key: secret(),
            streams: 0,
        };
        self.members.push(member.clone());
        self.send(CompanyEvent::Member { member: member.clone() });
        let role = if member.role == Role::Owner { " as the owner" } else { "" };
        self.note(&member.id, format!("{} joined{role}; their bot {} is at its desk", member.name, member.bot), None);
        Ok(member)
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

    /// The member acting on this request, by their key header.
    pub(crate) fn member(&self, headers: &HeaderMap) -> Option<Member> {
        let key = headers.get(MEMBER_HEADER).and_then(|v| v.to_str().ok()).unwrap_or_default();
        self.company().by_key(key).cloned()
    }
}

#[derive(Deserialize)]
pub(crate) struct Join {
    name: String,
    #[serde(default)]
    area: String,
}

#[derive(Serialize)]
struct Joined {
    member: Member,
    key: String,
}

pub(crate) async fn join(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Result<Json<Join>, JsonRejection>) -> Response {
    if !authorized(&hub, &headers, None) {
        return unauthorized();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("that isn't a valid join: {}", e.body_text())),
    };
    let mut company = hub.company();
    match company.join(&body.name, &body.area) {
        Ok(member) => {
            let key = member.key.clone();
            (StatusCode::CREATED, Json(Joined { member, key })).into_response()
        }
        Err(why) => error(StatusCode::CONFLICT, &why),
    }
}

#[derive(Deserialize)]
pub(crate) struct Change {
    #[serde(default)]
    role: Option<Role>,
    #[serde(default)]
    area: Option<String>,
}

pub(crate) async fn change(State(hub): State<Arc<Hub>>, Path(id): Path<String>, headers: HeaderMap, body: Result<Json<Change>, JsonRejection>) -> Response {
    if !authorized(&hub, &headers, None) {
        return unauthorized();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("that isn't a valid change: {}", e.body_text())),
    };
    let Some(me) = hub.member(&headers) else { return error(StatusCode::FORBIDDEN, "join the office first") };
    let mut company = hub.company();
    let area = match body.area.as_deref().map(|a| clean(a, "the area", 40)).transpose() {
        Ok(a) => a,
        Err(why) => return error(StatusCode::BAD_REQUEST, &why),
    };
    let owner = me.role == Role::Owner;
    if !owner && (me.id != id || body.role.is_some()) {
        return error(StatusCode::FORBIDDEN, "only the owner can change someone's role, or anyone else's area");
    }
    if body.role == Some(Role::Owner) {
        return error(StatusCode::BAD_REQUEST, "there is one owner");
    }
    // One lead per area: errands for an area must have one place to go.
    if let Some(current) = company.members.iter().find(|m| m.id == id) {
        let role = body.role.unwrap_or(current.role);
        let new_area = area.clone().unwrap_or_else(|| current.area.clone());
        if role != Role::Member
            && !new_area.is_empty()
            && let Some(other) = company.members.iter().find(|m| m.id != id && m.role != Role::Member && m.area.eq_ignore_ascii_case(&new_area))
        {
            return error(StatusCode::CONFLICT, &format!("{} already leads {}", other.name, other.area));
        }
    }
    let Some(target) = company.members.iter_mut().find(|m| m.id == id) else { return error(StatusCode::NOT_FOUND, "no such member") };
    if target.role == Role::Owner && body.role.is_some() {
        return error(StatusCode::BAD_REQUEST, "the owner stays the owner");
    }
    if let Some(role) = body.role {
        target.role = role;
    }
    if let Some(area) = area {
        target.area = area;
    }
    let member = target.clone();
    company.send(CompanyEvent::Member { member: member.clone() });
    if member.role == Role::Lead && !member.area.is_empty() {
        let text = format!("{} now leads {}: errands for {} go to their bot {}", member.name, member.area, member.area, member.bot);
        company.note(&member.id, text, None);
    }
    Json(member).into_response()
}

pub(crate) async fn leave(State(hub): State<Arc<Hub>>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers, None) {
        return unauthorized();
    }
    let Some(me) = hub.member(&headers) else { return error(StatusCode::FORBIDDEN, "join the office first") };
    if me.id != id && me.role != Role::Owner {
        return error(StatusCode::FORBIDDEN, "only the owner can remove someone else");
    }
    let mut company = hub.company();
    let Some(at) = company.members.iter().position(|m| m.id == id) else { return error(StatusCode::NOT_FOUND, "no such member") };
    let gone = company.members.remove(at);
    company.send(CompanyEvent::Left { id: gone.id.clone() });
    company.note(&gone.id, format!("{} left", gone.name), None);
    // Someone must own the office: the longest-standing member takes over.
    if gone.role == Role::Owner
        && let Some(next) = company.members.first_mut()
    {
        next.role = Role::Owner;
        let member = next.clone();
        company.send(CompanyEvent::Member { member });
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Who the `X-Agora-Member` key belongs to (a device handed a key finds out here).
pub(crate) async fn whoami(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers, None) {
        return unauthorized();
    }
    match hub.member(&headers) {
        Some(member) => Json(member).into_response(),
        None => error(StatusCode::NOT_FOUND, "no one in the office has that key"),
    }
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

pub(crate) async fn events(State(hub): State<Arc<Hub>>, headers: HeaderMap, Query(query): Query<HashMap<String, String>>) -> Response {
    if !authorized(&hub, &headers, query.get("token").map(String::as_str)) {
        return unauthorized();
    }
    let (snapshot, live, id) = {
        let mut company = hub.company();
        let id = query.get("member").and_then(|k| company.by_key(k)).map(|m| m.id.clone());
        if let Some(id) = &id {
            company.set_online(id, 1);
        }
        let snapshot = CompanyEvent::Snapshot { members: company.members.clone(), activity: company.activity.clone() };
        (snapshot, company.live.subscribe(), id)
    };
    let presence = Presence { hub: Arc::clone(&hub), id };
    let past = stream::iter([snapshot]).filter_map(|e| async move { to_sse(&e) });
    let live = stream::unfold((live, presence), |(mut rx, presence)| async move { rx.recv().await.ok().map(|e| (e, (rx, presence))) })
        .filter_map(|e| async move { to_sse(&e) });
    Sse::new(past.chain(live)).keep_alive(KeepAlive::default()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_in_owns_the_office_and_names_stay_unique() {
        let mut c = Company::default();
        let asha = c.join("Asha", "").unwrap();
        let priya = c.join("Priya", "Finance").unwrap();
        assert_eq!((asha.role, priya.role), (Role::Owner, Role::Member));
        assert_eq!((asha.bot.as_str(), priya.bot.as_str()), ("Scout", "Atlas"));
        assert_ne!(asha.key, priya.key);
        assert!(c.join("priya", "").is_err(), "names are unique, ignoring case");
        assert!(c.join("Bad@name", "").is_err());
        assert!(c.by_key(&priya.key).is_some_and(|m| m.id == priya.id));
        assert!(c.by_key("").is_none());
    }

    #[test]
    fn errands_for_an_area_go_to_its_lead() {
        let mut c = Company::default();
        c.join("Asha", "").unwrap();
        let priya = c.join("Priya", "Finance").unwrap();
        assert!(c.lead_of("finance").is_none(), "working in an area isn't leading it");
        c.members.iter_mut().find(|m| m.id == priya.id).unwrap().role = Role::Lead;
        assert_eq!(c.lead_of(" Finance ").map(|m| m.bot.as_str()), Some("Atlas"));
    }

    #[test]
    fn the_office_holds_at_most_eight() {
        let mut c = Company::default();
        for i in 0..MAX_MEMBERS {
            c.join(&format!("P{i}"), "").unwrap();
        }
        assert!(c.join("One more", "").unwrap_err().contains("full"));
    }
}
