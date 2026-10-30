//! Skills: short playbooks an agent works by, distilled (in our own words) from the
//! agent-skill library this project was built with. An agent carries up to
//! [`MAX_KIT`] of them; their rules go into its instructions on every task, chat
//! reply and errand.

use std::fmt::Write as _;

use serde::Serialize;

pub const MAX_KIT: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Skill {
    pub id: &'static str,
    pub name: &'static str,
    pub summary: &'static str,
    pub playbook: &'static [&'static str],
    /// The agent skills it was distilled from (credit, and where to read more).
    pub sources: &'static [&'static str],
}

include!("skills_list.rs");

/// The skill with this id.
#[must_use]
pub fn skill(id: &str) -> Option<&'static Skill> {
    SKILLS.iter().find(|s| s.id == id)
}

/// The playbooks of `kit`, as an agent reads them (unknown ids are skipped).
#[must_use]
pub fn playbooks(kit: &[String]) -> String {
    kit.iter().filter_map(|id| skill(id)).fold(String::new(), |mut out, s| {
        let _ = writeln!(out, "{} ({}):", s.name, s.summary);
        for rule in s.playbook {
            let _ = writeln!(out, "- {rule}");
        }
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skills_are_unique_short_and_concrete() {
        let mut ids: Vec<&str> = SKILLS.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), SKILLS.len(), "skill ids are unique");
        for s in SKILLS {
            assert!(s.id.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'), "{}", s.id);
            assert!((3..=6).contains(&s.playbook.len()), "{} has {} rules", s.id, s.playbook.len());
            assert!(s.playbook.iter().all(|r| r.split_whitespace().count() <= 24), "{}: a rule is too long", s.id);
        }
        let text = playbooks(&["no-such-skill".into(), SKILLS[0].id.into()]);
        assert!(text.starts_with(SKILLS[0].name) && text.lines().count() == SKILLS[0].playbook.len() + 1);
    }
}
