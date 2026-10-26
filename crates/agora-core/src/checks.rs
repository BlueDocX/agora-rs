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
fn headings(text: &str) -> Vec<Vec<String>> {
    let mut in_fence = false;
    let mut found = Vec::new();
    for line in text.lines() {
        let indent = line.len() - line.trim_start_matches(' ').len();
        let line = line.trim_start();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || indent >= 4 {
            continue;
        }
        let level = line.chars().take_while(|&c| c == '#').count();
        let rest = &line[level..];
        if (1..=6).contains(&level) && (rest.is_empty() || rest.starts_with([' ', '\t'])) {
            found.push(words_of(rest));
        }
    }
    found
}

/// True when `wanted` appears as whole consecutive words: "Pricing" matches
/// "Delivery pricing" but "Plan" does not match "Explanation".
fn has_words(heading: &[String], wanted: &[String]) -> bool {
    heading.windows(wanted.len()).any(|w| w == wanted)
}

/// Every limit the text breaks, as an unmet criterion. Empty = all limits met.
/// A required heading with no words in it (e.g. `""` or `"##"`) is ignored.
#[must_use]
pub fn check(text: &str, limits: &Limits) -> Vec<String> {
    let mut failed = Vec::new();
    let words = word_count(text);
    if limits.max_words > 0 && words > limits.max_words as usize {
        failed.push(format!("at most {} words (it has {words}; cut {})", limits.max_words, words - limits.max_words as usize));
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
        assert_eq!(failed, vec!["at most 3 words (it has 7; cut 4)", "has a Markdown heading \"Schedule\""]);
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
        fn word_limit_fails_exactly_when_exceeded(words in 0usize..60, max in 1u32..60) {
            let text = vec!["word"; words].join(" ");
            let failed = check(&text, &Limits { max_words: max, required_headings: vec![] });
            prop_assert_eq!(!failed.is_empty(), words > max as usize);
        }
    }
}
