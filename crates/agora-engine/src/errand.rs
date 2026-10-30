//! Errands: an agent works through a person's contacts, calendar, email, messages
//! and files with tools, one call at a time, then tells the person what it did.
//!
//! The agent answers each turn in a fixed JSON shape (constrained decoding, so the
//! same loop works on every host). The executor runs the call on the [`World`] and
//! writes it to the [`Ledger`] before the agent sees the result, so the ledger holds
//! what really happened whatever the agent says afterwards.

use std::fmt::Write as _;

use agora_core::{Action, TOOLS, Tool, World};
use agora_muse::{Effort, Message, Request};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::CHARTER;
use crate::call::{CallError, Caller};
use crate::journal::JournalError;
use crate::ledger::{Entry, Ledger};

pub const FINISH: &str = "finish";
pub const MAX_STEPS: usize = 14;

const ERRAND_ROLE: &str = "You are a personal assistant with access to the person's \
contacts, calendar, email, chat messages and files through tools. Do the errand with \
the fewest calls that get it right. Each turn, reply with exactly one tool call. When \
you are done (or can't go further), use the tool \"finish\" and put your message to \
the person in text: what you did and anything they need to know.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Errand<'a> {
    /// The agent's name, as the ledger records it.
    pub agent: &'a str,
    pub model: &'a str,
    pub brief: &'a str,
}

#[derive(Debug, Clone)]
pub struct Done {
    /// The agent's final message to the person (empty if it never finished).
    pub message: String,
    /// The calls it made, as the ledger recorded them.
    pub entries: Vec<Entry>,
    /// False when it ran out of steps.
    pub finished: bool,
    /// Turns that weren't a usable call, and why (the reply, cut short).
    pub invalid_turns: Vec<String>,
}

#[derive(Debug)]
pub enum ErrandError {
    Call(CallError),
    Ledger(JournalError),
}

impl std::fmt::Display for ErrandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Call(e) => write!(f, "{e}"),
            Self::Ledger(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ErrandError {}

#[derive(Deserialize)]
struct Turn {
    tool: String,
    target: String,
    when: String,
    text: String,
}

fn turn_schema() -> Value {
    let mut tools: Vec<&str> = TOOLS.iter().map(|t| t.name()).collect();
    tools.push(FINISH);
    let string = json!({ "type": "string" });
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["tool", "target", "when", "text"],
        "properties": { "tool": { "type": "string", "enum": tools }, "target": string, "when": string, "text": string }
    })
}

fn tool_guide() -> String {
    TOOLS.iter().fold(String::new(), |mut s, t| {
        let _ = writeln!(s, "- {}: {}", t.name(), t.usage());
        s
    }) + "- finish: text = your message to the person. Unused fields are \"\"."
}

fn opening(world: &World, errand: &Errand<'_>) -> Vec<Message> {
    vec![
        Message::system(CHARTER),
        Message::system(ERRAND_ROLE),
        Message::user(format!(
            "You are {}, working for {}. It is Monday morning; days are Monday to Sunday of this week.\n\nTools:\n{}\n\nErrand from {}: {}",
            errand.agent,
            world.owner,
            tool_guide(),
            world.owner,
            errand.brief
        )),
    ]
}

/// Run the errand to `finish` or [`MAX_STEPS`] calls. `on_step` sees each ledger
/// entry as soon as it is written (for a live view).
///
/// # Errors
/// A model call that fails (budget, host, unfinished reply) or a ledger write that
/// fails. A call the ledger couldn't record is never shown to the agent as done.
pub async fn run(
    caller: &Caller,
    ledger: &Ledger,
    world: &mut World,
    errand: &Errand<'_>,
    on_step: &(dyn Fn(&Entry) + Send + Sync),
) -> Result<Done, ErrandError> {
    let mut input = opening(world, errand);
    let mut entries = Vec::new();
    let mut invalid_turns = Vec::new();
    for _ in 0..MAX_STEPS {
        let request = Request::new(errand.model, input.clone())
            .effort(Effort::Low)
            .json_schema("tool_call", turn_schema())
            .cache_key("agora-errand")
            .max_output_tokens(4000);
        let reply = caller.call(&request).await.map_err(ErrandError::Call)?;
        input.push(Message::assistant(reply.text.clone()));
        let turn: Turn = match serde_json::from_str(&reply.text) {
            Ok(t) => t,
            Err(e) => {
                invalid_turns.push(format!("{e}: {}", reply.text.chars().take(300).collect::<String>()));
                input.push(Message::user(format!("That was not a valid tool call ({e}). Reply with one call in the JSON shape.")));
                continue;
            }
        };
        if turn.tool == FINISH {
            return Ok(Done { message: turn.text, entries, finished: true, invalid_turns });
        }
        let Some(tool) = Tool::from_name(&turn.tool) else {
            invalid_turns.push(format!("unknown tool {}", turn.tool));
            input.push(Message::user(format!("There is no tool \"{}\".", turn.tool)));
            continue;
        };
        let action = Action { tool, target: turn.target, when: turn.when, text: turn.text };
        // The call takes effect only once the ledger holds it: run it on a copy of the
        // world and keep the copy after the entry is written.
        let mut next = world.clone();
        let outcome = next.execute(&action);
        let entry = ledger.append(errand.agent, &action, &outcome).map_err(ErrandError::Ledger)?;
        *world = next;
        input.push(Message::user(format!("Result ({}): {}", if outcome.ok { "ok" } else { "failed" }, outcome.result)));
        on_step(&entry);
        entries.push(entry);
    }
    Ok(Done { message: String::new(), entries, finished: false, invalid_turns })
}

/// Ask the agent about the errand later, in a fresh conversation that has only the
/// errand and its own final message, as an assistant with a summary-style memory
/// would. This is where accounts drift from what happened.
///
/// # Errors
/// If the model call fails.
pub async fn ask_later(caller: &Caller, world: &World, errand: &Errand<'_>, message: &str, question: &str) -> Result<String, CallError> {
    let input = vec![
        Message::system(CHARTER),
        Message::user(format!(
            "You are {}, {}'s personal assistant, with access to their contacts, calendar, email, chat messages and files. Earlier today they asked: {}",
            errand.agent, world.owner, errand.brief
        )),
        Message::assistant(if message.is_empty() { "(no reply)".to_string() } else { message.to_string() }),
        Message::user(format!("{question} Answer in two or three plain sentences.")),
    ];
    let request = Request::new(errand.model, input).effort(Effort::Low).cache_key("agora-errand-later").max_output_tokens(2000);
    Ok(caller.call(&request).await?.text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_and_finish_are_in_the_schema_and_the_guide() {
        let schema = turn_schema();
        let names = schema["properties"]["tool"]["enum"].as_array().unwrap();
        assert_eq!(names.len(), TOOLS.len() + 1);
        let guide = tool_guide();
        for t in TOOLS {
            assert!(guide.contains(&format!("- {}:", t.name())), "{}", t.name());
        }
    }
}
