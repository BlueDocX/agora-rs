//! The guard: no message reaches the person with a claim the ledger contradicts, or
//! with an action or private read left out.
//!
//! Three layers:
//! 1. **Prevent.** Asked about an errand later, the agent answers from its ledger
//!    ([`log`]), not from memory: lost context is why agents deny what they did.
//! 2. **Catch and revise.** Every draft is checked before it is delivered. Here a
//!    claim counts as false if *either* parser finds it contradicted (stricter than
//!    the display, which asks both to agree). A draft with problems goes back to the
//!    agent with the exact receipts, up to [`MAX_REVISIONS`] times.
//! 3. **Guarantee.** A draft that still fails is not delivered as written: every
//!    sentence holding a false claim is removed, and a correction written by code
//!    from the ledger (so it can't misreport) says what was actually done.

use std::fmt::Write as _;

use agora_core::{Tool, Weight};
use agora_muse::{Effort, Message, Request};
use serde::Serialize;

use crate::CHARTER;
use crate::alibi::{self, Audit, Claim, Verdict};
use crate::call::{CallError, Caller};
use crate::ledger::Entry;

pub const MAX_REVISIONS: usize = 2;
/// How much of each call's result the log shows (enough for an email body).
const LOG_RESULT_CHARS: usize = 400;

/// The models that parse messages into claims: one, or two that check each other.
#[derive(Debug, Clone, Copy)]
pub struct Checkers<'a> {
    pub first: &'a str,
    pub second: Option<&'a str>,
}

/// One message, parsed and judged.
#[derive(Debug, Clone, Serialize)]
pub struct Checked {
    pub message: String,
    /// The first parser's audit (what the page marks).
    pub audit: Audit,
    /// Per claim in `audit`: whether the second parser agrees (`None` = no second).
    pub agreed: Vec<Option<bool>>,
    /// Quotes of claims either parser found contradicted.
    pub false_claims: Vec<String>,
    /// Acts and private reads neither parser found mentioned.
    pub unmentioned: Vec<u64>,
}

impl Checked {
    #[must_use]
    pub fn clean(&self) -> bool {
        self.false_claims.is_empty() && self.unmentioned.is_empty()
    }
}

/// What reached the person, and how.
#[derive(Debug, Clone, Serialize)]
pub struct Guarded {
    /// Every draft the agent wrote, in order; the last one is what it ended with.
    pub drafts: Vec<Checked>,
    /// The agent's words as delivered (the last draft, minus any removed sentences),
    /// checked again.
    pub delivered: Checked,
    /// Added by code from the ledger when the agent's own words couldn't be fixed.
    pub correction: Option<String>,
}

fn red(v: &Verdict) -> bool {
    matches!(v, Verdict::Contradicted { .. })
}

fn contradicted(audit: &Audit) -> Vec<String> {
    audit.checked.iter().filter(|c| red(&c.verdict)).map(|c| c.claim.span.clone()).collect()
}

/// Judge `message` with both parsers' claims.
fn judge(message: &str, first: &[Claim], second: Option<&[Claim]>, entries: &[Entry]) -> Checked {
    let audit = alibi::audit(message, first, entries);
    let (agreed, other) = match second {
        Some(claims) => {
            let agreed = alibi::agreement(&audit, message, claims, entries).into_iter().map(Some).collect();
            (agreed, Some(alibi::audit(message, claims, entries)))
        }
        None => (vec![None; audit.checked.len()], None),
    };
    let mut false_claims = contradicted(&audit);
    let mut unmentioned: Vec<u64> = audit.undisclosed_acts.iter().chain(&audit.undisclosed_reads).copied().collect();
    if let Some(other) = &other {
        for span in contradicted(other) {
            if !false_claims.contains(&span) {
                false_claims.push(span);
            }
        }
        // Unmentioned only if neither parser found a mention.
        let theirs: Vec<u64> = other.undisclosed_acts.iter().chain(&other.undisclosed_reads).copied().collect();
        unmentioned.retain(|seq| theirs.contains(seq));
    }
    unmentioned.sort_unstable();
    // Show as "not mentioned" exactly what the guard counts as not mentioned.
    let mut audit = audit;
    audit.undisclosed_acts.retain(|seq| unmentioned.contains(seq));
    audit.undisclosed_reads.retain(|seq| unmentioned.contains(seq));
    Checked { message: message.to_string(), audit, agreed, false_claims, unmentioned }
}

/// Parse `message` with each checker and judge it.
///
/// # Errors
/// If the first parser fails. (A failed second parser only removes its vote.)
pub async fn check(caller: &Caller, checkers: Checkers<'_>, message: &str, entries: &[Entry]) -> Result<Checked, CallError> {
    let first = alibi::extract(caller, checkers.first, message);
    let (first, second) = match checkers.second {
        Some(model) => {
            let (a, b) = tokio::join!(first, alibi::extract(caller, model, message));
            (a?, b.ok())
        }
        None => (first.await?, None),
    };
    Ok(judge(message, &first, second.as_deref(), entries))
}

/// One line per ledger entry, as the agent and the person see it.
#[must_use]
pub fn log(entries: &[Entry]) -> String {
    entries.iter().fold(String::new(), |mut s, e| {
        let o = &e.outcome;
        // The whole result (lines joined, capped): a rewrite must be able to cite what
        // a read returned, not just its first line.
        let what = if e.forgotten_at_ms.is_some() {
            "(content forgotten at the person's request)".to_string()
        } else {
            let flat = o.result.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" / ");
            if flat.chars().count() > LOG_RESULT_CHARS { format!("{}…", flat.chars().take(LOG_RESULT_CHARS).collect::<String>()) } else { flat }
        };
        let _ = writeln!(s, "#{} {} · {} → {}: {what}", e.seq, e.action.tool.name(), o.subject, if o.ok { "ok" } else { "FAILED" });
        s
    })
}

/// What an entry did, in plain words, for the correction.
fn plainly(e: &Entry) -> String {
    let o = &e.outcome;
    if e.forgotten_at_ms.is_some() {
        return format!("used {} (what it involved was forgotten at your request)", e.action.tool.name());
    }
    let did = match e.action.tool {
        Tool::ContactsRead => format!("read {}'s contact card, including your notes on them", o.subject),
        Tool::EmailRead | Tool::FilesRead => format!("read {}", o.subject),
        Tool::MessagesRead => format!("read your {}", o.subject),
        Tool::EmailSend => format!("emailed {}", o.subject),
        Tool::MessagesSend => format!("texted {}", o.subject),
        Tool::CalendarCreate => format!("booked {}", o.subject),
        Tool::FilesWrite => format!("saved {}", o.subject),
        other => format!("looked up {} ({})", o.subject, other.name()),
    };
    if o.ok { did } else { format!("tried: {did}, but it failed ({})", o.result.trim_end_matches('.')) }
}

/// The correction: every act and private read, from the ledger, by code.
#[must_use]
pub fn correction(entries: &[Entry]) -> String {
    let lines: Vec<String> = entries.iter().filter(|e| e.action.tool.weight() != Weight::Lookup).map(|e| format!("- #{}: I {}.", e.seq, plainly(e))).collect();
    if lines.is_empty() {
        "Correction from the ledger: I didn't send, book, write or read anything private.".to_string()
    } else {
        format!("Correction from the ledger, what I actually did:\n{}", lines.join("\n"))
    }
}

const ABBREVIATIONS: [&str; 6] = ["mr.", "mrs.", "ms.", "dr.", "e.g.", "i.e."];

/// `message` without the sentences that hold any of `spans`.
#[must_use]
pub fn without(message: &str, spans: &[String]) -> String {
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let spans: Vec<String> = spans.iter().map(|s| squash(s)).filter(|s| !s.is_empty()).collect();
    let mut kept = String::new();
    let mut sentence = String::new();
    let flush = |sentence: &mut String, kept: &mut String| {
        let s = squash(sentence);
        if s.is_empty() {
            // A bare line break: keep it unless a removed sentence left a gap anyway.
            if !kept.ends_with(char::is_whitespace) {
                kept.push_str(sentence);
            }
        } else if !spans.iter().any(|span| s.contains(span.as_str())) {
            kept.push_str(sentence);
        }
        sentence.clear();
    };
    let mut chars = message.chars().peekable();
    while let Some(c) = chars.next() {
        sentence.push(c);
        // "Mr. Shah" doesn't end a sentence.
        let abbreviation = c == '.'
            && ABBREVIATIONS.iter().any(|a| {
                let cut = sentence.len().saturating_sub(a.len());
                sentence.is_char_boundary(cut)
                    && sentence[cut..].eq_ignore_ascii_case(a)
                    && sentence[..cut].chars().next_back().is_none_or(|p| !p.is_alphanumeric())
            });
        let ends = matches!(c, '.' | '!' | '?') && !abbreviation && chars.peek().is_none_or(|n| n.is_whitespace()) || c == '\n';
        if ends {
            // Keep the whitespace that follows with this sentence.
            while let Some(&n) = chars.peek() {
                if !n.is_whitespace() || n == '\n' && c != '\n' {
                    break;
                }
                sentence.push(n);
                chars.next();
            }
            flush(&mut sentence, &mut kept);
        }
    }
    flush(&mut sentence, &mut kept);
    kept.trim().to_string()
}

fn revision_prompt(owner: &str, draft: &Checked, entries: &[Entry]) -> String {
    let mut p = format!("You wrote this message to {owner}:\n<<<\n{}\n>>>\n\nYour action log (written by the system as you worked; it is the record of what you did):\n{}\n", draft.message, log(entries));
    if !draft.false_claims.is_empty() {
        p.push_str("\nThese statements don't match the log:\n");
        for span in &draft.false_claims {
            let _ = writeln!(p, "- \"{span}\"");
        }
    }
    if !draft.unmentioned.is_empty() {
        let seqs: Vec<String> = draft.unmentioned.iter().map(|n| format!("#{n}")).collect();
        let _ = writeln!(p, "\nYou didn't mention these, and the person should know about them: {}", seqs.join(", "));
    }
    p.push_str(
        "\nRewrite the message so that every statement about what you did, or didn't do, matches the log exactly, \
         including anything that failed, and mention every send, booking, write and private read in it. \
         Keep the useful parts and keep it short. Reply with the new message only.",
    );
    p
}

/// Ask the agent to rewrite a draft against its log.
async fn revise(caller: &Caller, model: &str, owner: &str, draft: &Checked, entries: &[Entry]) -> Result<String, CallError> {
    let request = Request::new(model, vec![Message::system(CHARTER), Message::user(revision_prompt(owner, draft, entries))])
        .effort(Effort::Low)
        .cache_key("agora-guard-revise")
        .max_output_tokens(2000);
    Ok(caller.call(&request).await?.text.trim().to_string())
}

/// Check `message` and, if needed, have the agent (`model`) revise it, then correct
/// what is still wrong. `on_draft` sees each checked draft as it is judged.
///
/// # Errors
/// If a checker or revision call fails.
pub async fn deliver(
    caller: &Caller,
    checkers: Checkers<'_>,
    model: &str,
    owner: &str,
    message: &str,
    entries: &[Entry],
    on_draft: &(dyn Fn(usize, &Checked) + Send + Sync),
) -> Result<Guarded, CallError> {
    let mut drafts = vec![check(caller, checkers, message, entries).await?];
    on_draft(0, &drafts[0]);
    while drafts.last().is_some_and(|d| !d.clean()) && drafts.len() <= MAX_REVISIONS {
        let Some(last) = drafts.last() else { break };
        let rewritten = revise(caller, model, owner, last, entries).await?;
        let checked = check(caller, checkers, &rewritten, entries).await?;
        on_draft(drafts.len(), &checked);
        drafts.push(checked);
    }
    let Some(last) = drafts.last().cloned() else { unreachable!("there is always a first draft") };
    if last.clean() {
        return Ok(Guarded { delivered: last, drafts, correction: None });
    }
    // Still wrong after revising: deliver only the sentences with no false claim, and
    // say what happened from the ledger. The kept text is judged with the claims
    // already parsed (a claim whose quote was removed drops out).
    let claims: Vec<Claim> = last.audit.checked.iter().map(|c| c.claim.clone()).collect();
    let kept = without(&last.message, &last.false_claims);
    // Every false claim either parser found must be gone (one spanning two sentences
    // can survive `without`): if any is still there, deliver the correction alone.
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let survived = last.false_claims.iter().any(|span| squash(&kept).contains(&squash(span)));
    let mut delivered = judge(if survived { "" } else { &kept }, &claims, None, entries);
    if !delivered.false_claims.is_empty() {
        delivered = judge("", &[], None, entries);
    }
    // The correction lists every act and private read, so nothing is left unmentioned.
    delivered.unmentioned.clear();
    delivered.audit.undisclosed_acts.clear();
    delivered.audit.undisclosed_reads.clear();
    Ok(Guarded { delivered, drafts, correction: Some(correction(entries)) })
}

/// What to deliver when the guard itself can't run (a checker or rewrite call
/// failed): nothing the agent wrote, only the ledger's account. Never unchecked text.
#[must_use]
pub fn withheld(entries: &[Entry]) -> Guarded {
    let delivered = judge("", &[], None, entries);
    Guarded { drafts: Vec::new(), delivered, correction: Some(correction(entries)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alibi::{Claimed, Polarity};
    use crate::ledger::Ledger;
    use agora_core::{Action, World};

    fn errand() -> Vec<Entry> {
        let ledger = Ledger::in_memory("run-g").unwrap();
        let mut world = World::demo();
        for (tool, target) in [(Tool::ContactsSearch, "Priya"), (Tool::MessagesRead, "Neha"), (Tool::EmailSend, "arjun.shah@oldmail.in"), (Tool::MessagesSend, "Priya")] {
            let action = Action { tool, target: target.into(), when: String::new(), text: "Saturday works!".into() };
            let outcome = world.execute(&action);
            ledger.append("Scout", &action, &outcome).unwrap();
        }
        ledger.entries().unwrap()
    }

    fn claim(span: &str, tool: Tool, target: &str, polarity: Polarity) -> Claim {
        Claim { span: span.into(), tool, target: target.into(), polarity, outcome: Claimed::Unspecified }
    }

    #[test]
    fn either_parser_finding_a_false_claim_is_enough_to_send_it_back() {
        let entries = errand();
        let message = "I texted Priya. I emailed Arjun. I didn't read any chats.";
        let first = [claim("I texted Priya", Tool::MessagesSend, "Priya", Polarity::Did)];
        let second = [
            claim("I texted Priya", Tool::MessagesSend, "Priya", Polarity::Did),
            claim("I emailed Arjun", Tool::EmailSend, "Arjun", Polarity::Did),
            claim("I didn't read any chats", Tool::MessagesRead, "", Polarity::DidNot),
        ];
        let c = judge(message, &first, Some(&second), &entries);
        assert_eq!(c.false_claims, ["I emailed Arjun", "I didn't read any chats"]);
        // The bounced email (#3) and the chat (#2) are mentioned by the second parser.
        assert!(c.unmentioned.is_empty(), "{c:?}");
        assert!(!c.clean());
    }

    #[test]
    fn false_sentences_are_removed_and_the_rest_kept() {
        let message = "Done! I texted Priya about Saturday. I also emailed Arjun.\nI did not read any of your chats. Anything else?";
        let kept = without(message, &["emailed Arjun".into(), "did not read any of your chats".into()]);
        assert_eq!(kept, "Done! I texted Priya about Saturday. Anything else?");
        assert_eq!(without("Booked it.\nTexted Priya.", &["emailed".into()]), "Booked it.\nTexted Priya.");
        // An abbreviation doesn't split a sentence, so its false claim is removed whole.
        assert_eq!(without("I emailed Mr. Shah today. Done.", &["emailed Mr. Shah".into()]), "Done.");
    }

    #[test]
    fn the_correction_lists_every_act_and_private_read_from_the_ledger() {
        let text = correction(&errand());
        assert_eq!(
            text,
            "Correction from the ledger, what I actually did:\n\
             - #2: I read your chat with Neha Iyer.\n\
             - #3: I tried: emailed Arjun Shah <arjun.shah@oldmail.in>, but it failed (Bounced: the mailbox arjun.shah@oldmail.in no longer exists).\n\
             - #4: I texted Priya Menon (+91 98450 11223)."
        );
    }

    #[test]
    fn the_log_shows_failures_and_forgotten_entries_plainly() {
        let entries = errand();
        let text = log(&entries);
        assert!(text.contains("#3 email.send · Arjun Shah <arjun.shah@oldmail.in> → FAILED: Bounced"), "{text}");
        // A read's whole result is there, lines joined.
        assert!(text.contains("Neha: How did the Kestrel first round go?? / Asha: Good I think!"), "{text}");
        assert_eq!(text.lines().count(), 4);
    }
}
