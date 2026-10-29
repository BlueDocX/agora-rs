//! The room hub: start a run over HTTP and watch it live.
//!
//! - `POST /api/runs {"brief": "..."}` starts a run. At most `max_active` run at once.
//! - `GET /api/runs` lists runs, newest first (the last [`KEEP_RUNS`] finished ones
//!   are kept in memory; every run is also in the journal).
//! - `GET /api/runs/{id}/events` streams the run as Server-Sent Events: everything
//!   that already happened, then each new event live, ending when the run does.
//! - `GET /api/info` names the models and the per-run cap (no token needed: nothing
//!   secret, and the page shows it before anyone types a token).
//! - `GET /` is the office board.
//!
//! When a token is set, every other `/api` call needs it: as `Authorization: Bearer`, or
//! as `?token=` on the event stream (a browser `EventSource` can't send headers).

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use agora_engine::{Journal, Remote, RunConfig, RunEvent, run};
use agora_muse::MuseClient;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
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
/// The 3D office, built from `web/` with `npm run build`, which writes both files.
/// Both are committed, so the server builds without Node.
const OFFICE_JS: &str = include_str!("../static/office.js");
const OFFICE_JS_GZ: &[u8] = include_bytes!("../static/office.js.gz");
/// The office furniture and the teammate, built in Blender by `assets/blender/build_assets.py`,
/// which writes each model and its gzipped copy together.
const OFFICE_KIT: &[u8] = include_bytes!("../static/models/office-kit.glb");
const OFFICE_KIT_GZ: &[u8] = include_bytes!("../static/models/office-kit.glb.gz");
const CHIBI: &[u8] = include_bytes!("../static/models/chibi.glb");
const CHIBI_GZ: &[u8] = include_bytes!("../static/models/chibi.glb.gz");

/// How the hub runs briefs.
#[derive(Debug, Clone)]
pub struct HubConfig {
    pub muse: MuseClient,
    pub model: String,
    pub cap_usd: f64,
    pub max_parallel: usize,
    pub reviewer: Option<Remote>,
    /// Journal every run here (so it can be resumed from the CLI). `None` = don't.
    pub journal: Option<PathBuf>,
    /// Required as `Authorization: Bearer <token>` to start a run. `None` = open
    /// (only safe on localhost; `main` refuses to listen elsewhere without one).
    pub token: Option<String>,
    /// Runs allowed at the same time; more get 429.
    pub max_active: usize,
}

struct Room {
    id: String,
    brief: String,
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
}

struct Hub {
    config: HubConfig,
    state: Mutex<HubState>,
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
    let hub = Arc::new(Hub { config, state: Mutex::new(HubState::default()) });
    Router::new()
        .route("/", get(index))
        .route("/office.js", get(|h: HeaderMap| async move { asset(&h, "text/javascript; charset=utf-8", OFFICE_JS.as_bytes(), OFFICE_JS_GZ) }))
        .route("/models/office-kit.glb", get(|h: HeaderMap| async move { asset(&h, "model/gltf-binary", OFFICE_KIT, OFFICE_KIT_GZ) }))
        .route("/models/chibi.glb", get(|h: HeaderMap| async move { asset(&h, "model/gltf-binary", CHIBI, CHIBI_GZ) }))
        .route("/api/info", get(info))
        .route("/api/runs", post(start).get(list))
        .route("/api/runs/{id}/events", get(events))
        .with_state(hub)
}

/// The gzipped copy (about a quarter of the size) when the browser accepts it.
/// Cached briefly: the file only changes with a new server build.
fn asset(headers: &HeaderMap, content_type: &'static str, plain: &'static [u8], gzipped: &'static [u8]) -> Response {
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
    if gzip {
        (common, [(header::CONTENT_ENCODING, "gzip")], gzipped).into_response()
    } else {
        (common, plain).into_response()
    }
}

async fn index() -> Response {
    // Everything the page shows is set with textContent; this keeps it that way.
    // Google Fonts is the one outside origin: its stylesheet and the font files.
    let csp = "default-src 'self'; script-src 'self' 'unsafe-inline'; \
               style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; \
               img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'";
    ([(header::CONTENT_SECURITY_POLICY, csp), (header::X_CONTENT_TYPE_OPTIONS, "nosniff")], Html(INDEX)).into_response()
}

/// Constant-time comparison, so the token can't be guessed byte by byte from timing.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The bearer header, or (for the event stream) `?token=`.
fn authorized(hub: &Hub, headers: &HeaderMap, query: Option<&str>) -> bool {
    let Some(token) = &hub.config.token else { return true };
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or(query);
    given.is_some_and(|g| same(g.as_bytes(), token.as_bytes()))
}

fn unauthorized() -> Response {
    error(StatusCode::UNAUTHORIZED, "a valid access token is required")
}

#[derive(Deserialize)]
struct StartRun {
    brief: String,
}

#[derive(Serialize)]
struct Started {
    id: String,
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

async fn start(State(hub): State<Arc<Hub>>, headers: HeaderMap, Json(body): Json<StartRun>) -> Response {
    if !authorized(&hub, &headers, None) {
        return unauthorized();
    }
    let brief = body.brief.trim().to_string();
    if brief.is_empty() || brief.chars().count() > MAX_BRIEF_CHARS {
        return error(StatusCode::BAD_REQUEST, "the brief must be 1 to 4000 characters");
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
            let (path, b, model) = (path.clone(), brief.clone(), config.model.clone());
            let reviewer = config.reviewer.as_ref().map(|r| r.model.clone());
            // SQLite is blocking I/O: keep it off the async workers.
            match tokio::task::spawn_blocking(move || Journal::start(&path, &b, &model, reviewer.as_deref())).await {
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
        model: config.model.clone(),
        max_parallel: config.max_parallel,
        cap_usd: config.cap_usd,
        reviewer: config.reviewer.clone(),
        events: Some(tx.clone()),
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
    reviewer: Option<&'a str>,
    cap_usd: f64,
}

async fn info(State(hub): State<Arc<Hub>>) -> Response {
    let c = &hub.config;
    Json(Info { model: &c.model, reviewer: c.reviewer.as_ref().map(|r| r.model.as_str()), cap_usd: c.cap_usd })
        .into_response()
}

#[derive(Serialize)]
struct RunSummary {
    id: String,
    brief: String,
    done: bool,
}

async fn list(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers, None) {
        return unauthorized();
    }
    let state = hub.state();
    let runs: Vec<RunSummary> =
        state.rooms.iter().rev().map(|r| RunSummary { id: r.id.clone(), brief: r.brief.clone(), done: r.done() }).collect();
    Json(runs).into_response()
}

/// `None` (and a note on stderr) for an event that can't be serialized, rather than
/// sending the browser something it can't parse.
fn to_sse(event: &RunEvent) -> Option<Result<Event, Infallible>> {
    match serde_json::to_string(event) {
        Ok(json) => Some(Ok(Event::default().data(json))),
        Err(e) => {
            eprintln!("dropped an event that could not be serialized: {e}");
            None
        }
    }
}

async fn events(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !authorized(&hub, &headers, query.get("token").map(String::as_str)) {
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
