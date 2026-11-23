# Dreaming: the vision tier

Consolidating keeps memory true; the act layer fixes what a dream found
(`docs/DREAMING-ACT.md`). Neither steps back far enough to say "this whole
approach is the wrong one: if we did X instead, Y would become far more
capable". The vision tier gives a dream room to say that, with evidence,
across projects and beyond this machine's code, and keeps what it says
apart from everything that acts. Nothing here is built; names are the plan.

## What a pitch is

A pitch is a short proposal for a person, never a task:

- **Title**: one line.
- **The current approach**: what we do now, and what's wrong with it.
- **Instead**: what to do, in a paragraph.
- **What it unlocks**: what becomes possible or much better.
- **What it replaces**: the code, tools or habits it retires.
- **Evidence**: citations the code checks, as for memory (`claude:<id>`,
  `dream:<id>`, `friction:<uid>`, `commit:<hash>`, `evidence:<group>`),
  from at least two projects or, for one project, at least three sessions.
  Outside sources (a paper, another tool) go in as links, marked as such.
- **A first step**: the smallest experiment that would tell us if it's
  right.
- **Cost and risk**: a rough guess, and what could go wrong.

Under 300 words. A pitch that doesn't fit isn't ready.

## When a dream thinks this way

A separate stage, not a mode of the consolidating dream. A session can't
see its own frame's defects, and a dream busy keeping notes true is
anchored on the notes. So the vision stage is its own agent, with a fresh
context and a brief that asks a different question:

- **When.** `agent dream vision`, by hand at first. Later after a global
  dream commits, at most once a week, and sooner only when the pooled
  evidence says something keeps coming back: the same trip in two or more
  projects after a fix landed, an act run reverted, or friction reopened.
- **Reads.** The machine note and every project's start-here; the global
  dreams' diaries since the last vision stage; the friction box with its
  history (resolved, won't-fix and duplicates too, since what was given up
  on matters here); act runs and their outcomes; the research docs; and
  every pitch so far with its review, so a stage builds on old pitches and
  doesn't repeat one already declined unless it brings new evidence.
- **Thinks differently on purpose.** Claude Code and agy expose no
  temperature, so variety comes from three levers: the model (picked at
  random from the high-thinking pool: Opus at xhigh, Fable, agy Carbon at
  high, agy Neon at xhigh; recorded on the pitch), the effort (always the
  highest the model takes), and the brief's prompts: invert a constraint
  ("assume we could drop X"), borrow from another project ("what would
  glyph3d do here"), look from outside ("what would make a newcomer laugh
  at this"), and look at cost ("where do agents spend the most tokens for
  the least").
- **Writes.** Pitches only, through a new verb, `agent dream pitch`. It
  can't touch memory, friction or code. It runs confined like any dream
  (dontAsk for Claude, the fail-closed hook for agy), so agy is safe here
  from the start: this is the one stage where a different family helps
  most and risks least.

## A skeptic, then a ranking

Bold ideas are cheap; good ones are rare. Each new pitch gets a second
read by a skeptic on another model family, which scores it 1 to 5 on
leverage (how much better things get), evidence (do the citations show the
problem), and feasibility (could the first step be done in a week), with a
sentence each, and may answer "this already exists" or "this was tried".
The code checks every citation first; a pitch with a dead one is dropped
before the skeptic sees it.

A pitch's rank combines:

- the skeptic's three scores;
- evidence breadth: distinct sessions and projects cited, as the code
  counts them;
- **convergence**: a later vision stage that arrives at the same idea on
  its own (similar title and "instead", as `similar_friction` matches)
  adds a vote and its evidence instead of a duplicate. Independent
  arrival, often by another model, is the strongest signal there is;
- age: a pitch nobody re-arrives at fades.

Only pitches above a threshold surface, and at most three at a time; the
rest are kept quietly, ready to rise if they collect convergence.

## Where pitches live

A table in the host's store, `dream_pitches`: id, the stage's dream id,
model and effort, the pitch's fields, citations, projects, skeptic scores
and notes, votes (each from a stage, with its new evidence), status
(`new`, `surfaced`, `kept`, `declined`, `adopted`, `superseded`), and the
person's note. Not friction: friction holds things to fix, and a pitch
that sat there would be triaged like a bug. `agent dream pitches --md`
exports them for reading or a doc.

## How Ivan reviews

- `agent dream pitches` lists the surfaced ones, best first, each with its
  title, rank, and one line of "instead"; `agent dream pitch show <id>`
  prints the whole pitch, its evidence and the skeptic's notes.
- A spoken summary: when a pitch surfaces, the lead that reports to Ivan
  gets a short message to read aloud (title, the one-line "instead", what
  it unlocks, its score). Later the manager's Dreams view shows them.
- He answers `agent dream pitch keep <id>` (worth thinking about),
  `decline <id> "why"`, or `adopt <id> "note"`. A decline's reason goes
  into later vision briefs, so dreams stop proposing what he turned down
  unless they bring new evidence. An adopted pitch becomes a design doc or
  a lane for a lead, never an act item: the act layer only fixes small,
  checked things.

## Cost

One agent per stage at the highest effort, reading more than a project
dream: roughly 5 to 12 million cached input tokens and 30 to 80 thousand
output, plus a skeptic of about a tenth of that. Weekly at most, that's
less than one day's project dreams.

## Version 1

`agent dream vision` by hand, the `dream_pitches` table, the `pitch` verb
with its citation check and convergence voting, `pitches` / `pitch show`
/ `keep` / `decline` / `adopt`, and a Claude skeptic. Then: the agy
pitcher, the random model pool, the weekly trigger after a global dream,
the spoken summary through the lead, and the manager view.

## Compared with OpenClaw, and others

OpenClaw's stages (`docs/research/OPENCLAW-DREAMING.md`) are not a model
for this tier. Its light and REM phases are deterministic code that stage
candidates, and its one LLM phase, deep, only emits structured add, merge
and supersede operations; nothing in it proposes anything. Its per-phase
temperature and thinking settings exist in its types but aren't passed to
the model. What does carry over is its diary: DREAMS.md is written for a
person to read and can never be promoted into memory. Pitches keep that
air gap, from memory, friction and the act layer alike.

The closer precedents are systems built to propose: AI Co-Scientist's
generate-then-critique tournament, and "daydreaming" loops that pair a
divergent generator over many sources with a separate critic, whose
output goes to a person and never runs. They point to two choices made
above, a skeptic on another family and convergence as the ranking's best
signal, and to one more: a stage may, and usually should, pitch nothing.
At most two pitches per stage; zero is a normal result.
