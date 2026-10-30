//! A small typed client for Meta's Model API (the Muse models).
//!
//! Only what Agora uses: the Responses endpoint (and Chat Completions, for partner
//! hosts of Meta's open-weight models), structured output, retries that
//! respect `Retry-After`, rate-limit headers, and exact cost per call.
//! Facts it relies on are in `research/muse-api-notes.md` (and checked live in R1).

mod client;
mod cost;
mod models;
mod types;

pub use client::{Api, BASETEN_BASE_URL, MuseClient, MuseError};
pub use cost::{Price, price_for};
pub use models::{Host, MODELS, ModelInfo, host_of, model_info};
pub use types::{Effort, Message, RateLimit, Request, Response, Role, Usage};
