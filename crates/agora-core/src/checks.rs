//! Checks that plain code can decide, run before any model reviews a result.
//!
//! A model is unreliable at counting words or noticing a missing section; this code
//! is exact, free and instant. Each failure is worded as an unmet criterion so it can
//! go straight back to the worker.

/// Limits the planner sets on a task. `max_words == 0` means no word limit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Limits {
    pub max_words: u32,
    pub required_headings: Vec<String>,
}

/// Words a reader would count: tokens with at least one letter or digit, so Markdown
/// markers like `-`, `|`, `---` and `**` don't count.
#[must_use]
pub fn word_count(text: &str) -> usize {
    text.split_whitespace().filter(|t| t.chars().any(char::is_alphanumeric)).count()
}

/// Lowercase letter-and-digit runs: `## Pricing: Delivery` → `pricing`, `delivery`.
fn words_of(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// The words of every ATX heading (`#` to `######` then a space or tab), skipping
/// fenced and indented code, where `#` starts a comment, not a heading.
/// Each line of `text` with its heading text if it is a Markdown heading (level 1-6,
/// a space or tab after the `#`s, not in a code fence or a 4-space indent; so a
/// `#hashtag` isn't one).
fn lines_with_headings(text: &str) -> Vec<(&str, Option<&str>)> {
    let mut in_fence = false;
    let mut out = Vec::new();
    for raw in text.lines() {
        let indent = raw.len() - raw.trim_start_matches(' ').len();
        let line = raw.trim_start();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            out.push((raw, None));
            continue;
        }
        let level = line.chars().take_while(|&c| c == '#').count();
        let rest = &line[level..];
        let heading = !in_fence && indent < 4 && (1..=6).contains(&level) && (rest.is_empty() || rest.starts_with([' ', '\t']));
        out.push((raw, heading.then(|| rest.trim())));
    }
    out
}

fn headings(text: &str) -> Vec<Vec<String>> {
    lines_with_headings(text).into_iter().filter_map(|(_, h)| h.map(words_of)).collect()
}

/// Words per section (the text before the first heading is "(opening)"), by the same
/// heading rules as the checks: for showing a too-long draft where its words are.
#[must_use]
pub fn sections(text: &str) -> Vec<(String, usize)> {
    let mut out = vec![("(opening)".to_string(), 0)];
    for (line, heading) in lines_with_headings(text) {
        match heading {
            Some(h) => out.push((if h.is_empty() { "(untitled)".to_string() } else { h.to_string() }, 0)),
            None => {
                if let Some(last) = out.last_mut() {
                    last.1 += word_count(line);
                }
            }
        }
    }
    out.retain(|(_, n)| *n > 0);
    out
}

/// True when `wanted` appears as whole consecutive words: "Pricing" matches
/// "Delivery pricing" but "Plan" does not match "Explanation".
fn has_words(heading: &[String], wanted: &[String]) -> bool {
    heading.windows(wanted.len()).any(|w| w == wanted)
}

/// How far over a word limit still passes: 5%, at least one word.
#[must_use]
pub fn slack(limit: usize) -> usize {
    (limit / 20).max(1)
}

/// Every limit the text breaks, as an unmet criterion. Empty = all limits met.
/// A required heading with no words in it (e.g. `""` or `"##"`) is ignored.
#[must_use]
pub fn check(text: &str, limits: &Limits) -> Vec<String> {
    let mut failed = Vec::new();
    let words = word_count(text);
    // A word limit reads the way an editor does: "a 100-word description" is met by
    // 102 words. 5% over (at least 1 word) passes; the agent is told to aim lower (B9:
    // drafts 2-3% over were rejected three times running and sank whole tasks).
    let limit = limits.max_words as usize;
    if limit > 0 && words > limit + slack(limit) {
        // Models cut too little when told the exact overshoot (B9: "cut 2" failed three
        // drafts running), so the target leaves a tenth of the limit as margin.
        let target = (limit * 9 / 10).max(1);
        failed.push(format!("at most {} words (it has {words}; cut it to about {target})", limits.max_words));
    }
    let found = headings(text);
    for required in &limits.required_headings {
        let wanted = words_of(required);
        if !wanted.is_empty() && !found.iter().any(|h| has_words(h, &wanted)) {
            failed.push(format!("has a Markdown heading \"{}\"", required.trim().trim_start_matches('#').trim()));
        }
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn markdown_markers_are_not_words() {
        assert_eq!(word_count("## Pricing\n\n- **Base fee:** ₹40 | ---"), 4);
    }

    #[test]
    fn reports_each_broken_limit_in_words_the_worker_can_act_on() {
        let limits = Limits { max_words: 3, required_headings: vec!["Pricing".into(), "Schedule".into()] };
        let failed = check("# Launch plan\n\n## pricing: delivery\nFree above ₹500.", &limits);
        assert_eq!(failed, vec!["at most 3 words (it has 7; cut it to about 2)", "has a Markdown heading \"Schedule\""]);
    }

    #[test]
    fn a_word_limit_allows_what_an_editor_would() {
        let limits = Limits { max_words: 100, required_headings: vec![] };
        assert_eq!(check(&"word ".repeat(102), &limits), Vec::<String>::new(), "102 words is a 100-word piece");
        assert_eq!(check(&"word ".repeat(105), &limits), Vec::<String>::new());
        assert_eq!(check(&"word ".repeat(106), &limits), ["at most 100 words (it has 106; cut it to about 90)"]);
        // Short limits allow one word over, not more (a 5-word tagline isn't 7).
        assert_eq!(check("a b c d", &Limits { max_words: 3, required_headings: vec![] }), Vec::<String>::new());
        assert_eq!(check("a b c d e f g", &Limits { max_words: 5, required_headings: vec![] }).len(), 1);
    }

    #[test]
    fn headings_match_whole_words_whatever_the_planner_wrote() {
        let draft = "## Delivery Pricing\n## Explanation";
        let limits = Limits { max_words: 0, required_headings: vec!["## Pricing".into(), "Plan".into(), String::new(), "##".into()] };
        assert_eq!(check(draft, &limits), vec!["has a Markdown heading \"Plan\""]);
    }

    #[test]
    fn comments_in_code_are_not_headings() {
        let limits = Limits { max_words: 0, required_headings: vec!["Setup".into()] };
        assert_eq!(check("```bash\n# Setup steps\n```\n    # Setup", &limits).len(), 1);
        assert_eq!(check("#\tSetup", &limits), Vec::<String>::new());
    }

    #[test]
    fn sections_count_words_under_real_headings_only() {
        let text = "Intro here.\n# Posts\n#StayHydrated #CampusLife go\n```\n# not a heading\n```\n## Tagline\nSip smart";
        assert_eq!(sections(text), [("(opening)".to_string(), 2), ("Posts".to_string(), 6), ("Tagline".to_string(), 2)]);
    }

    #[test]
    fn bold_text_and_hashtags_are_not_headings() {
        let limits = Limits { max_words: 0, required_headings: vec!["Pricing".into()] };
        assert_eq!(check("**Pricing**\n#Pricing\n####### Pricing", &limits).len(), 1);
    }

    proptest! {
        #[test]
        fn no_limits_means_no_failures(text in ".{0,400}") {
            prop_assert!(check(&text, &Limits::default()).is_empty());
        }

        #[test]
        fn word_limit_fails_exactly_when_past_its_slack(words in 0usize..200, max in 1u32..150) {
            let text = vec!["word"; words].join(" ");
            let failed = check(&text, &Limits { max_words: max, required_headings: vec![] });
            let max = max as usize;
            prop_assert_eq!(!failed.is_empty(), words > max + (max / 20).max(1));
        }
    }
}
