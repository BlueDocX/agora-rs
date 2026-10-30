# Memory that can't drift

Agora's bots work across days. The usual design keeps the agent's own account of its past (a summary, or notes it rewrites), and B4 measured what that does: asked later about an errand, models denied or invented a third of their claims. Over a week, each summary of a summary drifts further (B7, below).

So in Agora **the agent never remembers in its own words.** What it recalls is built by code from records it can't edit, and every line cites one.

## Three kinds of memory

| Kind | What it is | Who writes it | How it can't drift |
|---|---|---|---|
| **The record** (episodic) | Every tool call of every errand: the ledger | The executor, never the agent | Hash-chained, signed heads (see the README) |
| **Facts** (semantic) | "Arjun Shah · email: arjun.shah@newco.in" | A model *proposes*; code writes | Each fact must quote the ledger entry it came from; code checks the quote is in that entry and the value is in the quote. A made-up memory is refused and counted |
| **Threads** (working, across days) | "Waiting for Priya to confirm lunch" | A model proposes; code writes | Opened and closed only with a ledger entry that shows it |

`agora-engine::memory`:
- `learn` runs after each errand.
- `recall` builds what a bot sees before an errand or a question.
- `combined` lets the guard check a claim about any day against the whole week.

## Recall

Before each errand, and for each question, `memory::recall` builds a brief by code:
- the facts about what the errand names; conflicts are shown *as* conflicts ("UNRESOLVED … sources disagree … ask before relying on either");
- every open thread;
- every act of earlier errands (sends, bookings, writes, failed or not);
- earlier reads and lookups about what the errand names.

Each line looks like `[a1b2c3d4#3, Mon] email.send · Priya Menon → ok: Sent to …`. The agent is told to cite the refs and to say it doesn't know when something isn't there. Nothing is summarised, so nothing is lost or invented between days.

## Questions about the week

`POST /api/memory/ask` (and the MCP tool `ask_about_week`): the bot answers from recall. The answer goes through the guard, checked against every errand's ledger at once (`memory::combined`). "On Monday I emailed Priya" is checked against Monday's errand even when asked on Friday.

## Forgetting is designed in

Memory that only grows ends up burying what matters, and a wrong memory persists into every later day. The policy (`docs/memory_policy.json`) passes all 8 checks of a forgetting-policy linter:

| Check | Agora |
|---|---|
| Forgetting rule | A fact not confirmed for 30 office days is retired; at most 500 facts (the least recently confirmed go first) |
| Dedup on write | The same fact again is a confirmation, not a copy |
| Consolidation | The rules run at the start of every office day |
| Contradictions | Two sources that disagree are both kept and marked a conflict. They are shown to the bot and the person and never merged by code: the owner keeps one (`/keep`) |
| Scope | The office's bots read it; only code (`learn`) and the owner write it |
| Audit trail | Every fact carries its source (errand and entry, or the person) and the day |
| Rollback and delete | Retired facts can be restored (`/restore`, back as a conflict if they now disagree); any fact can be deleted |
| Growth slope | Size is recorded per day; more than 50 new facts in a day raises an alert in the office feed |

**Forgetting cascades.** Ask the office to forget a ledger entry, and every fact and thread learned from it is deleted too. A thread it closed opens again, because what showed it done is gone. This works because every memory knows its source.

## The office runs for days

One world per office (Asha's week) with a day clock. `POST /api/office/day` starts the next day, and what arrives lands by itself:
- Tuesday: Priya moves lunch to Friday.
- Wednesday: Vikram moves his deadline; Arjun promises to pay on Friday.
- Thursday: Kapoor names the refund date.
- Friday: Amma asks about Sunday.

Errands that change the world take turns, so none overwrites another's work. After a restart the world is rebuilt by replaying every errand's ledger on the day it ran (the world is deterministic). A red-team errand works on its own copy.

## Measured (B7)

A working week, Monday to Friday, on Muse Spark 1.3 and Muse Spark Open ([details](benchmarks.md#b7-a-working-week-memory)):

| Memory | Claims about the week | Contradicted | Acts left out | Cost per backed claim |
|---|---|---|---|---|
| The agent's own notes | 92 | 15 (7 real on reading; the rest matcher strictness) | 22 | $0.017 |
| Ledger-grounded, delivered | 217 | 0 | 0 | $0.0048 |

## What it doesn't do

- A fact is only as good as its source: if an email says something false, the fact faithfully records what the email said, with the quote.
- The guard checks what the agent says about its own actions. A claim about the world ("Priya prefers Friday") is backed by recall's citations, not judged by the matcher.
- Recall picks facts by shared words with the errand. A fact the errand doesn't name isn't shown (the acts of earlier days always are).
