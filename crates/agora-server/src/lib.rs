//! The room hub: start a run over HTTP and watch it live.
//!
//! - `POST /api/runs {"brief": "..."}` starts a run. At most `max_active` run at once.
//! - `GET /api/runs` lists runs, newest first (the last [`KEEP_RUNS`] finished ones
//!   are kept in memory; every run is also in the journal).
//! - `GET /api/runs/{id}/events` streams the run as Server-Sent Events: everything
//!   that already happened, then each new event live, ending when the run does.
//! - `GET /api/info` names the default models and the per-run cap, `GET /api/models`
//!   lists the models this office can run on, and `GET /api/teams` the preset teams
//!   (no token needed: nothing secret, and the page shows them before anyone types
//!   a token).
//! - A run can pick its `model` and `reviewer_model`; an agent can name its own `model`.
//! - A run can name a team: `"team": "<preset id>"`, or a whole team of one's own.
//! - `GET /` is the office board.
//!
//! - `/api/chat` is the office chat: see [`chat`](crate::chat).
//! - `/mcp` is the office as an MCP server (for Meta AI Connectors and any MCP
//!   client): see [`mcp`](crate::mcp). `/glasses` is the Ray-Ban Display web app.
//! - `/api/company` is who is in the office and the bot each one has: see
//!   [`company`](crate::company).
//! - `/api/errands` runs errands whose every claim is checked against a ledger: see
//!   [`errands`](crate::errands).
//! - `/api/transcribe` turns a spoken errand into text with Muse Voice Transcribe:
//!   see [`voice`](crate::voice).
//!
//! When a token is set, every other `/api` call needs it: as `Authorization: Bearer`, or
//! as the `agora_token` cookie that `POST /api/session` sets (a browser `EventSource`
//! can't send headers, and a secret in a URL ends up in logs and history). The
//! cookie is `HttpOnly` and `SameSite=Strict`; a page on another site can't send it,
//! and couldn't send JSON without a preflight this server never allows.
//!
//! Every request must name this server in `Host` (localhost, or one of
//! `HubConfig::hosts`): that stops DNS rebinding, where a page on another site
//! points its own name at this machine to reach it from the visitor's browser.
//! `GET /healthz` answers `ok` for load balancers.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

mod chat;
mod company;
mod errands;
mod mcp;
mod store;
mod voice;

pub use chat::MAX_CHAT_CHARS;

use agora_core::Team;
use agora_engine::{Budget, Journal, RunConfig, RunEvent, Signer, run};
use agora_muse::{Host, MuseClient, host_of, price_for};
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};

pub const MAX_BRIEF_CHARS: usize = 4000;
/// Finished runs kept in memory for the board; older ones are dropped first.
pub const KEEP_RUNS: usize = 50;
const INDEX: &str = include_str!("../static/index.html");
/// The Ray-Ban Display web app (see the file's own notes) and its wearables manifest.
const GLASSES: &str = include_str!("../static/glasses.html");
const GLASSES_MANIFEST: &str = include_str!("../static/meta-wearables-manifest.json");
const GLASSES_ICON: &[u8] = include_bytes!("../static/glasses-icon.png");
/// The 3D office, built from `web/` with `npm run build`, which writes both files.
/// Both are committed, so the server builds without Node.
const OFFICE_JS: &str = include_str!("../static/office.js");
const OFFICE_JS_GZ: &[u8] = include_bytes!("../static/office.js.gz");
/// The office's people and furniture: Kenney's CC0 packs, trimmed by `web/models/build.mjs`,
/// which writes each model and its gzipped copy together.
const PEOPLE: &[u8] = include_bytes!("../static/models/people.glb");
const PEOPLE_GZ: &[u8] = include_bytes!("../static/models/people.glb.gz");
const FURNITURE: &[u8] = include_bytes!("../static/models/furniture.glb");
const FURNITURE_GZ: &[u8] = include_bytes!("../static/models/furniture.glb.gz");
/// Thumbnails of the eight teammate characters, in office.js's TEAM order, for the
/// team editor (from the Kenney pack's previews).
const CAST_PNG: &[u8] = include_bytes!("../static/models/cast.png");
/// B4's self-report fidelity results (`alibi-spike` writes `bench-out/b4_summary.json`;
/// the rows behind it are in `bench/`).
const LEADERBOARD: &str = include_str!("../static/leaderboard.json");

impl HubConfig {
    /// The client for a model's host (by `agora_muse::host_of`), if this office has it.
    #[must_use]
    pub fn client_for(&self, model: &str) -> Option<&MuseClient> {
        match host_of(model)? {
            Host::Meta => Some(&self.muse),
            Host::Baseten => self.baseten.as_ref(),
        }
    }

    /// A model a run (or an agent) may use here: priced, with its host set up.
    ///
    /// # Errors
    /// Why not, in words a person can act on.
    pub fn check_model(&self, model: &str) -> Result<(), String> {
        if price_for(model).is_none() {
            return Err(format!("\"{model}\" is not a model this office can price, so it can't run here"));
        }
        if self.client_for(model).is_none() {
            return Err(format!("\"{model}\" needs a host this office doesn't have (its API key isn't set)"));
        }
        Ok(())
    }
}

/// How the hub runs briefs.
#[derive(Debug, Clone)]
pub struct HubConfig {
    pub muse: MuseClient,
    pub model: String,
    pub cap_usd: f64,
    pub max_parallel: usize,
    /// The default reviewer model (`None` = `model`). A run can pick others.
    pub reviewer_model: Option<String>,
    /// Baseten, for the open weights. `None` = models it serves are unavailable.
    pub baseten: Option<MuseClient>,
    /// Journal every run here (so it can be resumed from the CLI). `None` = don't.
    pub journal: Option<PathBuf>,
    /// Required as `Authorization: Bearer <token>` to start a run. `None` = open
    /// (only safe on localhost; `main` refuses to listen elsewhere without one).
    pub token: Option<String>,
    /// Runs allowed at the same time; more get 429.
    pub max_active: usize,
    /// What the chat may spend on replies over the server's life (USD).
    pub chat_cap_usd: f64,
    /// What all errands together may spend while the server runs (USD).
    pub errands_cap_usd: f64,
    /// Host names this server answers to besides localhost (e.g. its public name);
    /// `"*"` = any (only behind a proxy that checks it).
    pub hosts: Vec<String>,
    /// Keep the company, its messages and errand streams here across restarts
    /// (errands also need `journal`). `None` = in memory only.
    pub state: Option<PathBuf>,
    /// The office's Ed25519 key for signing ledger heads. `None` = a new key each start.
    pub signing_key: Option<PathBuf>,
}

impl HubConfig {
    /// An office on `muse` with the usual defaults (no token, no files kept).
    #[must_use]
    pub fn new(muse: MuseClient) -> Self {
        Self {
            muse,
            model: "muse-spark-1.3".into(),
            cap_usd: 2.0,
            max_parallel: 4,
            reviewer_model: None,
            baseten: None,
            journal: None,
            token: None,
            max_active: 2,
            chat_cap_usd: 2.0,
            errands_cap_usd: 10.0,
            hosts: Vec::new(),
            state: None,
            signing_key: None,
        }
    }
}

struct Room {
    id: String,
    brief: String,
    /// The team's name, if the run has one.
    team: Option<String>,
    inner: Mutex<RoomInner>,
}

struct RoomInner {
    history: Vec<RunEvent>,
    /// `None` once the run is over, which ends every live stream.
    live: Option<broadcast::Sender<RunEvent>>,
}

impl Room {
    fn lock(&self) -> std::sync::MutexGuard<'_, RoomInner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// History and live updates under one lock, so a watcher never misses or
    /// repeats an event.
    fn push(&self, event: RunEvent) {
        let mut inner = self.lock();
        if let Some(live) = &inner.live {
            let _ = live.send(event.clone()); // no watchers is fine
        }
        inner.history.push(event);
    }

    fn close(&self) {
        self.lock().live = None;
    }

    fn subscribe(&self) -> (Vec<RunEvent>, Option<broadcast::Receiver<RunEvent>>) {
        let inner = self.lock();
        (inner.history.clone(), inner.live.as_ref().map(broadcast::Sender::subscribe))
    }

    fn done(&self) -> bool {
        self.lock().live.is_none()
    }

    fn history(&self) -> Vec<RunEvent> {
        self.lock().history.clone()
    }
}

struct Hub {
    config: HubConfig,
    state: Mutex<HubState>,
    chat: Mutex<chat::ChatState>,
    /// `None` when the chat can't be budgeted (an unpriced model or a bad cap).
    chat_budget: Option<Arc<Budget>>,
    errands: Mutex<errands::Errands>,
    company: Mutex<company::Company>,
    signer: Arc<Signer>,
    store: Option<store::Store>,
    /// Seconds of audio transcribed while the server runs.
    transcribed_s: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
struct HubState {
    rooms: Vec<Arc<Room>>,
    /// Runs that passed the `max_active` check but whose room isn't added yet;
    /// counted so two requests at the same moment can't both take the last slot.
    starting: usize,
}

impl Hub {
    fn state(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn room(&self, id: &str) -> Option<Arc<Room>> {
        self.state().rooms.iter().find(|r| r.id == id).cloned()
    }
}

pub fn app(config: HubConfig) -> Router {
    // Agents may chat on any model in the catalog, so the chat budget prices them all.
    let priced: Vec<&str> = agora_muse::MODELS.iter().map(|m| m.id).chain([config.model.as_str()]).collect();
    let chat_budget = match Budget::new(&priced, config.chat_cap_usd) {
        Ok(budget) => Some(Arc::new(budget)),
        Err(e) => {
            eprintln!("chat is off: {e}");
            None
        }
    };
    let (store, loaded) = match &config.state {
        Some(path) => match store::Store::open(path, errands::KEEP_ERRANDS) {
            Ok((store, loaded)) => (Some(store), loaded),
            Err(e) => {
                eprintln!("the office can't keep its state in {} ({e}): everything stays in memory", path.display());
                (None, store::Loaded::default())
            }
        },
        None => (None, store::Loaded::default()),
    };
    let signer = match &config.signing_key {
        Some(path) => Signer::load_or_create(path).unwrap_or_else(|e| {
            eprintln!("{e}: signing with a key for this run only");
            Signer::ephemeral()
        }),
        None => Signer::ephemeral(),
    };
    let company = company::Company::new(store.clone(), &loaded);
    let hub = Arc::new(Hub {
        config,
        state: Mutex::new(HubState::default()),
        chat: Mutex::default(),
        chat_budget,
        errands: Mutex::default(),
        company: Mutex::new(company),
        signer: Arc::new(signer),
        store,
        transcribed_s: std::sync::atomic::AtomicU64::new(0),
    });
    let restored = errands::restore(&hub, loaded.errands, &loaded.pruned);
    if restored > 0 {
        eprintln!("brought back {restored} errands");
    }
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/session", post(session).delete(end_session))
        .route("/api/session/member", axum::routing::delete(end_membership))
        .route("/.well-known/agora-signing-key", get(signing_key))
        .route("/", get(index))
        .route("/glasses", get(glasses))
        .route("/.well-known/meta-wearables-manifest.json", get(|| async { ([(header::CONTENT_TYPE, "application/json; charset=utf-8")], GLASSES_MANIFEST) }))
        .route("/glasses-icon.png", get(|h: HeaderMap| async move { asset(&h, "image/png", GLASSES_ICON, None) }))
        .route("/office.js", get(|h: HeaderMap| async move { asset(&h, "text/javascript; charset=utf-8", OFFICE_JS.as_bytes(), Some(OFFICE_JS_GZ)) }))
        .route("/models/people.glb", get(|h: HeaderMap| async move { asset(&h, "model/gltf-binary", PEOPLE, Some(PEOPLE_GZ)) }))
        .route("/models/cast.png", get(|h: HeaderMap| async move { asset(&h, "image/png", CAST_PNG, None) }))
        .route("/models/furniture.glb", get(|h: HeaderMap| async move { asset(&h, "model/gltf-binary", FURNITURE, Some(FURNITURE_GZ)) }))
        .route("/api/info", get(info))
        .route("/api/leaderboard", get(|| async { ([(header::CONTENT_TYPE, "application/json")], LEADERBOARD) }))
        .route("/api/teams", get(|| async { Json(agora_core::presets()) }))
        .route("/api/skills", get(|| async { Json(agora_core::SKILLS) }))
        .route("/api/models", get(models))
        .route("/api/runs", post(start).get(list))
        .route("/api/runs/{id}/events", get(events))
        .route("/api/chat", post(chat::post))
        .route("/api/chat/events", get(chat::events))
        .route("/mcp", post(mcp::post).get(mcp::get))
        .route("/api/company/join", post(company::join))
        .route("/api/company/members/{id}", axum::routing::patch(company::change).delete(company::leave))
        .route("/api/company/events", get(company::events))
        .route("/api/company/whoami", get(company::whoami))
        .route("/api/company/messages", post(company::say))
        .route("/api/errands", post(errands::start).get(errands::list))
        .route("/api/errands/{id}/events", get(errands::events))
        .route("/api/errands/{id}/ask", post(errands::ask))
        .route("/api/errands/{id}/ledger", get(errands::ledger))
        .route("/api/errands/{id}/forget", post(errands::forget))
        .route("/api/errands/{id}/receipt", get(errands::receipt))
        .route("/api/transcribe", post(voice::transcribe).layer(axum::extract::DefaultBodyLimit::max(voice::MAX_BYTES)))
        .layer(middleware::from_fn_with_state(Arc::clone(&hub), guard_host))
        .layer(middleware::from_fn(access_log))
        .with_state(hub)
}

/// The host a request names, without its port.
fn host_name(request: &Request) -> Option<String> {
    let host = request.headers().get(header::HOST).and_then(|v| v.to_str().ok()).map(str::to_string).or_else(|| request.uri().authority().map(ToString::to_string))?;
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default().to_string(),
        None => host.rsplit_once(':').map_or(host.as_str(), |(h, _)| h).to_string(),
    };
    Some(name.to_ascii_lowercase())
}

/// Refuse requests that don't name this server (see the module notes on DNS rebinding).
async fn guard_host(State(hub): State<Arc<Hub>>, request: Request, next: Next) -> Response {
    let hosts = &hub.config.hosts;
    let allowed = hosts.iter().any(|h| h == "*")
        || host_name(&request).is_some_and(|name| matches!(name.as_str(), "localhost" | "127.0.0.1" | "::1") || hosts.iter().any(|h| h.eq_ignore_ascii_case(&name)));
    if allowed { next.run(request).await } else { error(StatusCode::MISDIRECTED_REQUEST, "this office doesn't answer to that host name") }
}

/// One line per request on stderr: method, path (never the query), status, time.
async fn access_log(request: Request, next: Next) -> Response {
    let (method, path) = (request.method().clone(), request.uri().path().to_string());
    let started = std::time::Instant::now();
    let mut response = next.run(request).await;
    // Event streams carry private history: never cache them.
    if response.headers().get(header::CONTENT_TYPE).is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream")) {
        response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    if path != "/healthz" {
        eprintln!("{method} {path} {} {}ms", response.status().as_u16(), started.elapsed().as_millis());
    }
    response
}

/// The office's public signing key (hex), for checking receipts.
async fn signing_key(State(hub): State<Arc<Hub>>) -> Response {
    Json(serde_json::json!({ "algorithm": "Ed25519", "key": hub.signer.public_hex() })).into_response()
}

/// A cookie's value from the request.
pub(crate) fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| pair.trim().strip_prefix(name)?.strip_prefix('=').map(str::to_string))
}

const TOKEN_COOKIE: &str = "agora_token";

#[derive(Deserialize)]
struct Session {
    #[serde(default)]
    token: Option<String>,
    /// A member key (e.g. one handed to the glasses).
    #[serde(default)]
    member: Option<String>,
}

fn set_cookie(headers: &HeaderMap, name: &str, value: &str, max_age: u32) -> Option<HeaderValue> {
    let secure = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).is_some_and(|p| p.eq_ignore_ascii_case("https"));
    HeaderValue::from_str(&format!("{name}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{}", if secure { "; Secure" } else { "" })).ok()
}

/// Turn a token and/or member key into session cookies, so the page holds no secret
/// and event streams need none in their URLs.
async fn session(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Result<Json<Session>, JsonRejection>) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("that isn't a valid session: {}", e.body_text())),
    };
    let cookie_safe = |v: &str| !v.is_empty() && v.len() <= 256 && v.bytes().all(|b| b.is_ascii_graphic() && !matches!(b, b';' | b',' | b'"' | b'\\'));
    let mut out = HeaderMap::new();
    if let Some(token) = body.token.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        let ok = hub.config.token.as_ref().is_some_and(|t| same(t.as_bytes(), token.as_bytes()));
        if !ok || !cookie_safe(token) {
            return unauthorized();
        }
        out.extend(set_cookie(&headers, TOKEN_COOKIE, token, 30 * 24 * 3600).map(|v| (header::SET_COOKIE, v)));
    } else if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let mut me = None;
    if let Some(key) = body.member.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        let Some(member) = hub.company().by_key(key).cloned() else { return error(StatusCode::NOT_FOUND, "no one in the office has that key") };
        if !cookie_safe(key) {
            return error(StatusCode::BAD_REQUEST, "that key can't be kept in a cookie");
        }
        out.append(header::SET_COOKIE, set_cookie(&headers, company::MEMBER_COOKIE, key, 365 * 24 * 3600).unwrap_or(HeaderValue::from_static("")));
        me = Some(member);
    }
    (out, Json(serde_json::json!({ "member": me }))).into_response()
}

/// Forget the session cookies (signing this device out).
async fn end_session(headers: HeaderMap) -> Response {
    clear_cookies(&headers, &[TOKEN_COOKIE, company::MEMBER_COOKIE])
}

/// Forget only who this device is (after leaving the office), keeping the token.
async fn end_membership(headers: HeaderMap) -> Response {
    clear_cookies(&headers, &[company::MEMBER_COOKIE])
}

fn clear_cookies(headers: &HeaderMap, names: &[&str]) -> Response {
    let mut out = HeaderMap::new();
    for &name in names {
        if let Some(v) = set_cookie(headers, name, "", 0) {
            out.append(header::SET_COOKIE, v);
        }
    }
    (out, StatusCode::NO_CONTENT).into_response()
}

/// The gzipped copy (about a quarter of the size) when the browser accepts it.
/// Cached briefly: the file only changes with a new server build.
fn asset(headers: &HeaderMap, content_type: &'static str, plain: &'static [u8], gzipped: Option<&'static [u8]>) -> Response {
    let gzip = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|e| {
            let mut parts = e.split(';').map(str::trim);
            parts.next() == Some("gzip") && !parts.any(|p| matches!(p, "q=0" | "q=0.0" | "q=0.00" | "q=0.000"))
        }));
    let common = [
        (header::CONTENT_TYPE, content_type),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::CACHE_CONTROL, "public, max-age=300"),
        (header::VARY, "Accept-Encoding"),
    ];
    match gzipped {
        Some(gz) if gzip => (common, [(header::CONTENT_ENCODING, "gzip")], gz).into_response(),
        _ => (common, plain).into_response()
    }
}

/// The glasses app: same rules as the board (no outside scripts at all).
async fn glasses() -> Response {
    let csp = "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; \
               img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'";
    ([(header::CONTENT_SECURITY_POLICY, csp), (header::X_CONTENT_TYPE_OPTIONS, "nosniff")], Html(GLASSES)).into_response()
}

async fn index() -> Response {
    // Everything the page shows is set with textContent; this keeps it that way.
    // Google Fonts is the one outside origin: its stylesheet and the font files. `blob:`
    // is the glTF loader's own: it fetches the models' embedded textures as blob URLs.
    let csp = "default-src 'self'; script-src 'self' 'unsafe-inline'; \
               style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; \
               img-src 'self' data: blob:; connect-src 'self' blob:; frame-src https://meet.jit.si; frame-ancestors 'none'; base-uri 'none'";
    ([(header::CONTENT_SECURITY_POLICY, csp), (header::X_CONTENT_TYPE_OPTIONS, "nosniff")], Html(INDEX)).into_response()
}

/// Constant-time comparison, so the token can't be guessed byte by byte from timing.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The bearer header, or the session cookie.
fn authorized(hub: &Hub, headers: &HeaderMap) -> bool {
    let Some(token) = &hub.config.token else { return true };
    let given = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(str::to_string).or_else(|| cookie(headers, TOKEN_COOKIE));
    given.is_some_and(|g| same(g.as_bytes(), token.as_bytes()))
}

fn unauthorized() -> Response {
    error(StatusCode::UNAUTHORIZED, "a valid access token is required")
}

#[derive(Deserialize)]
struct StartRun {
    brief: String,
    #[serde(default)]
    team: Option<TeamChoice>,
    /// The planner's (and default workers') model; `None` = the office default.
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reviewer_model: Option<String>,
}

impl Hub {
    /// The client for a model's host, if the office has that host.
    fn client_for(&self, model: &str) -> Option<&MuseClient> {
        self.config.client_for(model)
    }

    fn check_model(&self, model: &str) -> Result<(), String> {
        self.config.check_model(model)
    }
}

/// A preset by id, or a team someone made (checked before it runs).
#[derive(Deserialize)]
#[serde(untagged)]
enum TeamChoice {
    Preset(String),
    Custom(Team),
}

fn resolve(choice: Option<TeamChoice>) -> Result<Option<Team>, String> {
    match choice {
        None => Ok(None),
        Some(TeamChoice::Preset(id)) => {
            agora_core::preset(&id).map(Some).ok_or_else(|| format!("there is no preset team \"{id}\""))
        }
        Some(TeamChoice::Custom(team)) => team.validate().map(|()| Some(team)),
    }
}

#[derive(Serialize)]
struct Started {
    id: String,
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

async fn start(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Result<Json<StartRun>, JsonRejection>) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    // A malformed body (e.g. a custom team missing a field) still gets a JSON error
    // the page can show.
    let Json(body) = match body {
        Ok(body) => body,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("that request isn't a valid run: {}", e.body_text())),
    };
    let brief = body.brief.trim().to_string();
    if brief.is_empty() || brief.chars().count() > MAX_BRIEF_CHARS {
        return error(StatusCode::BAD_REQUEST, "the brief must be 1 to 4000 characters");
    }
    let team = match resolve(body.team) {
        Ok(team) => team,
        Err(why) => return error(StatusCode::BAD_REQUEST, &why),
    };
    let model = body.model.unwrap_or_else(|| hub.config.model.clone());
    let reviewer_model = body.reviewer_model.or_else(|| hub.config.reviewer_model.clone());
    let chosen = [Some(&model), reviewer_model.as_ref()].into_iter().flatten();
    let agents = team.iter().flat_map(|t| &t.agents).filter_map(|a| a.model.as_ref());
    if let Some(why) = chosen.chain(agents).find_map(|m| hub.check_model(m).err()) {
        return error(StatusCode::BAD_REQUEST, &why);
    }
    let config = &hub.config;
    {
        let mut state = hub.state();
        if state.rooms.iter().filter(|r| !r.done()).count() + state.starting >= config.max_active {
            return error(StatusCode::TOO_MANY_REQUESTS, "too many runs in progress; try again when one finishes");
        }
        state.starting += 1; // the slot is ours until the room is added
    }
    let release = || hub.state().starting -= 1;
    let journal = match &config.journal {
        Some(path) => {
            let (path, b, model, t) = (path.clone(), brief.clone(), model.clone(), team.clone());
            let reviewer = reviewer_model.clone();
            // SQLite is blocking I/O: keep it off the async workers.
            match tokio::task::spawn_blocking(move || Journal::start(&path, &b, &model, reviewer.as_deref(), t.as_ref())).await {
                Ok(Ok(j)) => Some(j),
                Ok(Err(e)) => {
                    release();
                    return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
                }
                Err(_) => {
                    release();
                    return error(StatusCode::INTERNAL_SERVER_ERROR, "could not start the run journal");
                }
            }
        }
        None => None,
    };
    let id = journal.as_ref().map_or_else(
        || format!("run-{:x}", SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos())),
        |j| j.run_id().to_string(),
    );

    let (live, _) = broadcast::channel(256);
    let room = Arc::new(Room {
        id: id.clone(),
        brief: brief.clone(),
        team: team.as_ref().map(|t| t.name.clone()),
        inner: Mutex::new(RoomInner { history: Vec::new(), live: Some(live) }),
    });
    {
        let mut state = hub.state();
        state.starting -= 1;
        state.rooms.push(Arc::clone(&room));
        while state.rooms.iter().filter(|r| r.done()).count() > KEEP_RUNS {
            let Some(oldest) = state.rooms.iter().position(|r| r.done()) else { break };
            state.rooms.remove(oldest);
        }
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    let forward = Arc::clone(&room);
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            forward.push(event);
        }
        forward.close(); // every sender is gone: the run is over
    });
    let run_config = RunConfig {
        model,
        max_parallel: config.max_parallel,
        cap_usd: config.cap_usd,
        reviewer_model,
        baseten: config.baseten.clone(),
        events: Some(tx.clone()),
        team,
    };
    let muse = config.muse.clone();
    let running = tokio::spawn(async move { run(muse, &run_config, &brief, journal).await });
    tokio::spawn(async move {
        // Whatever happens to the run, the board hears how it ended.
        let reason = match running.await {
            Ok(Ok(_)) => return,
            Ok(Err(e)) => e.to_string(),
            Err(_) => "the run stopped unexpectedly (internal error)".to_string(),
        };
        let _ = tx.send(RunEvent::RunFailed { reason });
    });
    (StatusCode::CREATED, Json(Started { id })).into_response()
}

#[derive(Serialize)]
struct Info<'a> {
    model: &'a str,
    /// The default reviewer model; `None` = the same as the writer's.
    reviewer: Option<&'a str>,
    /// `None` when runs have no spending cap.
    cap_usd: Option<f64>,
    /// Whether a token is needed (the page asks for one only then).
    token_required: bool,
    /// The key that signs ledger heads (hex).
    signing_key: String,
    /// Whether the office keeps its state across restarts.
    persistent: bool,
}

async fn info(State(hub): State<Arc<Hub>>) -> Response {
    let c = &hub.config;
    Json(Info {
        model: &c.model,
        reviewer: c.reviewer_model.as_deref(),
        cap_usd: c.cap_usd.is_finite().then_some(c.cap_usd),
        token_required: c.token.is_some(),
        signing_key: hub.signer.public_hex(),
        persistent: hub.store.is_some(),
    })
    .into_response()
}

#[derive(Serialize)]
struct ModelChoice {
    #[serde(flatten)]
    info: agora_muse::ModelInfo,
    /// Whether this office has the model's host set up.
    available: bool,
}

async fn models(State(hub): State<Arc<Hub>>) -> Response {
    let list: Vec<ModelChoice> =
        agora_muse::MODELS.iter().map(|m| ModelChoice { info: *m, available: hub.client_for(m.id).is_some() }).collect();
    Json(list).into_response()
}

#[derive(Serialize)]
struct RunSummary {
    id: String,
    brief: String,
    team: Option<String>,
    done: bool,
}

async fn list(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let state = hub.state();
    let runs: Vec<RunSummary> =
        state.rooms.iter().rev().map(|r| RunSummary { id: r.id.clone(), brief: r.brief.clone(), team: r.team.clone(), done: r.done() }).collect();
    Json(runs).into_response()
}

/// `None` (and a note on stderr) for an event that can't be serialized, rather than
/// sending the browser something it can't parse.
fn to_sse<T: Serialize>(event: &T) -> Option<Result<Event, Infallible>> {
    match serde_json::to_string(event) {
        Ok(json) => Some(Ok(Event::default().data(json))),
        Err(e) => {
            eprintln!("dropped an event that could not be serialized: {e}");
            None
        }
    }
}

async fn events(State(hub): State<Arc<Hub>>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let Some(room) = hub.room(&id) else { return error(StatusCode::NOT_FOUND, "no such run") };
    let (history, live) = room.subscribe();
    let past = stream::iter(history).filter_map(|e| async move { to_sse(&e) });
    // A watcher that falls more than 256 events behind is dropped; its browser
    // reconnects and gets the full history.
    let live = stream::unfold(live, |rx| async move {
        let mut rx = rx?;
        let event = rx.recv().await.ok()?;
        Some((event, Some(rx)))
    })
    .filter_map(|e| async move { to_sse(&e) });
    Sse::new(past.chain(live)).keep_alive(KeepAlive::default()).into_response()
}
