//! The text models Agora can run on, and which host serves each.
//!
//! A model id belongs to exactly one host here (Baseten's ids start with
//! `meta-models/`), so routing a call by its model id is never ambiguous. Every model
//! listed has a price in [`crate::price_for`]. Checked live against `GET /v1/models`
//! and one call each on 2026-10-30.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Host {
    /// Meta's Model API (`MODEL_API_KEY`).
    Meta,
    /// Muse Spark Open, the open weights, on Baseten (`BASETEN_API_KEY`).
    Baseten,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ModelInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub host: Host,
    /// One line on when to pick it.
    pub about: &'static str,
}

pub const MODELS: &[ModelInfo] = &[
    ModelInfo { id: "muse-spark-1.3", label: "Muse Spark 1.3", host: Host::Meta, about: "The default: strong, fast, balanced." },
    ModelInfo {
        id: "muse-wonder-1.0",
        label: "Muse Wonder 1.0",
        host: Host::Meta,
        about: "The most capable, for the hardest tasks. About 4x the price of Spark.",
    },
    ModelInfo {
        id: "muse-spark-1.3-contributor",
        label: "Muse Spark 1.3 Contributor",
        host: Host::Meta,
        about: "Spark 1.3 at about a tenth of the price; prompts may be used to improve Meta's models.",
    },
    ModelInfo { id: "muse-spark-1.2", label: "Muse Spark 1.2", host: Host::Meta, about: "The previous Spark." },
    ModelInfo {
        id: "muse-spark-1.2-contributor",
        label: "Muse Spark 1.2 Contributor",
        host: Host::Meta,
        about: "Spark 1.2, contributor pricing.",
    },
    ModelInfo { id: "muse-spark-1.1", label: "Muse Spark 1.1", host: Host::Meta, about: "An older Spark." },
    ModelInfo {
        id: "meta-models/muse-spark-open-1026",
        label: "Muse Spark Open (Baseten)",
        host: Host::Baseten,
        about: "Meta's open-weight Spark, served by Baseten. A second opinion on a different host.",
    },
];

/// The catalog entry for a model id.
#[must_use]
pub fn model_info(id: &str) -> Option<&'static ModelInfo> {
    MODELS.iter().find(|m| m.id == id)
}

/// The host that serves a model id: the catalog's, else by the id's form (Baseten's
/// ids start with `meta-models/`, Meta's with `muse-`). `None` = no known host.
#[must_use]
pub fn host_of(id: &str) -> Option<Host> {
    match model_info(id) {
        Some(m) => Some(m.host),
        None if id.starts_with("meta-models/") => Some(Host::Baseten),
        None if id.starts_with("muse-") => Some(Host::Meta),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_model_is_priced_and_hosts_never_share_an_id() {
        for m in MODELS {
            assert!(crate::price_for(m.id).is_some(), "{} has no price", m.id);
            assert_eq!(m.host == Host::Baseten, m.id.starts_with("meta-models/"), "{}", m.id);
        }
        assert!(model_info("sam-3.1").is_none(), "not a text model");
        // Ids the catalog doesn't list still go to the host their form names.
        assert_eq!(host_of("meta-models/muse-spark-open"), Some(Host::Baseten));
        assert_eq!(host_of("muse-spark-9.9"), Some(Host::Meta));
        assert_eq!(host_of("gpt-5"), None);
    }
}
