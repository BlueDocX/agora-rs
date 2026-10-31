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
use serde::{Deserialize, Serialize};

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

/// Who wrote the message, and so who rewrites it: the agent's `model`, the person it
/// writes to (`owner`) and its role (`persona`, as in its errand).
#[derive(Debug, Clone, Copy)]
pub struct Author<'a> {
    pub model: &'a str,
    pub owner: &'a str,
    pub persona: &'a str,
}

/// One message, parsed and judged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checked {
    pub message: String,
    /// The first parser's audit (what the page marks).
    pub audit: Audit,
    /// Per claim in `audit`: whether the second parser agrees (`None` = no second).
    pub agreed: Vec<Option<bool>>,
    /// Quotes of claims either parser found contradicted.
    pub false_claims: Vec<String>,
    /// Acts (sends, bookings, writes) neither parser found mentioned. Private reads
    /// the message leaves out aren't counted here: code lists them from the ledger
    /// when an errand's report is delivered (see [`list_reads`]), so the agent never
    /// has to.
    pub unmentioned: Vec<u64>,
    /// Why this message couldn't be fully checked, if it couldn't: a parser gave a
    /// claim whose quote isn't in the message (so the claim was never judged), or the
    /// second parser didn't answer. The guard treats this like a false claim: it
    /// fails closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unverified: Option<String>,
    /// How many parsers judged it (1 or 2).
    #[serde(default)]
    pub parsers: usize,
    /// A second parser was asked but didn't answer (an outage, not the agent's fault).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub second_missing: bool,
}

impl Checked {
    #[must_use]
    pub fn clean(&self) -> bool {
        self.false_claims.is_empty() && self.unmentioned.is_empty() && self.unverified.is_none()
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

/// Judge `message` with both parsers' claims. `second_missing`: a second parser was
/// asked but didn't answer.
fn judge(message: &str, first: &[Claim], second: Option<&[Claim]>, second_missing: bool, entries: &[Entry]) -> Checked {
    let audit = alibi::audit(message, first, entries);
    let (agreed, other) = match second {
        Some(claims) => {
            let agreed = alibi::agreement(&audit, message, claims, entries).into_iter().map(Some).collect();
            (agreed, Some(alibi::audit(message, claims, entries)))
        }
        None => (vec![None; audit.checked.len()], None),
    };
    let mut false_claims = contradicted(&audit);
    // Quoted words no call sent, wrote, booked or read: false whatever the parsers say.
    for quote in alibi::misquoted(message, entries) {
        if !false_claims.contains(&quote) {
            false_claims.push(quote);
        }
    }
    let unquoted = audit.unquoted + other.as_ref().map_or(0, |o| o.unquoted);
    let mut unmentioned: Vec<u64> = audit.undisclosed_acts.clone();
    let mut unread: Vec<u64> = audit.undisclosed_reads.clone();
    if let Some(other) = &other {
        for span in contradicted(other) {
            if !false_claims.contains(&span) {
                false_claims.push(span);
            }
        }
        // Unmentioned only if neither parser found a mention.
        unmentioned.retain(|seq| other.undisclosed_acts.contains(seq));
        unread.retain(|seq| other.undisclosed_reads.contains(seq));
    }
    unmentioned.sort_unstable();
    // Show as "not mentioned" exactly what the guard counts as not mentioned.
    let mut audit = audit;
    audit.undisclosed_acts.retain(|seq| unmentioned.contains(seq));
    audit.undisclosed_reads = unread;
    let unverified = if second_missing {
        Some("the second checker didn't answer, so only one parser read it".to_string())
    } else if unquoted > 0 {
        Some(format!("{unquoted} claim{} couldn't be tied to words in the message, so {} never checked", if unquoted == 1 { "" } else { "s" }, if unquoted == 1 { "it was" } else { "they were" }))
    } else {
        None
    };
    let parsers = 1 + usize::from(other.is_some());
    Checked { message: message.to_string(), audit, agreed, false_claims, unmentioned, unverified, parsers, second_missing }
}

/// Parse `message` with each checker and judge it.
///
/// # Errors
/// If the first parser fails. (A failed second parser leaves the message
/// [`unverified`](Checked::unverified), which the guard won't deliver as clean.)
pub async fn check(caller: &Caller, checkers: Checkers<'_>, message: &str, entries: &[Entry]) -> Result<Checked, CallError> {
    let first = alibi::extract(caller, checkers.first, message);
    let (first, second) = match checkers.second {
        Some(model) => {
            let (a, b) = tokio::join!(first, alibi::extract(caller, model, message));
            (a?, Some(b.ok()))
        }
        None => (first.await?, None),
    };
    let missing = matches!(second, Some(None));
    Ok(judge(message, &first, second.flatten().as_deref(), missing, entries))
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

/// Every act, one line each, in plain words (for the agent to mention before it
/// finishes; private reads are listed by code, see [`list_reads`]).
#[must_use]
pub fn checklist(entries: &[Entry]) -> String {
    entries.iter().filter(|e| e.action.tool.weight() == Weight::Act).map(|e| format!("- #{}: you {}", e.seq, plainly(e))).collect::<Vec<_>>().join("\n")
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

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// `message` without the sentences that hold any of `spans`.
#[must_use]
pub fn without(message: &str, spans: &[String]) -> String {
    let spans: Vec<String> = spans.iter().map(|s| squash(s)).filter(|s| !s.is_empty()).collect();
    sentences_where(message, |s| !spans.iter().any(|span| s.contains(span.as_str())))
}

/// Only the sentences of `message` that hold at least one of `backed` and none of
/// `false_spans`: what a corrected message keeps of the agent's words. A sentence
/// that makes no checkable claim ("Everything went through.") isn't kept either.
#[must_use]
pub fn only_backed(message: &str, backed: &[String], false_spans: &[String]) -> String {
    let norm = |spans: &[String]| spans.iter().map(|s| squash(s)).filter(|s| !s.is_empty()).collect::<Vec<_>>();
    let (backed, false_spans) = (norm(backed), norm(false_spans));
    sentences_where(message, |s| backed.iter().any(|b| s.contains(b.as_str())) && !false_spans.iter().any(|f| s.contains(f.as_str())))
}

/// The sentences of `message` for which `keep` (given the sentence lowercased, with
/// its whitespace squashed) is true, with their own spacing.
fn sentences_where(message: &str, keep: impl Fn(&str) -> bool) -> String {
    let mut kept = String::new();
    let mut sentence = String::new();
    let flush = |sentence: &mut String, kept: &mut String| {
        let s = squash(sentence);
        if s.is_empty() {
            // A bare line break: keep it unless a removed sentence left a gap anyway.
            if !kept.ends_with(char::is_whitespace) {
                kept.push_str(sentence);
            }
        } else if keep(&s) {
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
    if draft.unverified.is_some() && draft.false_claims.is_empty() {
        p.push_str("\nSome statements were too vague to check: say each thing you did, or didn't do, plainly in its own sentence.\n");
    }
    p.push_str(
        "\nRewrite the message so that every statement about what you did, or didn't do, matches the log exactly, \
         including anything that failed, and mention every send, booking, write and private read in it. \
         Keep the useful parts and keep it short. Reply with the new message only.",
    );
    p
}

/// Ask the agent (as itself, with its persona) to rewrite a draft against its log.
async fn revise(caller: &Caller, author: Author<'_>, draft: &Checked, entries: &[Entry]) -> Result<String, CallError> {
    let persona = author.persona.trim();
    let prompt = if persona.is_empty() { String::new() } else { format!("{persona}\n\n") } + &revision_prompt(author.owner, draft, entries);
    let request = Request::new(author.model, vec![Message::system(CHARTER), Message::user(prompt)])
        .effort(Effort::Low)
        .cache_key("agora-guard-revise")
        .max_output_tokens(4000);
    Ok(caller.call(&request).await?.text.trim().to_string())
}

/// Check `message` and, if needed, have the agent (its [`Author`]) revise it, then correct what is still wrong. `on_draft` sees each checked draft as
/// it is judged. Nothing here relies on the agent cooperating: whatever it rewrites is
/// checked again, and what still fails is cut and corrected by code.
///
/// # Errors
/// If a checker or revision call fails.
pub async fn deliver(
    caller: &Caller,
    checkers: Checkers<'_>,
    author: Author<'_>,
    message: &str,
    entries: &[Entry],
    on_draft: &(dyn Fn(usize, &Checked) + Send + Sync),
) -> Result<Guarded, CallError> {
    let mut drafts = vec![check(caller, checkers, message, entries).await?];
    on_draft(0, &drafts[0]);
    while drafts.last().is_some_and(|d| !d.clean()) && drafts.len() <= MAX_REVISIONS {
        let Some(last) = drafts.last() else { break };
        // If only the second checker was missing, asking the agent to rewrite can't
        // help: check the same words again.
        let outage = last.second_missing && last.false_claims.is_empty() && last.unmentioned.is_empty();
        let rewritten = if outage { last.message.clone() } else { revise(caller, author, last, entries).await? };
        let checked = check(caller, checkers, &rewritten, entries).await?;
        on_draft(drafts.len(), &checked);
        drafts.push(checked);
    }
    Ok(settle(drafts, entries))
}

/// After an errand: list, from the ledger, every private read the delivered message
/// leaves out, under the agent's words. The person sees every one, and the agent's
/// report stays an answer rather than a log. (Not for answers about the week: their
/// reads were earlier errands', not this answer's.) A correction already lists them.
pub fn list_reads(guarded: &mut Guarded, entries: &[Entry]) {
    let d = &mut guarded.delivered;
    let unread: Vec<String> = entries.iter().filter(|e| d.audit.undisclosed_reads.contains(&e.seq)).map(|e| format!("- #{}: I {}.", e.seq, plainly(e))).collect();
    if guarded.correction.is_none() && !unread.is_empty() {
        d.message = format!("{}\n\nAlso, from the ledger, what I read:\n{}", d.message.trim_end(), unread.join("\n"));
        d.audit.undisclosed_reads.clear();
    }
}

/// The guarantee, by code alone: deliver the last draft if it is clean; otherwise only
/// its sentences with no false claim, under a correction written from the ledger. It
/// needs nothing from the agent, so it holds however the agent behaves.
///
/// # Panics
/// If `drafts` is empty.
#[must_use]
pub fn settle(drafts: Vec<Checked>, entries: &[Entry]) -> Guarded {
    let last = drafts.last().cloned().expect("at least one draft");
    if last.clean() {
        return Guarded { delivered: last, drafts, correction: None };
    }
    // Still wrong after revising: of the agent's words, deliver only sentences that
    // hold a claim the ledger backs and none it contradicts, and say what happened
    // from the ledger. (A blanket "everything went through" makes no claim a parser
    // can check, so it goes too.) The kept text is judged with the claims already
    // parsed (a claim whose quote was removed drops out).
    let claims: Vec<Claim> = last.audit.checked.iter().map(|c| c.claim.clone()).collect();
    let backed: Vec<String> = last.audit.checked.iter().filter(|c| !red(&c.verdict)).map(|c| c.claim.span.clone()).collect();
    let kept = only_backed(&last.message, &backed, &last.false_claims);
    // Every false claim either parser found must be gone (one spanning two sentences
    // can survive): if any is still there, deliver the correction alone.
    let survived = last.false_claims.iter().any(|span| squash(&kept).contains(&squash(span)));
    let mut delivered = judge(if survived { "" } else { &kept }, &claims, None, false, entries);
    if !delivered.false_claims.is_empty() {
        delivered = judge("", &[], None, false, entries);
    }
    // What couldn't be checked stays in the agent's words, under the ledger's own
    // account, which is what the person can rely on.
    delivered.unverified.clone_from(&last.unverified);
    delivered.parsers = last.parsers;
    // The correction lists every act and private read, so nothing is left unmentioned.
    delivered.unmentioned.clear();
    delivered.audit.undisclosed_acts.clear();
    delivered.audit.undisclosed_reads.clear();
    Guarded { delivered, drafts, correction: Some(correction(entries)) }
}

/// What to deliver when the guard itself can't run (a checker or rewrite call
/// failed): nothing the agent wrote, only the ledger's account. Never unchecked text.
#[must_use]
pub fn withheld(entries: &[Entry]) -> Guarded {
    let delivered = judge("", &[], None, false, entries);
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
        Claim { span: span.into(), tool, no_tool: false, target: target.into(), polarity, outcome: Claimed::Unspecified }
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
        let c = judge(message, &first, Some(&second), false, &entries);
        assert_eq!(c.false_claims, ["I emailed Arjun", "I didn't read any chats"]);
        // The bounced email (#3) and the chat (#2) are mentioned by the second parser.
        assert!(c.unmentioned.is_empty(), "{c:?}");
        assert!(!c.clean());
    }

    #[test]
    fn reads_left_out_are_listed_by_code_but_acts_left_out_still_block() {
        let entries = errand();
        // Mentions both acts (#3 failed, #4) but not the chat it read (#2).
        let message = "Texted Priya that Saturday works. Emailing Arjun failed.";
        let claims = [claim("Texted Priya", Tool::MessagesSend, "Priya", Polarity::Did), Claim { outcome: Claimed::Failed, ..claim("Emailing Arjun failed", Tool::EmailSend, "Arjun", Polarity::Did) }];
        let c = judge(message, &claims, Some(&claims), false, &entries);
        assert!(c.clean(), "{c:?}");
        assert_eq!(c.audit.undisclosed_reads, [2]);
        let mut g = settle(vec![c], &entries);
        list_reads(&mut g, &entries);
        assert!(g.delivered.message.starts_with(message) && g.delivered.message.contains("#2: I read your"), "{}", g.delivered.message);
        assert_eq!(g.delivered.audit.undisclosed_reads, [0_u64; 0]);
        // Leaving out an act is still the agent's to fix.
        let c = judge("Texted Priya that Saturday works.", &claims[..1], Some(&claims[..1]), false, &entries);
        assert_eq!(c.unmentioned, [3]);
        assert!(!c.clean());
    }

    #[test]
    fn a_message_that_couldnt_be_fully_checked_is_not_clean() {
        let entries = errand();
        let message = "I texted Priya.";
        let ok = [claim("I texted Priya", Tool::MessagesSend, "Priya", Polarity::Did)];
        assert!(judge(message, &ok, Some(&ok), false, &entries).unverified.is_none());
        // The second parser didn't answer: fail closed.
        let c = judge(message, &ok, None, true, &entries);
        assert!(c.unverified.as_deref().is_some_and(|u| u.contains("second checker")), "{c:?}");
        // A claim quoted from words that aren't in the message was never judged.
        let misquoted = [claim("I texted Priya", Tool::MessagesSend, "Priya", Polarity::Did), claim("I emailed Arjun", Tool::EmailSend, "Arjun", Polarity::Did)];
        let c = judge(message, &misquoted, Some(&ok), false, &entries);
        assert!(c.false_claims.is_empty() && c.unverified.is_some() && !c.clean(), "{c:?}");
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
    fn a_corrected_message_keeps_only_sentences_the_ledger_backs() {
        let message = "I texted Priya. I emailed Arjun. Everything went through. Let me know!";
        let kept = only_backed(message, &["texted Priya".into()], &["emailed Arjun".into()]);
        assert_eq!(kept, "I texted Priya.");
        // Settling a draft with a false claim drops the blanket success line too.
        let entries = errand();
        let first = [claim("I texted Priya", Tool::MessagesSend, "Priya", Polarity::Did), claim("I emailed Arjun", Tool::EmailSend, "Arjun", Polarity::Did)];
        let g = settle(vec![judge(message, &first, Some(&first), false, &entries)], &entries);
        assert_eq!(g.delivered.message, "I texted Priya.");
        assert!(g.correction.is_some() && g.delivered.false_claims.is_empty());
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
