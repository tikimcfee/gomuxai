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

## The idea engine: a generator and a critic

The source of ideas is a component of its own, not part of the vision
stage: `agent-dream`'s idea engine, a **generator** paired with a separate
**critic**, in the style of AI Co-Scientist and daydreaming loops. Three
callers use it, each with its own question and evidence:

- the vision stage (this doc): "what approach should change?";
- the act layer's improvements: "what would you change here, and why?",
  whose winners are filed as cited `improvement` items;
- the step-3 global dream's synthesizer: "what should memory say now?",
  whose winners become the change set the skeptic checks and code applies.

Building it once means the diversity, the critic and the convergence
record are shared, and a caller only supplies a question, an evidence
bundle and what to do with the winners.

**The shape.** A run takes a *question*, an *evidence bundle* (cited
items, as a digest), the *lenses* to apply, and a sample count. It
returns ranked *ideas*, each a structured record: a title, a claim, an
"instead", what it unlocks and replaces, and its citations. Every stage is
an agent spawned confined like a dream, writing only through verbs
(`agent dream idea` for the generator, `agent dream judge` for the
critic), so the engine never needs more trust than a dream has.

**Diversity**, since one sample from one model finds one idea:

- *Several samples.* The generator is run N times (3 to 5 for vision, 1 to
  3 for the others), each a fresh agent.
- *A random model per sample* from the high-thinking pool (Opus at xhigh,
  Fable, agy Carbon at high, agy Neon at xhigh), at its highest effort,
  recorded on each idea. Neither CLI takes a temperature, so models and
  lenses are the levers.
- *A lens per sample*, drawn from a fixed list: invert a constraint
  ("assume we could drop X"), borrow from another project ("how does
  glyph3d solve this"), the outsider ("what would a newcomer find absurd"),
  the cost lens ("where do agents spend the most for the least"), and the
  history lens ("what did we give up on, and is that still true").
- *Zero is allowed.* A sample may return no idea, and that's a normal
  result.

**The critic**, always a different family from the idea's generator:

1. The code drops any idea with a citation that doesn't resolve, before a
   model sees it.
2. Absolute scores, 1 to 5 with a sentence each: leverage, evidence,
   feasibility, and novelty against what's already in the repos, the docs
   and earlier ideas (it may answer "this exists" or "this was tried").
3. A pairwise tournament among the run's survivors: the critic compares
   two at a time and says which is stronger and why; the code keeps an Elo
   score per idea over the matches. With few ideas (under five) every pair
   is compared; with more, a Swiss-style round of about N·log N matches.
4. The rank is the tournament score, gated by the absolute scores (an
   idea scoring 1 on evidence never surfaces, however it fares in pairs).

**Convergence across runs.** Ideas live in one table, `dream_ideas`, with
the caller (vision, act, synthesis) on each. A new idea is matched against
earlier ones of the same caller: first by the code (token similarity on
title and "instead", as `similar_friction` does), then confirmed by the
critic ("same idea as #N, or different?"). A confirmed match adds a vote
to the old idea (the run, the model, the new evidence) instead of a
duplicate. Votes from different models and different lenses count most;
votes from the same model with the same lens count least, since that's the
same sample drawn twice. A pitch's rank (below) adds its votes to its
tournament score.

**What steps 1 and 2 already give it, and what step 3 must keep open.**
Confinement (the read-only command list, dontAsk, the fail-closed agy
hook, snapshots outside the dream folder) already applies to any agent
spawned confined with an open dream, so generator and critic agents need
no new trust. Step 3 builds the first caller, the synthesizer and a
skeptic, so it should take the engine's shape from the start: the
synthesizer is a generator run with a "what should memory say" question,
the skeptic is the critic, and their outputs go through `idea` and
`judge` verbs into `dream_ideas`, not through a format private to the
global dream. A dream run records each worker (its role, model and lens)
in a workers table, so sessions of generators and critics are never
re-read as new evidence. (**built** for synthesis, as
`docs/DREAMING.md` describes: workers, `dream_ideas` and its votes, the
`idea`, `judge` and `done` verbs, and code applying what the critic
approves. The critic's verdict is approve or reject with optional
scores; the pairwise tournament and Elo are later, `rank` waiting for
them, and synthesis ideas never converge.)

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

Bold ideas are cheap; good ones are rare. The vision stage runs the idea
engine above with its question, three to five samples and every lens; its
critic is the skeptic here. Each new pitch gets a second read by that
skeptic on another model family, which scores it 1 to 5 on
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
