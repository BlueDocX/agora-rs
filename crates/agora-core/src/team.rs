//! Teams: who works in the office. A team is a few named agents, each with a role
//! and skills; the planner gives every task to one of them, and the chat can address
//! them by name. Teams come from [`presets`] or are made by a person (and checked
//! with [`Team::validate`] before use).

use serde::{Deserialize, Serialize};

pub const MAX_AGENTS: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    /// Short kebab-case id, unique in the team (the planner assigns tasks by it).
    pub id: String,
    /// What people call it in the chat, as `@name`.
    pub name: String,
    pub role: String,
    /// `#rrggbb`, for its desk, chair and chat bubble.
    pub color: String,
    /// How it works: added to its instructions on every task.
    #[serde(default)]
    pub skills: String,
    /// Skill playbooks it works by (ids from [`crate::skills::SKILLS`]), at most
    /// [`crate::skills::MAX_KIT`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kit: Vec<String>,
    /// The model it works on; `None` = the run's model. (Checked against the hosts
    /// the office has when a run starts.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Which character plays it in the 3D office; `None` = picked by seat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub character: Option<String>,
}

impl Agent {
    /// What its instructions say about how it works: its own note and its skills.
    #[must_use]
    pub fn briefing(&self) -> String {
        let mut out = String::new();
        if !self.skills.trim().is_empty() {
            out.push_str(" How you work: ");
            out.push_str(self.skills.trim());
        }
        let kit = crate::skills::playbooks(&self.kit);
        if !kit.is_empty() {
            out.push_str("\n\nYour skills (follow these):\n");
            out.push_str(&kit);
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    pub id: String,
    pub name: String,
    /// The kind of work, e.g. "Agency".
    pub kind: String,
    #[serde(default)]
    pub description: String,
    pub agents: Vec<Agent>,
}

fn id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn text_ok(what: &str, s: &str, min: usize, max: usize) -> Result<(), String> {
    let n = s.trim().chars().count();
    if n < min || n > max {
        return Err(if min > 0 { format!("{what} must be {min}-{max} characters") } else { format!("{what} must be at most {max} characters") });
    }
    Ok(())
}

impl Team {
    /// Checks a team before it runs: sizes, ids, colors, and names that are unique
    /// (ignoring case), so every `@name` means one agent.
    ///
    /// # Errors
    /// The first problem, in words a person can act on.
    pub fn validate(&self) -> Result<(), String> {
        text_ok("the team name", &self.name, 1, 40)?;
        text_ok("the team kind", &self.kind, 0, 40)?;
        text_ok("the team description", &self.description, 0, 200)?;
        if !id_ok(&self.id) {
            return Err("the team id must be 1-32 lowercase letters, digits or dashes".into());
        }
        if self.agents.is_empty() || self.agents.len() > MAX_AGENTS {
            return Err(format!("a team has 1-{MAX_AGENTS} agents"));
        }
        let mut ids = Vec::new();
        let mut names = Vec::new();
        for a in &self.agents {
            if !id_ok(&a.id) {
                return Err(format!("agent id \"{}\" must be 1-32 lowercase letters, digits or dashes", a.id));
            }
            text_ok("an agent name", &a.name, 1, 40)?;
            if a.name.chars().any(|c| c == '@' || c.is_control()) {
                return Err(format!("agent name \"{}\" can't contain @, tabs or line breaks", a.name.escape_debug()));
            }
            text_ok("an agent role", &a.role, 1, 80)?;
            text_ok("an agent's skills", &a.skills, 0, 600)?;
            if a.kit.len() > crate::skills::MAX_KIT {
                return Err(format!("agent \"{}\" can carry at most {} skills", a.name, crate::skills::MAX_KIT));
            }
            if let Some(unknown) = a.kit.iter().find(|id| crate::skills::skill(id).is_none()) {
                return Err(format!("there is no skill \"{unknown}\""));
            }
            for (what, value) in [("model", &a.model), ("character", &a.character)] {
                if value.as_ref().is_some_and(|v| v.is_empty() || v.len() > 64 || v.chars().any(char::is_control)) {
                    return Err(format!("agent \"{}\" has an unusable {what} name", a.name));
                }
            }
            let hex = a.color.strip_prefix('#').unwrap_or("");
            if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!("agent \"{}\" needs a color like #4285f4", a.name));
            }
            if ids.contains(&a.id.as_str()) {
                return Err(format!("two agents have the id \"{}\"", a.id));
            }
            let key = a.name.trim().to_lowercase();
            if names.contains(&key) {
                return Err(format!("two agents are called \"{}\"", a.name.trim()));
            }
            ids.push(&a.id);
            names.push(key);
        }
        Ok(())
    }

    #[must_use]
    pub fn agent(&self, id: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.id == id)
    }
}

const COLORS: [&str; 6] = ["#4285f4", "#34a853", "#ea4335", "#14b8a6", "#f97316", "#ec4899"];

fn team(id: &str, name: &str, kind: &str, description: &str, agents: &[(&str, &str, &str, &str, &[&str])]) -> Team {
    Team {
        id: id.into(),
        name: name.into(),
        kind: kind.into(),
        description: description.into(),
        agents: agents
            .iter()
            .zip(COLORS)
            .map(|(&(id, name, role, skills, kit), color)| Agent {
                id: id.into(),
                name: name.into(),
                role: role.into(),
                color: color.into(),
                skills: skills.into(),
                kit: kit.iter().map(|k| (*k).to_string()).collect(),
                model: None,
                character: None,
            })
            .collect(),
    }
}

/// The built-in team with this id.
#[must_use]
pub fn preset(id: &str) -> Option<Team> {
    presets().into_iter().find(|t| t.id == id)
}

/// The built-in teams. Text work only: image, music and video models aren't
/// available everywhere (Muse Image is geo-restricted), so no preset depends on them.
#[must_use]
pub fn presets() -> Vec<Team> {
    vec![
        team("agora", "Agora Core", "General", "A small all-round team for any written deliverable.", &[
            ("researcher", "Researcher", "Finds, organizes and sources the facts", "Separate what is known from what is assumed; never invent sources.", &["sourced-research", "verify-before-claiming"]),
            ("writer", "Writer", "Writes clear, engaging prose", "Lead with the point, vary sentence length, cut filler.", &["plain-writing", "editing"]),
            ("analyst", "Analyst", "Works with numbers, options and trade-offs", "State assumptions, show the calculation, give the headline number first.", &["numbers-and-stats", "decision-memo"]),
            ("editor", "Editor", "Tightens, checks and assembles the final piece", "Keep the voice consistent; remove repetition; add nothing new when assembling.", &["editing", "verify-before-claiming", "plain-writing"]),
        ]),
        team("creative-studio", "Creative Studio", "Agency", "Campaigns, brand voice and launch copy.", &[
            ("director", "Creative Director", "Sets the concept and keeps the work on brief", "Pick one strong idea and make every piece serve it.", &["narrative-and-concept", "pressure-test"]),
            ("copywriter", "Copywriter", "Writes headlines, taglines and campaign copy", "Lead with the benefit; every word earns its place.", &["hooks-and-social", "brand-voice", "editing"]),
            ("art-director", "Art Director", "Writes the visual direction and image briefs", "Describe composition, palette and mood precisely enough to shoot or generate.", &["narrative-and-concept", "brand-voice"]),
            ("strategist", "Brand Strategist", "Defines audience, positioning and message", "Name the audience and the one thing they should believe.", &["user-research", "brand-voice", "decision-memo"]),
        ]),
        team("strategy-coach", "Strategy Coach", "Strategic", "Decisions, plans and honest pressure-testing.", &[
            ("strategist", "Strategist", "Frames the decision and recommends a path", "Give a recommendation, not a survey; say what would change your mind.", &["decision-memo", "pressure-test"]),
            ("market-analyst", "Market Analyst", "Sizes markets and compares options", "Show the arithmetic; flag every number that is an estimate.", &["numbers-and-stats", "sourced-research"]),
            ("skeptic", "Devil's Advocate", "Finds the strongest objection and the likeliest failure", "Steelman first, then attack the weakest assumption.", &["pressure-test", "verify-before-claiming"]),
        ]),
        team("pr-agency", "PR Agency", "Public Relations", "Announcements, press and social.", &[
            ("pr-lead", "PR Lead", "Owns the story and the key messages", "Three messages at most, each backed by a fact.", &["plain-writing", "brand-voice", "decision-memo"]),
            ("press-writer", "Press Writer", "Writes press releases and media pitches", "Inverted pyramid; quotes that say something.", &["plain-writing", "sourced-research", "editing"]),
            ("social", "Social Manager", "Writes posts for each platform", "Native to the platform; one call to action.", &["hooks-and-social", "brand-voice"]),
            ("media-analyst", "Media Analyst", "Picks outlets and anticipates questions", "Prepare the hard questions and honest answers.", &["pressure-test", "sourced-research"]),
        ]),
        team("game-studio", "Game Dev Studio", "Game Development", "Game concepts, design docs and narrative.", &[
            ("designer", "Game Designer", "Designs the core loop, rules and progression", "Describe what the player does every 30 seconds.", &["narrative-and-concept", "specs-and-acceptance"]),
            ("narrative", "Narrative Writer", "Writes the world, characters and dialogue", "Show character through choices, not exposition.", &["narrative-and-concept", "editing"]),
            ("level-designer", "Level Designer", "Lays out levels, pacing and challenges", "Teach a mechanic, test it, then twist it.", &["specs-and-acceptance", "experiments"]),
            ("producer", "Producer", "Scopes, schedules and assembles the design doc", "Cut scope before quality; flag risks with owners.", &["planning", "decision-memo"]),
        ]),
        team("security", "Cybersecurity Swarm", "Security Audit", "Threat models, audit plans and incident playbooks (defensive).", &[
            ("threat-modeler", "Threat Modeler", "Maps assets, entry points and threats", "Use STRIDE; rank threats by likelihood and impact.", &["threat-modeling", "verify-before-claiming"]),
            ("tester", "Security Tester", "Plans authorized tests of the controls", "Defensive and authorized only; describe checks, not exploits.", &["threat-modeling", "specs-and-acceptance"]),
            ("compliance", "Compliance Analyst", "Maps findings to standards and policy", "Cite the control (OWASP, ISO 27001, SOC 2) each finding relates to.", &["sourced-research", "verify-before-claiming"]),
            ("responder", "Incident Responder", "Writes detection and response playbooks", "Concrete steps with owners and time targets.", &["incident-response", "plain-writing"]),
        ]),
        team("product", "Product Team", "Product", "Specs, user stories, release notes.", &[
            ("pm", "Product Manager", "Defines the problem, users and priorities", "Problem before solution; measurable success criteria.", &["user-research", "specs-and-acceptance", "decision-memo"]),
            ("ux-writer", "UX Writer", "Writes interface copy and flows", "Plain words; say what happens next.", &["ux-copy", "plain-writing"]),
            ("engineer", "Engineer", "Plans the build and its risks", "Simplest design that works; name the unknowns.", &["simple-engineering", "planning"]),
            ("qa", "QA Lead", "Writes test plans and acceptance checks", "Cover the edge cases a user will actually hit.", &["specs-and-acceptance", "verify-before-claiming"]),
        ]),
        team("research-lab", "Research Lab", "Research", "Literature summaries, explainers and fact checks.", &[
            ("lead-researcher", "Lead Researcher", "Scopes the question and synthesizes findings", "Say how strong the evidence is for each claim.", &["sourced-research", "plain-writing"]),
            ("fact-checker", "Fact-Checker", "Verifies every claim", "Mark each claim confirmed, unsupported or false, with a reason.", &["verify-before-claiming", "sourced-research"]),
            ("data-analyst", "Data Analyst", "Reads and explains the numbers", "Never invent figures; explain what a number does and doesn't show.", &["numbers-and-stats", "experiments"]),
            ("science-writer", "Science Writer", "Explains it for a general reader", "One idea per paragraph; analogies that are accurate.", &["plain-writing", "editing"]),
        ]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_is_valid() {
        let all = presets();
        for t in &all {
            t.validate().unwrap_or_else(|e| panic!("{}: {e}", t.id));
        }
        let mut ids: Vec<&str> = all.iter().map(|t| t.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), all.len(), "preset ids are unique");
        // Every preset agent carries skills that exist (validate checks the ids).
        assert!(all.iter().flat_map(|t| &t.agents).all(|a| (2..=crate::skills::MAX_KIT).contains(&a.kit.len())));
    }

    #[test]
    fn rejects_what_would_break_mentions_or_the_page() {
        let base = presets().remove(0);
        let with = |f: &dyn Fn(&mut Team)| {
            let mut t = base.clone();
            f(&mut t);
            t.validate().unwrap_err()
        };
        assert!(with(&|t| t.agents[1].name = "researcher ".into()).contains("two agents are called"));
        assert!(with(&|t| t.agents[0].name = "a@b".into()).contains('@'));
        assert!(with(&|t| t.agents[0].color = "blue".into()).contains("color"));
        assert!(with(&|t| t.agents[0].id = "Bad Id".into()).contains("lowercase"));
        assert!(with(&|t| t.agents.clear()).contains("1-6"));
        assert!(with(&|t| t.agents.extend(base.agents.clone())).contains("1-6"));
        assert!(with(&|t| t.agents[0].skills = "x".repeat(601)).contains("600"));
    }
}
