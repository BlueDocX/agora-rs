//! A small typed client for Meta's Model API (the Muse models).
//!
//! Only what Agora uses: the Responses endpoint, structured output, retries that
//! respect `Retry-After`, rate-limit headers, and exact cost per call.
//! Facts it relies on are in `research/muse-api-notes.md` (and checked live in R1).

mod client;
mod cost;
mod types;

pub use client::{MuseClient, MuseError};
pub use cost::{Price, price_for};
pub use types::{Effort, Message, RateLimit, Request, Response, Role, Usage};
