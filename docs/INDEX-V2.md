# The index, v2: the standard pipeline on Postgres

v1 (`docs/INDEX.md`) tied `rg` on a pre-registered blind test and ran
about ten times slower. Two lessons came out of it. We gated a
known-good architecture on an eval before building its known-good
parts: v1 had no summaries, no query expansion, no reranker, and none
of the corpora beyond code. And we hand-rolled what databases already
do: a store, full-text ranking, fuzzy matching, vector search and rank
fusion, written and tuned by hand in Rust over SQLite.

v2 builds the standard retrieval pipeline from real tools, and uses the
eval differently: as a tuning and regression loop at full scale, run on
every change, not as a gate in front of the work.

`docs/INDEX.md` stays the reference for the entity model, freshness
keys, edges, labels, the two layers (index is the what, memory the
how), dreams and confinement; this document says what v2 changes. Each
piece is marked **built** when it is. Iteration 1's store, sync, search
(full text, trigram, vectors, RRF in SQL), show, tree, related and embed
are built: "Iteration 1, as built" says how, and where it differs from
this plan.

## The pipeline

```
query ─► expand ─► retrieve (per corpus, several lists) ─► fuse (RRF, in SQL)
      ─► rerank (cross-encoder, GPU) ─► group by corpus ─► budget ─► answer
```

1. **Expand**: label match and the learned vocabulary (`INDEX.md`, "Query
   expansion"); the model rewrite only when the first pass is weak.
2. **Retrieve**, from each corpus, several candidate lists: full text
   (weighted), trigram similarity on names and paths, dense vectors on
   cards and chunks, and the asks (doc2query) as their own text.
3. **Fuse** the lists by reciprocal rank fusion, in one SQL statement.
4. **Rerank** the top 50 to 100 per corpus with a cross-encoder on the
   GPU: the step v1 lacked entirely, and the one that most often
   separates a good pipeline from a fair one.
5. **Group** by corpus and **budget** the answer: a few lines of code, a
   few of research, the session where it broke, the memory note about
   it.

## Store: Postgres

One Postgres database, local to the host, holds every corpus. Rust
keeps the parts only it can do (crawl, parse, scrub, chunk) and calls
the database for everything else. The extensions and versions are
chosen from what they do today (see "Feature check"):

- **full text** (**built**, code corpus): a `tsvector` per row with
  weighted parts (A: title, name and labels; B: summary and asks; C:
  seed and why; D: body), ranked by `ts_rank_cd`, queried with
  `websearch_to_tsquery`. The default parser doesn't split identifiers,
  so Rust adds the split forms (`gather_global` and `gatherGlobal` both
  give `gather global`) to a `terms` column that feeds the vector beside
  the whole identifier. As built, the title went to B (stemmed, with the
  summary), A holds the terms and labels, D the children's names, and
  the query is built per term (`plainto_tsquery` in `simple` and in
  `english`, OR'd for "any word" and AND'd for "every word") rather than
  by `websearch_to_tsquery`, to keep v1's query semantics.
- **fuzzy** (**built**): `pg_trgm` on names, paths and labels
  (`word_similarity` for a short query inside a longer name), with a
  trigram GIN index.
- **vectors** (**built** for cards; chunk vectors not yet): `pgvector`
  `halfvec` columns with HNSW indexes, and iterative index scans, so a
  filtered query (one project, one corpus) still returns enough rows.
- **graph walks** (**built**): recursive CTEs over `edges` (`related`,
  graph expansion, "what depends on this"); `show`'s breadcrumb and
  `tree` walk `parent` the same way.
- **fusion** (**built**): RRF over the lists as CTEs in one statement.
  As built a search is four round trips: its context (the caller's
  dependencies, whether vectors exist), the label n-grams, the fused
  statement, and the hits' details.

ParadeDB's `pg_search` (BM25) and VectorChord (vectors, and its own
BM25) are the alternatives to weigh against native full text and
pgvector; the decision and its reasons go here after the feature check.

### Schema sketch

```
projects (name, root, identity, remote, depth, crawled_commit, crawled_at)
entities (id text pk, corpus, kind, project, parent, path, content_key,
          title, seed, summary, why, asks text[], summary_key, meta jsonb,
          terms text, tsv tsvector, card_vec halfvec(N), updated_at)
chunks   (id, entity_id, ord, text, terms, tsv tsvector, vec halfvec(N),
          meta jsonb)              -- long docs, research notes, session turns
edges    (src, dst, kind, weight, origin, evidence, keys)
labels   (entity_id, label, kind, origin, weight, evidence)
runs     (id, kind, config jsonb, fingerprint, started, finished, coverage jsonb)
queries  (id, at, caller, text, results jsonb, ms jsonb)
```

`corpus` is one of `code`, `prose`, `session`, `memory`, `friction`. Ids
are shared across corpora (`gomuxai:rs/crates/...`, `session:claude:<id>`,
`friction:<uid>`, `memory:<project>/<file>`), so an edge can join any
two: a session `touched` a file, a memory note `noted-in` a file, a
research note `documents` a concept.

## Corpora, composed

One query fans out over every corpus and returns grouped, multi-shape
results, so an agent can triangulate in one lookup: the code, the
research behind it, the refutation, the session where it broke.

- **code**: every project under the dev roots, as v1 crawls them
  (entities, symbols, seeds, edges).
- **prose**: docs, research notes, design docs and experiment write-ups,
  including the research and review repos in the dev tree; long files
  are chunked by section.
- **sessions**: every Claude and agy session, as full scrubbed text,
  chunked (below).
- **memory and friction**: memory directories' notes and the friction
  box, each note or entry an entity.

Each corpus is retrieved and reranked on its own, then grouped; a
corpus with nothing good says so in one line rather than padding the
answer.

**Built** for code, prose, memory and friction (sessions are iteration
4); `pg/side.rs`, `sync_with` and `search_grouped`.

- **Where prose ends**: by entity, not by project. A document (file kind
  `doc`: Markdown, text, reStructuredText) and its sections are prose
  wherever they live, a README in a repo as much as a note in a research
  tree; everything else the crawl finds (source, tests, config, dirs,
  projects) is code. One entity, one corpus (`entities.corpus`), no
  second membership. The evidence: Markdown is 5,800 of the tree's 6,060
  document files; no project here is a notes tree (the most doc-heavy
  ones, 54 to 71% documents, are code repos with design notes); and the
  documents people ask about (`ARCHITECTURE.md`, design docs, research
  surveys) sit inside code repos, so a project-level rule would have put
  them in code. A Markdown file is cut into chunks by its headings, each
  chunk's `meta` holding its heading path and corpus
  (`{"corpus":"prose","headings":["Messages","Held messages"]}`), so a
  long note is found by its part.
- **memory**: the notes in `[memory].dirs` (default:
  `~/.claude/projects/*/memory` and `memory` in the agent's data
  directory; `MEMORY.md`, an index of the others, left out), each an
  entity `memory:<project>/<file>`, `<project>` the indexed project a
  Claude Code project directory stands for (else the directory's name;
  `machine` for the agent's own). Its title and seed come from the front
  matter's `name` and `description` (else its first heading and
  paragraph), its text goes into chunks, and each backticked path in it
  that resolves to a crawled file gives a `noted-in` edge (file → note:
  the file is noted in it).
- **friction**: this host's friction entries, read through
  `agent_store` (the agent's own store, or `[friction].store` opened read
  only), each `friction:<uid>`: the summary its title, the detail, the
  failed command's error and the resolution its text, its status, kind
  and filer its signature, the project of the directory it was filed
  from; backticked paths give `reported-in` edges.
- Both are written by the sync from what the CLI read (`crawl` and
  `sync` read them each time): a note no longer there is removed, and a
  sync from code that doesn't read them leaves them alone. Their edges
  carry their corpus as origin, apart from the ledger's. Both are
  personal: in the local database only, never in fixtures, docs or
  commits.
- **Grouped answers**: `search` with no `--kind` answers by corpus
  (code, prose, memory, friction), each group with its own limit and
  share of `--budget` (`[corpora.NAME]` `limit` and `budget`: code 8 and
  700 tokens, prose 5 and 450, memory 3 and 200, friction 3 and 150),
  and a corpus with no match says `nothing good here` in one line.
  `--corpus a,b` restricts it. Two fused statements: code and prose as
  one flat search (its order serves both groups, and its first `--limit`
  are `hits`, as before groups existed), memory and friction as another
  (each reranked on its own). `--json` keeps `hits` and adds `groups`
  (`corpus`, `hits`, `candidates`, `budget`, `note`) and a `corpus` on
  each hit.
- **The flat mode is unchanged**: `--kind file` (and any `--kind`) is one
  list over code and prose; memory and friction never enter it (every
  list filters by corpus: `entities.corpus`, and a chunk's `meta`, so no
  join). Its reranker reranks the flat list's top `[reranker].flat_depth`
  (50) as one pool. The term frequencies count crawled chunks only. On
  the same data, with the same lexical list (fts), the new code gave 256
  of 256 searches identical to 02df4d8's (64 dev questions, two
  directories, both modes; `groups` and `corpus` aside); with the
  default now bm25 (below), a search without `--lexical` differs from
  02df4d8's as bm25 differs from fts. No migration: the corpus and chunk meta use existing
  columns, and `entities_corpus`, the one new index, is created when
  missing without raising the schema version, so older binaries still
  read the schema.

**Measured** (2026-11-23, a copy schema `idx3`, cold-synced from the same
ledger with this host's memory and friction; the shared `idx` waits for
the evals running on it):

| corpus | entities | entity rows | chunks | chunk rows |
|---|---|---|---|---|
| code | 431,816 | 377 MB | 42,616 | 428 MB |
| prose | 29,093 | 59 MB | 23,907 | 74 MB |
| memory | 323 | 0.6 MB | 351 | 2.7 MB |
| friction | 64 | 0.1 MB | 64 | 0.1 MB |

The schema is 1,746 MB with indexes. Cold sync 91 s (every row
rewritten for its corpus, every file cut again for its chunk meta),
no-op sync 3 s; 387 side notes, 132 edges from them to files; their
cards embedded in 3 s. Latency over the 64 dev questions from the dev
root, CLI median and p95, with each lexical list:

| search | fts | bm25 (the default) |
|---|---|---|
| cold, flat `--kind file` | 251 / 567 ms | 302 / 673 ms |
| cold, grouped | 303 / 654 ms | 405 / 775 ms |
| warm, flat `--kind file` | 197 / 589 ms | 206 / 553 ms |
| warm, grouped | 266 / 557 ms | 323 / 676 ms |
| warm, `--corpus memory,friction` | 61 / 82 ms | 130 / 194 ms |

Grouping costs 60 to 120 ms over a flat search: the memory and friction
statement, which took 200 ms until `entities_corpus` let it start from
their 387 rows; with bm25 its card list still scans pg_search's whole
index and filters after (the shape that made `--under` fast), the next
thing to make cheaper for the small corpora. The friction group was
empty for 11 (fts) and 13 (bm25) of 64 questions, the others never. Against `idx` (same ledger, older HNSW graph and
chunk counts) `idx3`'s flat answers overlap 0.964 (top-10 Jaccard) with
the same top hit for 63 of 64.

## Sessions as full text

A summary throws information away before anyone has looked at it, so
sessions go in whole, and filtering does the work:

- read through `agent transcript` (both harnesses, one event shape);
- **scrubbed at the point of reading**, under the machine's rule:
  sign-in and token URLs, keys and credentials, and pasted content are
  dropped before anything reaches the database (`agent_dream::scrub`);
- chunked by turn: a person's prompt, the agent's replies, and one line
  per tool call (its action, target and outcome), in windows of about
  500 tokens with a small overlap; a chunk keeps its session, harness,
  project, time, turn and the files the turn touched;
- `touched` edges from a session to every file it read or edited, so a
  file's `show` lists the sessions that worked on it;
- summaries of sessions may be added as an extra layer, never instead
  of the text.

This corpus is personal. It lives only in the local database, is never
exported or committed, and a query from another host (later) sees it
only if the host says so.

**Built** (`pg/sessions.rs`; corpus `session`, a group in grouped
answers; `[sessions]` in the configuration):

- **Which**: the sessions `agent sessions` lists (its discovery over the
  Claude Code and agy directories, `agent_discovery::scan`), less those
  run from a scripted batch's directory (`skip_cwd`, default
  `style-distill`), subagents' own sessions (`include_subagents`, off:
  their work is the parent's, mostly tool calls) and transcripts over
  `max_mb` (200). `only_cwd` limits a trial to some directories.
- **Read** through the transcript event model (`agent_transcript::
  read_file`, folded to each event's latest state, so a tool call shows
  once with its result); branches the person left (edited or rewound
  prompts) and request records are skipped.
- **Scrubbed at the point of reading**: each prompt and reply goes
  through `agent_dream::scrub::excerpt` (pasted and harness-inserted
  blocks, code blocks, quotes, private key blocks, and lines with a
  credential marker, an email address or a sign-in URL dropped); a tool
  line (action, target or clipped detail, a failed status) with a
  credential is dropped whole. The scrubber is broad by design (any line
  naming a token goes), and the sync reports what it dropped.
- **Chunked by turn**: a turn is `Person:`, `Agent:` and `- action
  target` lines under `## Turn n time`; whole turns are gathered up to
  `chunk_chars` (2,000, about 500 tokens) and a longer turn is cut in
  windows, each starting with the last `overlap_chars` (200) of the one
  before. A chunk's meta: session, harness, project, time, its turns and
  the files they touched.
- **The entity** `session:<harness>:<id>`: its project (the indexed
  project holding its directory), title (the harness's, else the first
  prompt's line), seed (its first prompt, scrubbed), its card embedded
  like any other; `touched` edges session → file for each file it read,
  edited or wrote (an absolute path resolved to the crawled file), the
  call count as weight and the actions as evidence, so a file's
  `related` (and `show`) lists the sessions that worked on it ("touched
  by sessions").
- **Incremental**: a session's key is its transcript's path, size and
  last write and the reader's version; one unchanged since the last sync
  isn't read again, its row, chunks and edges kept.

**Measured** (2026-11-23, every session on this host, in a trial schema
with one project crawled; the full index's grouped latency waits for the
evals reading it):

| | |
|---|---|
| sessions listed | 2,363 |
| left out: scripted batches, subagents | 1,433, 492 |
| read | 438 (cold sync, reading included, 18 s; a no-op sync reads none) |
| turns, chunks | 4,414 turns in 14,626 chunks (84 MB of rows), average 1,805 characters |
| scrubbed | 322 pasted blocks, 5 inserted blocks, 8,361 credential-shaped lines; 74% of prompt and reply lines kept |
| left in the text (checked by pattern) | no keys or tokens, sign-in URLs, emails or inserted tags; two mentions of a key format's name, no key |
| session cards embedded | 438 (part of 1,352 in 22 s) |
| search, `--corpus session`, 64 dev questions | 79 ms warm, 155 ms cold (median); every question found sessions |

## Summaries and asks, by a local model

`INDEX.md`'s bet (a what, a why and three to five plain asks per entity)
is built in v2, over all of the dev tree, by a local model on the GPU,
with no cloud quota. Before any long batch:

1. **Preflight** on a few projects of different shapes (a Rust repo, a
   JS repo, a research repo): every step runs end to end, and the output
   is read.
2. **Coverage report**: what was indexed, what was skipped and why
   (fixture, vendored, too large, binary, scrubbed), by project and kind.
3. **Config fingerprint**: model, quantization, prompt version and hash,
   sampling parameters, input clipping, code commit; stored with every
   run and every summary, so a change in config is a change in key.
4. **A skeptic reviews the run config** (prompt, model, filters, budget)
   before the batch starts, not after.
5. **GPU manners**: the GPU is shared with a resident text-to-speech
   server. The batch checks free memory first, runs with a capped memory
   fraction, and backs off when the speech server is busy.

Thinking is off for these calls, sizes are checked against the server's
tokenizer, and JSON is validated before it lands (`INDEX.md`, "Who
writes them").

**Built** (`agent index summarize`; `agent-index` `summarize.rs`,
`pg/summ.rs`; preflight only, no batch yet):

- **Which files**: files of full-depth projects, kinds source, test,
  doc, config and build. Skipped before any read: sensitive names, no
  content, vendored paths (`vendor`, `third_party`), program output
  directories (`outputs`, `results`, `snapshots`, `runs`, `logs`),
  directories of 50+ numbered files of one extension, lock and checksum
  files, licenses and ignore files, JSON that isn't a manifest or a small
  hand-laid file. After reading: files under the floor (fewer than 200
  characters and 5 non-blank lines), a generator's mark on a comment line
  in the first 600 characters, a file changed since the crawl.
  `--coverage` reports it by reason, project and kind, with the 20
  directories most skipped, and calls no model.
- **The call**: `[llm.summaries]` in the configuration (endpoint, model,
  sampling, seed, concurrency, retries, clipping, floors, a pause file, the
  scrub's extra words). Thinking off, a JSON schema, the file's kind in
  the prompt, its text between unusual markers with a line saying it is
  data. Code keeps its head and an evenly sampled symbol list; a doc its
  heading outline, head and tail; the whole prompt is bounded by tokens.
  The input is scrubbed (keys, tokens, sign-in URLs, emails, the
  configured words) before the model reads it.
- **Checks** (`checks-3`): a "This file" or "A Python script that" lead
  cut, 3 sentences, 400/300/160 characters; every identifier-shaped token
  (snake, camel, `::`, a call, backticks) in the file, its path or its
  symbols, product names aside; asks holding the file name, a shaped stem,
  a listed mixed-case symbol or an identifier shape are dropped, and an
  entry keeps 2 to 5; the output scrub; a second attempt (seed + 1) when
  one fails. Asks found on 4+ entries are dropped after the run. Failures
  are counted by reason.
- **Rows**: `summaries` (migration 4) keeps every attempt with its
  content key, fingerprint, sha256 of the rendered input, attempt,
  clipped, tokens and status (`live`, `superseded`, `failed`). A run
  resumes by (content key, fingerprint), never by id. The fingerprint's
  key hashes what decides an answer: the model id and snapshot commit
  (read from the server's `/v1/models` root), max length, server version,
  dtype and quantization, the prompt version and the whole request
  skeleton, schema, sampling and seed, thinking, the clip and fit limits,
  the filter, scrub and check versions. The build, `[llm.summaries]`
  (endpoint, timeouts, concurrency, retries) and the transport retry
  policy are recorded with the run, not keyed. Each run is a `runs` row
  whose stats (done, remaining, skipped, errors, rate, ETA) are rewritten
  as it goes and when it ends.
- **Freshness**: the sync shows a live summary (and its asks) over the
  seed while its content key is the entity's, so a changed file falls back
  to its seed; its card is queued for embedding as any changed card is.
- **Stopping cleanly**: 8 connection errors in a row, `/health` failing 3
  times, no file finished for `--stall` (paused time aside), `--max-time`,
  or over `--max-error-rate` of the last `--error-window` files failing
  stop the run within seconds; it saves its stats, a sync shows what was
  written, and it exits 1. A file the server didn't answer in time (5xx,
  timeouts, past `--file-timeout`) gets no row, so a resume does it; a
  file the model answered badly is kept as failed. `--skip-project`,
  `--skip-prefix project:dir/` and `--own-files PROJECT=FILE` (a fork's
  own files) narrow the selection, and the coverage report counts what
  they leave out. `--corpus code|prose` picks the corpora (default code;
  prose files are kind doc and take the doc clip). Coverage also counts
  the directories and projects a bottom-up pass would summarize, and marks
  files in crates copied from elsewhere (the nearest Cargo or npm
  manifest names a repository that isn't the project's): summarized,
  but known.
- **The bottom-up pass** (`summarize --dirs`, `pg/dirsum.rs`), after the
  files: every directory holding a selected file, deepest first, then
  each project. A unit's input is composed: its README's start (4,000
  characters), its listing (every child's name, summarized or not, up to
  3,000 characters) and an entry per child, its live summary or else its
  seed (marked "its first line"). A directory whose entries don't fit the
  prompt's bound is cut into parts; each part is summarized from some of
  its entries and kept as status `part` (never shown, reused by a
  resume), and the directory from its README, listing and parts only:
  while a part is missing (failed, or not answered) the directory waits
  with no row, and a resume (`--retry-failed` for a failed part) does it.
  The README is scrubbed as it is read, like every input. A child under
  `--skip-prefix` or `[summaries].withhold` is named in the listing and
  nothing more (no entry, no README read from it), and a withheld
  directory is no unit, so no parent is built from a withheld child.
  Units go through the same run as files (one run per depth, and one for
  a depth's parts): the same request bound, checks and resume, under
  their own fingerprint (prompt `dirs-2`, checks `dir-checks-1`: the file
  checks, a "This directory" lead cut first, a child's name allowed as an
  identifier), so file summaries keep theirs. The stops span the pass:
  `--max-time` counts from its start, and the stall clock, the error
  window and the connection errors in a row carry from one run to the
  next. `--stats` is the pass's status (done of total units and parts,
  errors, rate, time left, the wave running, each finished wave). A
  unit's content key is its directory's, a hash of everything under it:
  its summary shows while nothing under it changed, so run the pass after
  the files (a child summarized again later doesn't stale its directory).
  A unit composed while one of its children in the pass wasn't settled (no
  summary and no failure, or a provisional summary of its own) is written
  provisional (`meta.incomplete`, the count): shown, but not done, so a
  resume composes it again; a preflight's sample leaves no parent built
  from seeds for good. `--coverage` reports units by depth, how many are
  cut into parts and how many hold no summarized child yet; `--limit N`
  samples N units (a preflight, best in a scratch schema: its failures
  count as done there).

## Reranker

A cross-encoder on the GPU scores (query, candidate) pairs for the top
50 to 100 of each corpus. The model is chosen from the current
state of the art that fits beside the other resident models (see
"Feature check"), served by a local endpoint, and is part of the tuning
loop: the eval measures the pipeline with and without it, and at
several candidate depths.

**Built** (pg store; `pg/rerank.rs`, `apply_rerank` in `pg/search.rs`):
off by default; `search --rerank` or `[reranker] enabled = true` turns it
on. After fusion and the priors, each corpus's top `[reranker.depth]`
candidates (code: 50) go to the endpoint's `/score` (vLLM's pooling
runner, one score per candidate in their order) with the configured
instruction, and are put in its order in the places they held, so the
candidates past the depth, and another corpus's, don't move. A
candidate's text is its card (path, title, summary or seed, the names it
defines; a symbol's own name and signature) and then the chunk of its
file the query's terms best match (a symbol's or section's own chunk),
clipped to `doc_chars` (1,500) and cut by the server at
`max_tokens_per_doc` (512). With `keep_pins` (on), an identifier's
definitions, a whole-query label's entities and a rare identifier's files
keep their places above it. `--explain` shows `rerank #n (score)` first
on each reranked hit; `--json` has a `rerank` object (model, depth,
scored, ok, error, ms) and `ms.rerank`. On a timeout (`timeout_ms`, 5 s)
or any error nothing moves: the hits say `rerank failed: fused order
kept`, `rerank.error` says why, and the text output says so in a line.
Tests run it against a stand-in server (it reorders; an endpoint that
doesn't answer leaves the fused order).

## A code-trained embedder

The dense lists moved from bge-small over cards alone to
Qwen3-Embedding-0.6B at 512 dimensions over cards and chunk windows,
with queries embedded on the GPU and the CPU behind it.

**Built** (`pg/vecs.rs`, `pg/embed_pg.rs`, `embed_models.rs`):
- **Models by name.** `[embedder].model` takes `bge-small-en-v1.5`,
  `qwen3-embedding-0.6b` (1,024 dimensions, Matryoshka: `dims` cuts and
  renormalizes) or `coderankembed` (768), run by candle on the CPU or
  by an OpenAI-compatible `/v1/embeddings` server (vLLM's pooling
  runner). `[embedder.corpus]` can give a corpus its own model.
- **Tables by model.** bge's card vectors stay in `entities.card_vec`.
  Every other model has `vec_cards_<key>` and `vec_chunks_<key>`
  (key: the model's slug, and its width when cut), with each entity's
  filter columns copied in, so the search's filters and HNSW's iterative
  scan work on one table. The sync drops the rows of entities that go.
- **Chunk windows.** `[embedder.chunks]` cuts each chunk into windows at
  line ends, `window_chars` (3,500) at most, capped at about 3
  characters a token of what the model reads. Each window's text starts
  with a header (`header`: none, path, or full: the project and path,
  the file's summary in a sentence, then what the window defines or its
  heading path; a session's title). A window's key hashes the model's
  slug, the header's version and mode, and its text, so a Matryoshka cut
  keeps the full vectors' keys and only changed windows are embedded
  again.
- **Search.** One dense list per model: `semantic <key>` over the cards
  and `semantic chunks <key>`, each entity ranked by its best window,
  both fused by RRF with the word lists.
- **The live path.** `[embedder].endpoint` names the query server. A
  query asks it with `timeout_ms` (1,500), a batch with
  `batch_timeout_s` (60). On an error or a timeout, the CPU embeds instead
  and the server is left alone for `recheck_s` (30), counted from the end
  of the CPU's work, so a dead server costs one timeout, not one per
  query. Both sides cut an over-long text the same way (`fit`: at a
  token boundary, before either sees it; vLLM's own cut keeps the first
  tokens too). The endpoint, served name, timeouts and `keep` are left
  out of the fingerprint: they change where a vector is made, not what
  it is.
- **What it says.** `search --json` has `"embedder": {model, on: gpu|cpu,
  note}`; the text output says in a line when the CPU did it; the query
  log has `embed_on`; `agent index serve --status` shows each embedding
  server's health (up, down since, served and fallen-back counts).
- **The watcher.** Each flush embeds, with every live model, the cards
  the sync queued and the chunks it cut again (`embed_pg::keep_up`).
  With the server down it embeds at most `[watch].cpu_texts` (2) texts,
  cards then windows, on the CPU, and the rest wait, counted by `agent
  index watch --status`. The tidy is capped the same way (a summary
  batch stops the server for half an hour or more). The next flush takes
  them up, or a catch-up every 30 s once the server answers its
  `/health` again; while it doesn't, the catch-up does nothing. With a model larger than bge
  the watcher embeds a text at a time on every core (about 3.5 s a
  window, against 28 s on one thread); bge alone keeps
  `embed_workers` single-threaded workers.
- **Kept models.** `[embedder].keep` lists models kept current but not
  searched: bge's cards, so going back is a configuration change.
- **`agent index embed`** takes `--model`, `--endpoint` (a bulk pass on
  the GPU), `--bulk` (HNSW built after the rows), `--copy-from SCHEMA`
  (a model's vectors from another schema of the same database, matched by
  entity key and text key), `--dry-run` (counts only, against the other
  schema's tables with `--copy-from`) and `--refit-long` (windows past the
  token cut embedded again with the live cut).

**Dev comparison** (2026-11-24, a scratch schema copied from the live
one; dev sets only: blind 2, blind 3, cross dev and the episodes, all
spent; B2 is blind 2 by sentence at the project, B3 blind 3 at its scope,
X cross dev at its scope; cells are hit@5 / MRR@10; dense R@10 is cards
and chunks over the 140 questions of B2, B3 and X, bge's cards only):

| arm | dense R@10 | B2, no rerank | B3, no rerank | X, no rerank | B2, depth 20 | B3, depth 20 | X, depth 20 |
|---|---|---|---|---|---|---|---|
| bge (cards) | 0.568 | 0.58 / 0.416 | 0.82 / 0.620 | 0.80 / 0.505 | 0.83 / 0.698 | 0.90 / 0.764 | 0.80 / 0.540 |
| Qwen3 1,024 | 0.656 | 0.83 / 0.572 | 0.87 / 0.624 | 0.80 / 0.580 | | | |
| Qwen3 512 | 0.648 | 0.80 / 0.575 | 0.88 / 0.640 | 0.80 / 0.651 | 0.92 / 0.795 | 0.92 / 0.846 | 0.85 / 0.595 |
| CodeRankEmbed | 0.579 | 0.68 / 0.476 | 0.88 / 0.650 | 0.75 / 0.456 | 0.88 / 0.766 | 0.90 / 0.820 | 0.85 / 0.610 |
| CRK code + Qwen3 rest | | 0.72 / 0.481 | 0.88 / 0.652 | 0.85 / 0.526 | | | |
| CRK code + bge rest | | 0.73 / 0.489 | 0.87 / 0.656 | 0.85 / 0.493 | | | |

- At depth 20, Qwen3 512 against bge, paired by question: B2 MRR +0.097
  [+0.031, +0.174], B3 +0.082 [+0.025, +0.144], X +0.054 [-0.037,
  +0.180].
- The episodes don't move with any arm (Qwen3 512 against bge at depth
  20: MRR -0.009 [-0.023, +0.001]).
- The chunk windows carry the gain: Qwen3 over cards alone lands near
  bge (B2 0.63 / 0.454).
- The headers help, mostly in the dense ranking (chunks alone without
  them: MRR 0.531 against 0.609).
- 512 dimensions equal 1,024 within the noise, at half the storage.
- The per-corpus mixes beat neither model alone.

**Blind 5** (pre-registered, once: Qwen3 512 at depth 20 against bge at
depth 20): a relevant file in the top 5 for 56 questions against 52,
+0.067 [0.000, +0.150], so **not shown** by the pre-registration's bar;
5 questions won, 1 lost; MRR +0.024 and nDCG +0.039, both inside the
noise. The switch was made anyway, on the dev gains and no loss seen, and
the smoke test on the live schema afterwards (blind 3, spent) matched the
scratch schema's run (hit@5 0.92, MRR 0.847).

**The query server** (a `serve.sh` beside the reranker's on the host;
vLLM 0.28, `--runner pooling --convert embed`, on 127.0.0.1:8201, as the
transient unit `gomuxai-embed`, started at boot after the reranker's):
- **Sizing** (a 32 GB GPU, 8 GiB kept free for its other users): the
  weights take 1.12 GiB and vLLM's own overhead about 0.6 more, so a
  1.5 GiB budget can't hold one 2,048-token window. At a 2.0 GiB budget
  it takes 2.47 GiB in all (the CUDA context sits outside vLLM's share),
  with KV for 5,360 tokens. Queries alone: p50 8 ms, p99 14. Windows:
  102 a second, the same at 2.5 GiB.
- **The cut.** 2,048 tokens. Over the live windows: p50 797 tokens, p90
  1,406, p99 2,456, max 3,561; 1.4% are past 2,048 (25.5% past 1,024, so
  1,024 would cut a quarter).
- **The hang, and its fix.** With vLLM's defaults (chunked prefill and
  prefix caching on) and a step of 2,048 tokens, a request of two windows
  at the cut and a few others hung: they were prefilled in part, filled
  the KV cache, and a pooling request isn't preempted, so none finished
  until the client gave up. The server runs with
  `--no-enable-chunked-prefill --no-enable-prefix-caching`: each window
  is prefilled whole in one step and its blocks freed. Requests of 6.7K
  to 10.4K tokens then answer in 0.1 to 0.2 s; 4 clients of 8 windows at
  the cut, 0.74 s. A client that times out drops its connection, which
  vLLM takes as an abort.
- **Agreement.** candle on the CPU against the server, at 512 dimensions,
  over 16 windows, 8 queries and 2 windows past the cut: least cosine
  0.99987, the same nearest window for 8 of 8 queries.
- **The GPU batches.** A summary batch stops it with the reranker and
  starts both again after (`embed.state` beside `reranker.state`); a bulk
  embed batch stops it and starts it after. Search embeds on the CPU
  meanwhile.

**Measured, live path** (blind 3 at depth 20 on the scratch schema; medians
and p90s in ms):

| | at the project | at its scope |
|---|---|---|
| bge | 333 / 420 | 875 / 1,014 |
| Qwen3 512, server up | 406 / 561 | 737 / 864 |
| Qwen3 512, server down (the CPU) | 792 / 896 | 1,129 / 1,278 |

The answers are the same up or down (MRR 0.893 at the project, against
0.823 for bge).

The watcher's flush (a scratch project, its own crawl, sync and
`keep_up`):

| flush | server up | server down |
|---|---|---|
| a small edit (1 window) | 75 ms | 2.7 s |
| a file of 40 windows | 1.15 s (embed 950 ms) | 7 s (the card and 1 window; 39 wait) |

- Keeping bge's cards current costs about 110 to 160 ms a flush.
- Uncapped, the 40 windows on one thread each took 287 s, which is why the
  cap exists.

**Install** (2026-11-24):
1. The server and its boot unit.
2. The new tables filled with `agent index embed --model
   qwen3-embedding-0.6b --copy-from <scratch schema>`: 83,149 cards and
   152,883 windows copied, 13 cards and 218 windows embedded through the
   server, HNSW built, in 30 s; 640 MB in all.
3. The same command without `--copy-from` just before the flip (the
   watcher's changes since).
4. The flip: `[embedder]` (the model, `dims = 512`, the endpoint, `keep =
   ["bge-small-en-v1.5"]`), `[embedder.chunks] enabled = true` and the
   reranker's `flat_depth = 20` (the depth measured), then a restart of
   serve and the watcher.

**Roll back**: the previous configuration file back, and a restart of
serve and the watcher. bge's cards stayed current (`keep`). The Qwen3
tables then go stale; going forward again is step 3, then step 4.

## A warm query process

`agent index serve` keeps a database pool, the query embedder and the
reranker client warm, behind a local socket; `agent index search` sends
it the query and prints the answer, and runs cold (slower, same answer)
when it isn't up. No search pays model load or connection setup.

**Built** (`pg/serve.rs`): `agent index serve` listens on
`[serve].socket` (else `index.sock` beside `index.db`, so each `agent`
copy has its own) with `[serve].workers` (4) threads, each with its own
database connection and reranker client, sharing the query embedder,
whose model it loads at start. `search` on the pg store asks it first
(one JSON line each way) and runs cold when nothing answers there, when
the server is another build of `agent` (its path, size and modification
time), or when it runs another configuration (its fingerprint): those
say so in a hint. `search --cold` skips it. Warm and cold go through one
function (`serve::answer`: the caller's project from its directory, the
search, the query log, the behind-its-crawl hint), and a test checks
that the server's answers equal the cold ones, timings aside.

**Measured** (2026-11-23; the 64 dev questions, `--kind file`, from the
dev root; the reranker `Qwen3-Reranker-4B` on the local GPU; each
reranking variant with its own instruction suffix so the server's prefix
cache can't carry one variant's prompts into another's; a shared host
under load, so read the differences more than the absolutes):

| variant | CLI median | CLI p95 | in search, median | reranker median | reranker p95 |
|---|---|---|---|---|---|
| cold, no reranker | 215 ms | 285 ms | 164 ms | | |
| warm, no reranker | 134 ms | 185 ms | 129 ms | | |
| cold, rerank 50 | 903 ms | 1,039 ms | 849 ms | 691 ms | 808 ms |
| warm, rerank 20 | 417 ms | 488 ms | 413 ms | 286 ms | 342 ms |
| warm, rerank 50 | 804 ms | 913 ms | 799 ms | 677 ms | 793 ms |
| warm, rerank 100 | 1,525 ms | 1,708 ms | 1,520 ms | 1,414 ms | 1,575 ms |

The warm process saves about 80 ms a search (the embedding model's load
and the connection). The reranker's time is about 14 ms a candidate (texts
of up to 1,500 characters), so depth sets the cost: 20 keeps a search
under half a second, 100 costs a second and a half. None fell back.

## Freshness: a watcher and a stale check

Staleness is the main reason people drop an index, and 3 of blind 3's 10
cross-project misses were stale files. Two parts: a watcher keeps the
store current within seconds of a save, and a search never serves a
changed file silently.

**The watcher** (`agent index watch`, `watch.rs`; Linux, inotify), its own
process and unit, not part of `serve`: serve stays the read-only query
path (restarted with each build or configuration), and the watcher is
the one writer.
- **What it watches**: every indexed directory of the full-depth projects
  (what the crawl recorded), and a new one as it appears, unless the crawl
  skips it (`target/`, `node_modules/`, `.git/`, `build/`, git-ignored).
  Build output is never watched, so a cargo or npm build makes no events.
  Editors' swap and backup files are dropped. The crawl, too, now leaves
  out an untracked file under such a directory even when git doesn't
  ignore it (an old commit checked out, from before its `.gitignore`).
- **The skip list**: the directory names no walk goes into, for the
  crawl, the watcher and dreams alike, are a file a person may edit,
  `skip-dirs.txt` beside `index.toml` (one name per line, `#` comments;
  the built-in defaults with no file). `agent index skip list|add|remove`
  prints and edits it. A dream may only propose a change to it, which a
  person accepts or rejects (`agent dream proposals`).
- **Reloading**: `agent index root add|remove` and `skip add|remove` tell
  a running watcher on its socket (`{"reload": true}`): it reads the skip
  list again, runs the tidy (which crawls every root, a new one too),
  watches what the projects now hold, and drops the watches of what they
  no longer hold or the list now skips; `status` counts `reloads`. Before
  this a new root went unwatched and unembedded until the watcher was
  restarted (friction #67).
- **When it flushes**: once a project has been quiet for
  `[watch].settle_ms` (1 s), or `max_wait_ms` (10 s) after its first
  change, however busy it stays. It waits while git holds
  `.git/index.lock` (a checkout or rebase under way), up to `git_lock_ms`
  more. A burst is one flush. A project whose changed paths git all
  ignores isn't flushed.
- **A flush**: a quick crawl of those projects into the ledger (the
  crawl's own rename, carry and delete logic; no new projects from the
  roots, no cross-project edges or importance), then `sync_projects`: one
  transaction that reads only their rows (theirs from the ledger, from
  Postgres, and their model summaries), writes the changed rows by COPY
  and one upsert, cuts the changed files' chunks again, and updates
  files' mtimes. Then the embed of the cards it queued (bge-small, loaded
  once). The words' document counts wait for the next full sync, which
  recounts them.
- **The tidy**: a full crawl (edges across projects, importance, new
  projects) and a full sync (memory, friction, sessions, word counts).
  It runs once all has been quiet for `tidy_quiet_s` (10 min), at most
  every `tidy_every_s` (1 h), and at start, which catches up on what
  changed while nothing watched. Importance moves many rows a little,
  which is why it waits for the tidy: pg_search rewrites every row
  written.
- **Summaries**: the watcher never summarizes and never uses the GPU. A
  changed file shows its seed (its summary was written against another
  content key) until the next summary batch, and `stale` lists it. A
  file renamed or copied keeps the summary of its content: the sync
  shows a live summary written for the same content key under another
  id, as the summarizer would copy it.
- **The ledger**: `[ledger].path` names it, so the watcher, `crawl` and
  `sync` all use the one the schema was built from. A schema records the
  ledger its first sync came from and refuses a sync, a flush or a
  watcher from any other; `sync --adopt-ledger` moves it on purpose.
- **Its socket** (`[watch].socket`, else `index-watch.sock` beside the
  ledger) takes one JSON line: changed paths, or a status request.
  `agent index touch PATH...` is the entry point for edit hooks (it
  succeeds, with a hint, when no watcher runs), and `agent index watch
  --status` reads the counts.

**The stale check** (`pg/fresh.rs`, in `serve::answer`, so warm and cold
alike): for the files behind the returned hits (a symbol's or section's
file too), one query reads their indexed size, mtime and content key,
then each file is statted. A different size is a change. The same mtime
is no change. A different mtime means hashing the file as the crawl
keyed it (a git blob id; `git hash-object` before calling a git file
changed, for filters; the crawl's content hash outside git). A missing
file is gone. The hit is marked, not cut again: `stale: changed` or
`gone` in the JSON, and `(changed since indexed: read the file)` or
`(deleted since indexed)` after its path in the text. Its paths go to the
watcher's socket, so the next search is current. It isn't cut again
inline because search only reads, and many agents search at once: a
re-cut would write Postgres and the ledger from the read path, racing
the one writer. Nor would a re-cut fix the ranking, since the hit was
found on the old text. The watcher re-cuts within seconds anyway.
`entities.mtime` is added when missing, outside the versioned
migrations, so older agents still read the schema.

**Measured** (2026-11-24). The bench was a scratch schema holding idx's
content: 483k entities, plus three clones (about 2,250 files) added as
projects to edit. The watcher ran as a transient unit, watching 17,847
directories of 32 projects.

| what | result |
|---|---|
| save to searchable (a new word in a chunk, committed), 36 edits across 3 projects, one every 3 s | p50 1,288 ms, p90 1,396 ms, max 1,407 ms; each word's search found the edited file first |
| one flush (one file) | median 380 ms: crawl 161, sync 159, embed 59 (p90 752) |
| start: catch-up (a full crawl, sync and embed with nothing new) | 14 s, 11 s of CPU, peak 2.8 GB |
| idle | 0 ms of CPU in 300 s; 175 MB resident after the catch-up, 465 MB after the checkouts' big flushes (the heap is trimmed after each flush and tidy) |
| a 64-commit checkout (969 files changed) | about 1,150 events, one flush: rows committed about 7 s after (crawl 0.9 s, sync 5.8 s), vectors 20 s more (987 cards on 4 threads) |
| and back | 546 events, one flush; the project then matched git exactly (718 files, every content key its blob id) |
| cargo build in a watched tree (223 MB of `target/`) | no flush: `target/` isn't watched, and the few other events were git-ignored |
| the stale check, 40 searches (26 hits each, all current) | median 0.67 ms, p90 0.95 ms, against about 270 ms for the rest of the search; 1.3 to 1.6 ms with a changed file to hash |

Every embed thread a background process takes is a core an agent's build
doesn't get, so the watcher embeds on 4 (`[watch].embed_workers`). A big
checkout's cards take longer (731 cards: 23 s on 16 threads, 39 s on
4), but its text is searchable first: the sync commits before the embed.

## Eval: a tuning and regression loop at full scale

Every change runs the eval over all of the dev tree:

- **session episodes**: the 734 mined episodes (search patterns and the
  files then edited), scored as v1 scored them, at the same scope as
  `rg`;
- **synthetic queries from the local model**: a sample of entities per
  corpus, each with questions generated by a different prompt (and
  ideally a different model) than the asks the index stores, and
  checked not to repeat them, so the test doesn't read its own answers;
  a cloud model spot-checks a small sample of them for quality only;
- **sanity sets**: the blind 40 and the 32 used in tuning, reported
  apart and never tuned on again.

Each run stores its config fingerprint beside its numbers, so a
regression can be traced to the change that caused it.

**Built: `agent index eval`** (`agent-index/src/eval/`). The Python
harness (`eval/run_eval.py`, and the blind-3 and cross runners and
scorers beside it) is now one command, so anyone can measure a change
the same way:
- **Configurations and sets.** `agent index eval run --config NAME=FILE`
  (repeatable) runs graded question sets (`--qrels`) and session
  episodes (`--episodes`).
- **In process.** Every search goes through `serve::answer`, one store
  per configuration, with no server and no copy of the binary. rg and
  fd stay subprocesses, since they are what is measured.
- **Scopes.** A question with a project is asked `--under` it
  (`index@scope`), from it (`index@cwd`), and of rg in it. A question
  with project `"*"`, or with a `"projects"` list, is asked over the
  tree. Over the tree, rg skips what the index skips, plus the top-level
  directories the index holds only shallow, which it reads from the
  store. `--systems run_eval` asks run_eval.py's systems instead.
- **Episodes.** They are asked as run_eval.py asked them: each pattern,
  the episode's patterns interleaved, its ask, and the searches it took
  to reach a target.
- **Numbers.** hit@1, @5 and @10, MRR@10, nDCG@10, the kinds' checks
  (multihop, cross, why), and latency. Each configuration is compared
  with rg and with the others by a paired bootstrap (by question, or by
  session for episodes). It draws Python's own random numbers (MT19937
  as CPython seeds it), so an interval is the one the Python harness
  printed.
- **What a run leaves.** A directory (default
  `~/.local/share/gomuxai/eval/runs/TIME-LABEL`) with `results.jsonl`
  (ids, ranks, top files, grades and times, never a question's text),
  `summary.md` and `run.json`. `run.json` records the build, each
  configuration's fingerprint and sha256, each set's sha256, the
  store's size before and after (the watcher is live), and the
  reranker's health polls.
- **Comparing runs.** `agent index eval compare A B` is cmp.py, with
  its own resampling.
- **Guards.** `--heldout` spends a set in a ledger of spent sets, and a
  second run on it needs `--again "why"`. `--prereg` records a
  pre-registration's sha256. A run with a reranker that is down before
  it starts is refused. The eval's calls go to the query log as agent
  `eval`, and `usage` leaves them out without `--all`.

Parity, on idx with the reranker off, each set run by the Python harness
and then by `agent index eval` with the same binary and configuration:
- blind 2: 1,080 of 1,080 rows the same (its 9 systems, with its strata),
  and every bootstrap interval;
- blind 3: 180 of 180 the same (top 10, rank, grades), and
  blind3_score.py's every number and interval. This is with blind3.sh's
  four tree skips; the list read from the store adds a fifth (a large
  project held shallow since), which moves two of rg's top 10s, at the
  same ranks;
- the cross dev 20: 60 of 60 the same;
- the 734 episodes: the same 245 rows kept from 73 sessions, with the
  same drops, and 3,798 of 3,798 rows the same (patterns, interleaved
  episodes, asks, rg and fd); every interval and the searches-to-target
  table are the same.

With the reranker on, blind 3's ranks all agree, but 3 of 170 index top
10s differ below the first hit (from position 4 or 5). Two Rust runs of
the same differ as much (3 or 4 of 170, with the same ranks, recall@5
and MRR), so that is the reranker's own run-to-run variance, not the
harness.

## How v2 measured against rg

Every set below was written before the system it tests was final. Each
had a pre-registration saying what counts as the claim (a grade ≥ 1
file in the top 5, index against `rg` on the question's content words at
the same scope), scored with a paired bootstrap by question (10,000
resamples, fixed seed). No system was tuned on any of them, before or
after. Pre-registrations and results live with the eval data, outside
the repo.

| set | questions | index, top 5 | rg, top 5 | difference [95% CI] | verdict |
|---|---|---|---|---|---|
| held-out (v1's set) | 40 | 36 (reranker depth 50) | 27 | +0.225 [+0.075, +0.400] | holds |
| blind 2: three new projects | 60 | 54 | 19 | +0.583 [+0.450, +0.717] | holds |
| blind 3: harder, five more projects | 60 | 54 | 35 | +0.317 [+0.183, +0.450] | holds |

v1 tied `rg` on the held-out set (25 against 27, not shown).

**Blind 2, what each part is worth** (the same 60, top 5; secondaries,
not decisive): the full pipeline 54; without the reranker 40 (+0.233
[+0.100, +0.367]); without summaries 48 (+0.100 [+0.017, +0.183]);
without the asks 55 (−0.017 [−0.067, +0.033]: the asks add nothing to
top 5, and lift top 1 from 33 to 38); v1 30. Caveat: only 11 of the 60
questions are under 8 words, and long plain questions are `rg`'s worst
case. So the size of the gap owes something to the set's style.

**Blind 3** was written to be harder and fairer to `rg`: long
situations, multi-hop, cross-project, why-in-prose and short questions,
over five projects no earlier set used, with gomuxai as the partner in
cross-project questions. `rg` scores 0.58
here against 0.32 on blind 2. System under test: snapshot dad8abb,
schema `idx` after the full-tree summaries and the sessions sync,
reranker depth 20, `--kind file`. Secondaries:

- Top 1: 38 against 16. MRR@10 0.751 against 0.419.
- By kind, top 5 (each kind is small): long 14/9 of 15, multi-hop 12/8
  of 12, cross 6/1 of 10, why 12/10 of 13, short 10/7 of 10. Only
  multi-hop and cross are clear of zero on their own; why-in-prose shows
  no difference (`rg` already finds the doc).
- Multi-hop, every needed file in the top 10: 8 against 3 of 12.
  Cross, a right file from each project in the top 10: 2 against 0 of
  10, the weakest spot. Three of those ten were impossible for the
  index: their gomuxai answer was written after the index's crawl.
  Leaving those three out: 53 against 35 of 57, +0.316 [+0.175, +0.456].
- Reranker: top 5 54 against 50 without it (not shown), top 1 38
  against 27 (+0.183 [+0.033, +0.317]). It fixes the order more than
  the recall. Without it the index still beats `rg` 50 to 35.
- The soft project prior from the caller's directory: 49 of 50, the
  same as the hard project filter (48 of 50).
- Latency, median inside the index: 0.64 s in a project, 0.49 s over
  the tree, 0.26 s at cwd; `rg` 30 ms in a project, 1.4 s over the tree.

## Across projects

The weak spot blind 3 found: a question whose answer lies in two projects
("how do project A and project B each start at boot") got a
right file from each project into the top 10 for 2 of 10. One project
fills the candidates before the reranker sees the other (RRF over lists
that one project's vocabulary dominates, a cap per file and none per
project, the own-project prior ×3 from a caller inside one of them), a
question with two intents buries one, and the sessions' `touched` edges,
the one signal that already joins files across projects, went unused.

**Built** (`pg/cross.rs`, called from `search_entities` in `pg/search.rs`;
`[cross]` in the configuration, every switch off by default). It applies
only to a search that may span projects: no `--project`, no `--under`, the
crawled corpora (code and prose). Each part has its own switch:

- **Route** (`route`): the projects ranked for the query, from four
  lists, each scaled to its best and averaged with its weight: project and
  folder cards by BM25 on their summary, asks, seed and why
  (`route_summary`; each project's best three of the top 60 summed;
  pg_search's `kind` term keeps it to those cards), the query's vector
  against each project's card (`route_dense`; scaled from the worst
  project to the best; the folder cards have no index to reach them
  cheaply by kind), the fused candidates' mass (`route_fused`; each
  project's best three fusion scores among the top 100), and the projects
  the query names (`route_names`). Projects crawled shallow hold no files
  and aren't ranked. The routed projects are the best and those with at
  least `route_ratio` of its score, `route_projects` at most. Two or more
  make the query read as spanning projects for a caller in no project;
  from inside a project that takes an explicit sign (below) unless
  `route_caller`, since nearly any query has a second project with some
  mass. `--json` has `cross.routed` (the top eight with their scores) and
  `cross.spans`. The defaults are the dev set's best (below): the
  candidates' mass and the names, two projects; the summaries and the
  projects' vectors are off, because they routed worse.
- **Read as spanning before any list**: a query in parts (below), one
  with a comparing word (`both`, `each`, `versus`, `compare`, `differ`,
  `between`...), or one that names another project (a word equal to its
  name; with routing on, only once the route has that project among its
  routed ones, since a project's name can be a plain word too).
- **Sub-queries** (`split`): each spanned project searched again on its
  own (the fused statement, labels and pins included, with that project as
  its filter, which pg_search's pushdown makes cheap). Its best
  `split_depth` files' candidates join; a candidate scores the better of
  the tree's fusion score and the project's times `split_raise`, and
  `--explain` names the project's lists (`in alpha: bm25 #1`). Its own
  ranking (after the priors) picks the project's places in the pool.
- **Slots** (`slots`): the reranker's pool (`[reranker] flat_depth`, or
  `pool_depth`) gives each spanned project its best `slot_min` files (one
  candidate each, by its sub-query's ranking with `split`, else by place),
  lets none hold more than `slot_cap` of it, and holds no file more than
  `per_file` times; the pool goes first, in fused order, then the reranker
  orders it. Without routing, the slots go to the `slot_projects` with the
  most fused mass that clear `route_ratio`. No reranker, no pool, no slots.
- **Top cap** (`top_cap`): a `--kind file` answer's first `limit` hold at
  most that many files of one spanned project while another spanned
  project has files further down.
- **Own project** (`own_project`): when the query reads as spanning
  projects, the caller's own project counts this much (a dependency at
  most this) and the lists scoped to it are left out; if the route decided
  it after those lists ran, the fused statement runs again without them.
- **Touched** (`touched`): from the best `touched_seeds` files, the files
  sessions touched with them join the candidates. A neighbor's strength
  sums, over each seed and each session that touched both, the seed's
  weight (1/√place) times the session's IDF (ln of all touched files over
  the session's own: a session that touched hundreds says little about
  any two) times the neighbor's (ln of the sessions over those that
  touched it: every session reads `CLAUDE.md`), ×`touched_cross` for a
  neighbor in another project than its seed. The best `touched_max` gain
  `touched_weight` × the pool's last score × their share of the
  strongest, so at weight 1 the strongest neighbor reaches the pool.
- **A question in parts**: `agent index search "layout engine cache |
  build script cache"`, or `--query` for each further part (the warm
  process's `Request.queries`). Each part runs its own fused statement over
  the tree (lists only: no pins or labels), pooled as the sub-queries are
  (every candidate of a part joins, and its ranking picks the part's places);
  each part gets slots in the pool whatever `slots` says, and each pooled
  candidate is reranked against the part that scored it best (one call to
  the reranker per part). No model call on the query side: the caller,
  usually a frontier model, splits its own question.

`cross` in `--json`: `routed`, `spans`, `reason` (`routed`, `compares`,
`names <project>`, `queries`), `softened`, `sub` (the sub-queries run),
`touched` (candidates it raised) and `pool` (the reranker's pool by
project or part); `ms` gains `route`, `split`, `parts`, `touched`,
`slots`. With every switch off a search is what it was: the same
fingerprint (`[cross]` at its defaults isn't hashed), and the same top 10
as the snapshot before it on blind 3's ten cross questions.

**Measured** (2026-11-24, schema `idx` after the asks-off re-embed, the
frozen 7afa987 sessions configuration plus the reranker at depth 20, as
blind 3 ran it; `--kind file`, words cut to 12; paired bootstrap, 10,000
resamples). Tuned on a dev set of 20 fresh blind cross-project questions
(two or three projects each, a grade-2 file in each project) only; blind 3
(its 10 cross questions, and the 50 with a project) and blind 2 are
regression sets, run once per change. Main measure: a grade-2 file from
each project in the top 10, the questions asked over the whole tree. The
dev questions were also asked from inside their first project
(`index@cwd1`), where the own-project prior applies.

| change (alone, unless named) | dev, each project | dev from inside a project | blind 3 cross, each project | blind 3 / blind 2, top 5 in the project's directory |
|---|---|---|---|---|
| none | 5 of 20 | 0 of 20 | 2 of 10 | 49 of 50 / 47 of 60 |
| slots, by fused mass | 6 (+0.05 [0, +0.15]) | 0 | 2 | 49 / 47 |
| route, split and slots | 5 (0 [−0.15, +0.15]) | 2 | 2 | 47 / 45 |
| route and the top cap (5) | 5 | 2 | 1 | 49 / 47 |
| **touched** | **7 (+0.10 [0, +0.25])** | 0 | 2 | 49 / 47 |
| route and own project 1, route alone counting | 5 | 5 (+0.25 [+0.05, +0.45]) | 2 | **33** / 43 |
| route and own project 1, explicit signs only | 5 | 3 (+0.15 [0, +0.30]) | 2 | 47 / 46 |
| touched, route and own project 1 (explicit) | 7 (+0.10 [0, +0.25]) | 4 (+0.20 [+0.05, +0.40]) | 2 | 47 / 46 |
| all of route, split, slots, cap 5 and touched | 3 (−0.10 [−0.30, +0.10]) | 3 | 2 | 44 / 46 |

The rows with "explicit signs" and the touched row's regression columns
ran on 2948443, the rest on 6eda1d7 (the same code but for the explicit
rule; both bases gave the same counts). Top 5 at the question's scope
didn't move outside ±1 on any set (dev 18 of 20; blind 3, 53 of 60;
blind 2, 47 of 60), nor MRR@10 outside ±0.02 for touched. Latency inside
the index, median at the question's scope: touched within ±15 ms of the
base (noise on a shared host); a route about 15 ms; a split +190 to 300
ms (a fused statement per spanned project).

What the dev set showed (only it was looked into):

- **Coverage was rarely the problem.** In most misses both projects were
  in the top 10 already, with the wrong files. Searched inside each of
  its projects (`--project`, no reranker), 7 of 20 questions had a grade-2
  file in every project's own top 5, so a perfect route plus per-project
  places could reach about 7 of 20; but the reranker, scoring the whole
  question against each candidate, puts a project's design notes above
  the code file a person names (a doc on headless rendering at 0.96, the
  wanted script at 0.06), and per-project places don't change that.
- **Routing on summaries routed worse** than routing on the candidates
  themselves: both gold projects in the route's top k for 3 of 20 by the
  project and folder summaries, 6 by the projects' vectors, 12 by the
  fused candidates' mass, 14 with the names the question holds. Big
  projects with thousands of folder summaries win a summary list for
  most questions.
- **Sub-queries cost more than they found**: a project's own search
  brings its candidates up, but they then take the pool from better
  ones across the tree; a wrong span (7 of 20 routes) adds a project the
  question isn't about.
- **The session-touched boost** is the one change that helped at tree
  scope, the same 7 of 20 on four builds, no question worse, nothing
  lost on the regression sets. It works only where sessions touched
  files in two projects (16 of the 70 sessions with touched files), and
  the dev questions lean on the projects people work across most, so it
  may help the held-out set less, or more if its questions came from the
  same sessions.
- **The own-project prior** is what kills a cross question asked from
  inside one project (0 of 20). Softening it when the route alone finds a
  second project lifts that to 5 and costs 16 of 50 single-project
  questions, since almost every query has a second project with some
  mass; on explicit signs only (a name, a comparing word, parts) it lifts
  it to 3 (4 with touched) for 2 of 50 and 1 of 60.

The configuration sent to the held-out run was `[cross] touched = true`
(build 39f09f6, fingerprint a6ca93811d2b), run once as pre-registered:

- held-out 20: touched 8 vs 8, not shown.

So `[cross]` stays off by default, every switch of it, and the code stays
to be switched on and measured again: `touched` as sessions across
projects grow, `own_project` with explicit signs for an agent that
searches from inside a project, and parts (`--query`, ` | `) for a caller
that can split its own question (not measured here: no set has split
questions).

## Iterations

Each lands on its own, measured, and said plainly when it does:

1. Postgres store and the crawl writing to it; full text, trigram and
   vectors on the code corpus; RRF in SQL; the eval running against it.
   **Built** (the eval run is next): see "Iteration 1, as built".
2. The warm process and the reranker. **Built** (the eval with and
   without the reranker is next).
3. The prose, memory and friction corpora; grouped answers. **Built**
   (see "Corpora, composed"); bm25 the default lexical list.
4. Sessions as full scrubbed text. **Built** (see "Sessions as full text");
   the full index takes them at its next sync.
5. Summaries and asks for all of the dev tree, after preflight and the
   config review. **Built and run** (2026-11-24): 14,694 file summaries
   (run 16: 9,262 written in 46 min, alone on the GPU, about 1.5% failed,
   mostly too few asks), then the bottom-up pass. Folder summaries are
   composed only from their children's summaries, READMEs and names,
   giving 3,237 folders and all 27 projects in 4 min (0.5% failed).
   After a 50-unit preflight in a scratch schema and a skeptic's review,
   the sessions sync added 439 sessions in 14,759 scrubbed chunks. The
   ledger the summaries were written against is now `[ledger].path`, and
   `idx` refuses a sync from any other (see "Freshness"). Run 17, with the
   installed agent beside the watcher, summarized what came or changed
   since and retried the failures: 145 of 277 files written, then 61
   folders.
6. Query expansion and learned labels, from the eval's misses.

**For the next change of the summaries' fingerprint** (each of these
changes it, and with it every done file is asked again, so they wait to
go together):
- The checks still fail a file whose answer has fewer than two asks left
  (most of the failures that remain). Asks are off in the pipeline now
  (`[cards].asks`), so a check or prompt that stops failing a file on
  asks alone would let those summaries in.
- Real code that the filters skip: a `license.rs` of logic and a
  `Notice.tsx` (the boilerplate names match their stems whatever the
  extension), and a test whose doc comment says "generated from" (the
  generated-header pattern). Name rules could apply to non-code
  extensions only, and the pattern could need a generator's words, not
  the phrase alone.

## Iteration 1, as built

`rs/crates/agent-index/src/pg/` (`mod.rs` says the shape). The
`agent index` commands take `--store sqlite|pg` (default
`AGENT_INDEX_STORE`, else sqlite); the pg store's URL is `AGENT_INDEX_PG`
(a Unix socket, peer auth), its schema `AGENT_INDEX_PG_SCHEMA` (default
`idx`). The search flags and `--json` shape are v1's, so the eval harness
runs unchanged against it.

**Interim: the crawl's ledger stays in SQLite.** v1's crawl runs as
before and writes `index.db`, which keeps its bookkeeping (renames and
aliases, references, manifests, co-change state). `agent index sync` (and
`crawl --store pg`, which runs it) mirrors that into Postgres, and every
query reads Postgres only. Moving the bookkeeping into Postgres comes
after the first eval run.

- **Sync**: each entity's row (its card text, terms, labels, children's
  names) is hashed into a sync key; rows whose key changed go through
  COPY into a staging table and one upsert whose `RETURNING old.*,
  new.*` queues changed cards for embedding and changed files for
  chunking (read from disk again). Edges and the small tables are
  compared by digest. One transaction under an advisory lock.
  `sync --rebuild` empties the tables and writes everything again,
  keeping card vectors whose text is unchanged; a test checks it gives
  the same rows as the incremental path.
- **Schema**: versioned migrations in `pg/schema.rs` (1: the tables; 2:
  the BM25 indexes for the A/B, where `pg_search` exists; 3: `lexemes`).
  Entities carry `terms` (identifiers whole and split, the path's words),
  `tsv` (STORED: `simple` terms and labels at A; `english` title,
  summary and asks at B; seed and why at C; `simple` children's names at
  D), `tri` (names, paths, labels; not symbols) and `card_vec
  halfvec(384)` on HNSW. Files' text goes into `chunks` (Markdown by
  heading, else 400-line windows, at most 32 KB) with `english` and
  `simple` tsvectors; `simple` gets camelCase words added.
- **Search**: four round trips (context, label n-grams, the fused
  statement, the hits' details). The fused statement holds every list as
  a CTE (cards any word and every word, chunks, trigram, dense,
  labels), each again scoped to the caller's project and its
  dependencies, ranked `ROW_NUMBER() OVER (ORDER BY score DESC, id)`,
  fused by `UNION ALL` + `GROUP BY` RRF (k = 60); the pins (an
  identifier's definitions, a rare identifier's files) and the absent
  terms come out of the same statement, and v1's priors apply in Rust.
- **A crude IDF**: `lexemes` counts each lexeme's chunks (those in 50 or
  more), refreshed when chunks change. The chunk list takes a query's
  rarest terms until their chunks add up to 4,000, the cards' any-word
  list until 8,000, and the trigram list the 4 rarest terms; the rarest
  always counts. This was for speed (a common word's 20,000 chunks took
  most of a second to rank) and changes ranking: the eval should sweep it
  (`[search]` `chunk_budget`, `card_budget`, `trigram_terms` in the
  configuration file; the `AGENT_INDEX_TUNE` variable of 19fed9b is gone).
- **show, tree, related** read Postgres: the breadcrumb, the subtree and
  the neighbors are recursive CTEs; their output matches v1's (tested).
- `--lexical fts|bm25|both` picks native full text, `pg_search`'s BM25
  lists (written by the pg_search lane), or both; `AGENT_INDEX_EXPLAIN=1`
  prints the fused statement's plan.

### Measured (2026-11-23, the whole dev tree)

461k entities (377k symbols), 66.5k chunks from 38.6k files, 82.3k card
vectors, 47 projects. Timings on a shared 16-core host.

| step | time |
|---|---|
| cold crawl (v1, into the ledger) | 40.0 s |
| cold sync | 71.7 s (52 s of it cutting and indexing chunks) |
| no-op crawl + sync | 5.5 s + 5.8 s (reading both sides' keys; 4.0 s warm) |
| migration 2 (BM25 indexes over the full data) | about 4 s |
| embed, 82,308 cards (bge-small, CPU) | 673 s (122 cards/s) |

Size: 1,673 MB in all; `entities` 859 MB (316 MB of it indexes; HNSW 86
MB, `tsv` GIN 47 MB, `pg_search` 44 MB, trigram 24 MB), `chunks` 787 MB
(two GIN 148 MB, `pg_search` 82 MB), everything else under 10 MB each.
The ledger (`index.db`) is 646 MB more.

Search latency over the 64 dev questions (qrels `_s` and `_a`), from the
dev root, as the snapshot `agent-pg-19fed9b` runs:

| | median | p95 |
|---|---|---|
| CLI wall clock (process, model load, connect) | 247 ms | 411 ms |
| inside the search (sum of its stages) | 200 ms | 370 ms |
| the fused statement | 136 ms | 304 ms |
| embedding the query (the model loaded cold each run) | 58 ms | |

Before the IDF budgets and the join change the fused statement took 450
ms median, 1.65 s p95. `EXPLAIN ANALYZE` of the fused statement (the
snapshot, run in the gomuxai project; planning 2.4 ms each):

| query | execution | cards any word | trigram | chunks | dense |
|---|---|---|---|---|---|
| `held message delivery` | 111 ms | 15 ms | 17 ms | 24 ms | 4 ms |
| `parse_config` | 40 ms | 3 ms | 6 ms | 7 ms | 4 ms |
| a 14-word question about scrubbing logs | 244 ms | 29 ms | 77 ms | 31 ms | 4 ms |

The rest of each is the local (`here`) copies of the lists and building
the candidates' JSON. The trigram list is the next thing to make cheaper.

A smoke check on the first 10 short dev questions (`--kind file`, dev
root): hit@5 v1 2/10, v2 between 2/10 and 4/10 across the tuning steps
(MRR@10 0.07 for v1, 0.09 to 0.18 for v2). Ten questions decide nothing;
the eval does.

## Configuration (**built**)

Every search and pipeline tunable of the pg store is read at run time
from a TOML file (`rs/crates/agent-index/src/config.rs` holds the keys,
their defaults and what each does), so a tuning run changes a file, not
the code:

- **Where**: `--config FILE` on `crawl`, `sync`, `embed`, `search`,
  `show`, `tree`, `related` and `config`; else `AGENT_INDEX_CONFIG`; else
  `~/.config/gomuxai/index.toml` (`$XDG_CONFIG_HOME` honored). No file
  there means the built-in defaults, which are exactly what the code did
  before the file existed; a file named by `--config` or the variable
  must exist. Every key is optional, and an unknown key is an error, so a
  typo can't pass as a default.
- **`agent index config`** prints the resolved configuration (every key)
  as TOML with its source and fingerprint; `agent index config >
  index.toml` starts a file.
- **Fingerprint**: a hash of the resolved values (equal values, equal
  fingerprint, however the file is written). `search --json` reports it
  as `config` (the sqlite store's output has no such key, as before);
  `runs.fingerprint` records it for every sync and embed, with the whole
  configuration in `runs.config`; each logged query carries it in its
  `ms` JSON under `config` (a column of its own would be a migration,
  which older binaries refuse; it comes with the next one). `[store]`,
  `[ledger]` and `[log]` are left out of it: they say where things are,
  not what a search answers.
- **The query log**: each `search`, `show`, `tree` and `related` appends
  one JSON line to `~/.local/share/gomuxai/index-queries.jsonl`: the
  time, the agent (`AGENT_NAME`, else null), the command, its query and
  the flags given, the top five paths, the time taken, the store and the
  fingerprint (and the error, for a call that failed). It is one append
  whose errors are dropped, so it never fails or slows a command.
  `agent index usage` counts it by agent and command and by day, with
  each command's median and p90 time.

The sections:

| section | what it tunes |
|---|---|
| `[search]` | RRF's `rrf_k` (60); the default `limit` (10, what `--limit` 0 or none gives); `list_depth` (300 a list); `per_file` (2); `files_depth_factor` (8, for `--kind file`); `local_lists` (the copies scoped to the caller's project); the rarest-term budgets `chunk_budget` (4,000), `card_budget` (8,000), `trigram_terms` (4); `trigram_threshold` (0.6); `ts_rank_cd`'s `rank_weights` (D, C, B, A: 0.1, 0.2, 0.4, 1.0) and `rank_normalization` (1); `hnsw_ef_search` (100); the label n-grams' `label_max_words` (6) and `labels_per_run` (25); `exact_limit` (20); `rare_files` (3); `lexical` (`auto`: bm25 where the schema has pg_search's indexes, else fts; or `fts`, `bm25`, `both`) |
| `[lists]` | each list's weight in fusion, by name (`lexical`, `all words`, `full text`, `trigram`, `semantic`, `label`, `bm25`, `bm25 full text`); a `here` copy takes its list's; 1 when unnamed |
| `[priors]` | the pins (`pin_exact` 2, `pin_label` 1, `pin_rare` 1.5), `section` (−0.002), `importance` (0.004), `out_of_the_way` (−0.008), `fixture` (−0.03), `test` (−0.004), and the multipliers `own_project` (3) and `dependency` (2) |
| `[corpora.NAME]` | `boost`, multiplying a positive score in that corpus (1), and its group's `limit` and `budget` (code 8 and 700 tokens, prose 5 and 450, memory 3 and 200, friction 3 and 150, session 3 and 300) |
| `[sessions]` | `off`, `skip_cwd` (`style-distill`), `only_cwd` (a trial), `include_subagents` (false), `max_mb` (200), `chunk_chars` (2,000), `overlap_chars` (200); see "Sessions as full text" |
| `[memory]` | `dirs`: the note directories (`~/.claude/projects/*/memory`, `{data}/memory`; `~` home, `*` one directory, `{data}` the agent's data directory) |
| `[friction]` | `store` (another agent's `gomuxai.db`, read only; "" this agent's) and `off` |
| `[chunks]` | the cut (`max_lines` 400, `max_bytes` 32,768), `lexeme_floor` (50) and the `file_kinds` cut into chunks. A cut other than the default becomes part of each file's chunk key, so the next sync cuts every file again; a new floor recounts `lexemes` |
| `[cards]` | `children` named in a card's search text (200), `symbols` after `defines:` in the embedded card (60), `max_chars` (2,000), `asks` (false, the default: a model's asks stay stored, and the summarizer still writes them, but they are in no card, vector or text search, since blind 2 found they add nothing to the top 5; true: in the card, after the names, so in its vector, and in the text search); a change rewrites the cards and re-queues their vectors at the next sync (switching `asks` re-embeds the summarized cards: about 5,400 took about 4 minutes) |
| `[summaries]` | `withhold`: entity id prefixes (`"project:docs/notes/"`) whose model summaries stay out of the live index, their seeds showing; empty by default. Which summaries go live, not how they are written (`[llm.summaries]`). An answer that echoes the scrub's placeholder (`[redacted]`) fails summarize's check ("placeholder echo") |
| `[embedder]` | `model` (`bge-small-en-v1.5`, `qwen3-embedding-0.6b` or `coderankembed`; another is refused), `model_dir` (the Hugging Face cache's when empty; `embed --model-dir` wins), `batch` (512), `dims` (0: the model's; a Matryoshka cut), `query_instruction` ("": the model's), `max_tokens` (2,048), `[embedder.chunks]` (`enabled` false, `window_chars` 3,500, `header` full), `[embedder.corpus]` (a model per corpus); and, left out of the fingerprint, `endpoint` with `served`, `backend` (candle: the server first, the CPU behind it; http: the server only), `timeout_ms` (1,500), `batch_timeout_s` (60), `recheck_s` (30) and `keep` (models kept current, not searched); see "A code-trained embedder" |
| `[reranker]` | `enabled` (false), `endpoint` (`http://127.0.0.1:8100`), `model` (`Qwen/Qwen3-Reranker-4B`), `instruction`, `timeout_ms`, and `depth` per corpus (code 50, prose 20, session 20, memory 10, friction 10), `doc_chars` (1,500), `max_tokens_per_doc` (512), `keep_pins` (true), `flat_depth` (50, a flat search's one pool); see "Reranker" |
| `[serve]` | `workers` (4) and `socket` ("" for `index.sock` beside `index.db`) of `agent index serve` |
| `[store]` | the store a command uses when neither `--store` nor `AGENT_INDEX_STORE` says: `kind` ("" is sqlite, or `pg`), and the pg store's `url` and `schema` when `AGENT_INDEX_PG` and `AGENT_INDEX_PG_SCHEMA` aren't set ("" is `idx`); not in the fingerprint |
| `[ledger]` | `path`: the crawl's ledger ("" is `index.db` beside the binary's `gomuxai.db`, which ties it to one copy of the binary); every sync of a schema should use the ledger the schema was built from; not in the fingerprint |
| `[log]` | the query log: `enabled` (true) and `path` ("" is `$XDG_DATA_HOME/gomuxai/index-queries.jsonl`); not in the fingerprint |
| `[watch]` | `agent index watch`: `socket` ("" is `index-watch.sock` beside the ledger), `settle_ms` (1,000), `max_wait_ms` (10,000), `git_lock_ms` (60,000), `tidy_quiet_s` (600), `tidy_every_s` (3,600), `embed_workers` (4: the embedder's threads in the watcher, with bge alone; a bulk `agent index embed` takes half the logical CPUs), `cpu_texts` (2: with the embedder's server down, the cards and windows a flush embeds on the CPU; the rest wait); not in the fingerprint |
| `[cross]` | answers that span projects, every switch off: `route` (`route_cards` 60, `route_per_project` 3, `route_summary_best` 3, the lists' weights `route_summary` 0, `route_dense` 0, `route_fused` 1, `route_names` 1, `route_fused_depth` 100, `route_projects` 2, `route_ratio` 0.5, `route_caller` false), `split` (`split_depth` 30, `split_raise` 1), `slots` (`slot_min` 5, `slot_cap` 0.6, `slot_projects` 2, `pool_depth` 0: the reranker's `flat_depth`), `top_cap` (0), `own_project` (0: the priors stand), `touched` (`touched_seeds` 10, `touched_max` 10, `touched_weight` 1, `touched_cross` 1); at its defaults the fingerprint is the one from before the section; see "Across projects" |
| `[bm25]` | the BM25 card list's field boosts (`terms` 10, `labels` 10, `title` 4, `summary` 4, `asks` 4, `seed` 2, `why` 2, `symbols` 1), for `--lexical bm25|both`; `pushdown` (true: kind, project, corpus and `--under`'s projects filtered inside pg_search's query; see "Filters inside the BM25 query") |
| `[llm.summaries]` | `agent index summarize`: `endpoint` (OpenAI-compatible; empty is none), `model`, `temperature` (0.2), `max_tokens` (500), `timeout_ms`, `seed`, `concurrency` (48), `retries` and `backoff_ms`, `max_connection_errors` (8), the clip (`clip_tokens` 6,000; a doc's `doc_head_tokens` 4,500 and `doc_tail_tokens` 1,500; `max_input_tokens` 9,000; `preclip_chars`; `symbols_chars` 2,500), the floors (`min_chars` 200, `min_lines` 5, `json_max_bytes`), `pause_file`, `scrub_words`, and the served `dtype` and `quantization`; every key is in each summary's fingerprint |
| `[llm.query_rewrite]` | `endpoint`, `model`, `temperature`, `max_tokens`, `timeout_ms`, for a later iteration |

Moving the values changed no result: with no file, and with a file of
every default written out, the 64 dev questions from the dev root and
from a project, default and `--kind file` (256 searches), gave output
identical to 19fed9b's, timings aside; a test checks the same on a
fixture tree, and that a changed value changes the fingerprint and the
results. The v1 SQLite path keeps its constants.

## Feature check

The host runs PostgreSQL 18.7 with `pgvector` 0.8.7 and `pg_trgm` 1.6, in
a user cluster reached by Unix socket only, with peer authentication.
Checked by hand on it:

- `halfvec(384)` with an HNSW index (`halfvec_cosine_ops`), and
  iterative scans (`hnsw.iterative_scan = relaxed_order`,
  `hnsw.max_scan_tuples` 20,000 by default, `hnsw.ef_search` 40).
  **Gotcha:** these settings exist only once the `vector` library has
  loaded in the session; before that, `SET hnsw.iterative_scan` silently
  creates a placeholder that does nothing. The warm process runs `LOAD
  'vector'` (or the cluster preloads it) before setting them.
- `pg_trgm`: `word_similarity` and a trigram GIN index.
- Full text: `websearch_to_tsquery` handles phrases, `or` and negation
  (`"held message" -draft or wake` → `'held' <-> 'messag' & !'draft' |
  'wake'`). The `english` parser splits `snake_case` into words but not
  `camelCase` (`gatherGlobal` becomes one stemmed token, `gatherglob`),
  and stems identifiers. So names and terms use the `simple`
  configuration (no stemming), with Rust adding camel and snake splits
  beside the whole name; prose uses `english`.

From a check of the current releases (2026-11-23, against primary docs
and release notes):

- **Postgres 18** (18.7 current; 19.0 shipped days ago, but neither
  `pg_search` nor VectorChord supports it yet, so 18 is the target).
  Useful here: B-tree skip scan for composite filters, `uuidv7()`,
  `OLD`/`NEW` in `RETURNING` (a crawl's upsert reports which rows' keys
  changed, to queue them for embedding, in one round trip), parallel GIN
  builds, async I/O for scans and vacuum. Virtual generated columns
  can't be indexed, so the `tsvector` columns are `STORED`.
- **Native full text has no global IDF.** `ts_rank` and `ts_rank_cd`
  score term frequency and proximity in the row, not how rare a term is
  across the corpus, so a rare identifier counts no more than a common
  word: the weakness v1's evaluation exposed. Limits: 256 positions per
  lexeme and 1 MB per `tsvector`, so long files go in as chunks.
- **`pg_trgm`**: put the indexed column on the left (`name %> 'query'`,
  `ORDER BY name <->> 'query'`) for `word_similarity`. GIN serves
  filters, only GiST (`siglen` tunable) serves `ORDER BY` distance.
- **`pgvector` 0.8.7**: `halfvec` indexes up to 4,000 dimensions (so an
  8B embedder's 4,096 needs a projection or `vector` capped at 2,000:
  measure); with `relaxed_order`, re-sort through a `MATERIALIZED` CTE
  with `ORDER BY dist + 0` (on Postgres 17 and later the planner drops
  the sort otherwise); HNSW builds fast only while the graph fits in
  `maintenance_work_mem`. For a selective filter (one project), a
  partial index or partitioning can beat a global index with iterative
  scans: measure.
- **RRF**: no extension does it natively; every reference (pgvector's,
  ParadeDB's, Supabase's) uses CTEs with `RANK() OVER (ORDER BY score,
  id)` and either a full outer join or `UNION ALL` + `GROUP BY` for
  three or more lists. v2 uses the latter.

### ParadeDB `pg_search`: the default lexical list, measured

**Measured** (the pg_search lane's A/B, 2026-11-23, snapshot 19fed9b,
`--kind file`, found in the top 5; paired bootstrap, 10,000 resamples;
v1 was run days earlier on an older tree, a reference only):

| set, scope | fts | bm25 | both | v1 | fts vs bm25, recall@5 |
|---|---|---|---|---|---|
| held-out 40, in the caller's directory | 19 | 29 | 27 | 29 | −0.250 [−0.375, −0.125] |
| held-out 40, limited to the project | 26 | 25 | 25 | 25 | +0.025 [−0.100, +0.150] |
| qrels_a 32, in the caller's directory | 19 | 25 | | | −0.188 [−0.344, −0.062] |
| qrels_a 32, limited to the project | 20 | 26 | | | −0.188 [−0.344, −0.062] |

On the session episodes (the agent's own patterns) every config, v1 and
`rg` sit at about 0.22 to 0.25 in the top 5; fts is ahead on single
identifier patterns limited to the project by 8 of 389.

Why: `ts_rank_cd` scores a term's frequency and proximity within a row,
with no inverse document frequency, so across 460,000 entities a common
word counts as much as a rare one, and other projects' files holding a
question's common words fill the any-word list ahead of the caller's
own. BM25 weighs each term by its rarity in the corpus. Limited to one
project the two are level. The rarest-term budgets (iteration 1) don't
explain the gap: with them off fts got 20 of 40. `both` adds nothing on
questions.

So `[search] lexical` is `auto` by default: bm25 where the schema has
pg_search's indexes (migration 2 builds them where the extension is
installed), fts otherwise; `fts`, `bm25` and `both` pick one, and
`--lexical` or `AGENT_INDEX_LEXICAL` override it for one search, whose
`--json` says which ran (`lexical`). Older snapshots keep fts unless
asked. What follows is the reasoning before the A/B.

**Filters inside the BM25 query** (built, `[bm25] pushdown`, on by
default). The card index has the filter fields as `pdb.literal` (one
term per value): project, kind, file kind, corpus. The card scan puts
kind, project and corpus in pg_search's query as terms, each
`pdb.const(0)`, so they add nothing to the score (a bare term adds about
2.4, and a plain `e.kind = 'file'` in the same WHERE is pushed down as
one too: a filter there must be `const`). `--under` goes in as the
projects that can hold its entities (the one holding the path, those
above it, those inside it) and the path is checked on the rows kept; the
local lists read `project` off the one scan's rows (a second scan for the
caller's projects cost 24 ms against 16). A schema whose card index
predates the fields gets them at connect, once, under the migration lock
(not a migration: older agents still read it; searches wait for the
build, about 2 s for 460,000 cards; a table takes one ParadeDB index, so
it is dropped first). Chunks keep their filters on the heap: a chunk
holds no project of its own.

The old scan had no cut to lose hits past: it scored every match and
filtered after, so the pushdown can only change speed. Measured
(2026-11-23, a copy of the live schema, the frozen embedder, reranker
off): 1,640 of 1,640 searches identical, ids and scores (held-out 40,
blind set 2's 60, the 64 dev questions; words and sentence; in the
project with `--under`, in the caller's directory, at the dev root,
`--project`, and with no `--kind`); the card scans and the fused
statement, median of each question's best of 3:

| scope | card scans, off → on | fused statement, off → on |
|---|---|---|
| `--project` | 15.3 → 4.0 ms | 78.7 → 66.8 ms |
| in the project (`--under`) | 15.4 → 4.4 ms | 313.5 → 301.6 ms |
| at the dev root | 15.4 → 14.9 ms | 97.9 → 95.8 ms |
| in the caller's directory (local lists) | 15.6 → 15.2 ms | 124.3 → 125.4 ms |

The lookup of the index's fields reads the one index by name in
`pg_class` first: a filter on `pg_indexes.indexdef` calls
`pg_get_indexdef` on every schema's indexes and fails on one another
process is dropping (tests' throwaway schemas share the database).

`pg_search` 0.27 (AGPL-3.0; used locally, not distributed) embeds
Tantivy: true BM25 with corpus IDF, and a `source_code` tokenizer that
splits camelCase and snake_case, the two things native full text lacks
for code. Against it: one covering index per table (columns fixed at
creation), write amplification on every update, vector search inside it
still beta, Postgres 15 to 18 only, and installing it into this cluster
means building with pgrx (its packages target Debian and RHEL) or
running ParadeDB's own image as a second cluster. So v2 starts on
native full text (installed, no new trust) and runs `pg_search` as an
A/B through the eval loop, in a second cluster, if a person says yes.
If BM25 wins on code clearly, it moves in.

### VectorChord: not now

VectorChord 1.1 (AGPL or ELv2) is built for tens of millions to
billions of vectors on disk (IVF with RaBitQ quantization). This index
holds a few hundred thousand to a few million vectors, comfortably
inside pgvector HNSW's range in memory; its BM25 sibling has no code
tokenizer, its documentation site was down on the day of the check,
and its speed claims are the vendor's own. Revisit if the vector count
grows past what HNSW holds in memory.

### Reranker

`Qwen3-Reranker-4B` (Apache-2.0, about 8 GB in BF16, 32K context):
81.2 on MTEB-Code, level with its 8B sibling and well above the older
`bge-reranker-v2-m3` (41.4), served by vLLM's pooling runner (`/rerank`)
or TEI. `Qwen3-Reranker-0.6B` (73.4) is the fallback when the GPU is
busy. `jina-reranker-v3.5` scores well but is licensed non-commercial.

**Built and measured** on the host's GPU (vLLM pooling runner, a
localhost endpoint, about 300-token candidates, p50 over 20 runs):

| model | GPU memory | 50 candidates | 100 candidates | throughput |
|-------|-----------|---------------|----------------|------------|
| Qwen3-Reranker-4B | 10.2 GB | 616 ms | 1,211 ms | 82 pairs/s |
| Qwen3-Reranker-0.6B | 3.7 GB | 130 ms | 256 ms | 390 pairs/s |

Both ranked a relevant passage first on five code-search checks. At
these speeds the 4B over four corpora at depth 50 would add about 2.4 s
to a query, so the eval decides between the 0.6B at depth 50 to 100,
the 4B at depth 20 to 30, and the 4B on the code group alone.
