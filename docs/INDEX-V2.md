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
  resumes by (content key, fingerprint), never by id. The fingerprint
  hashes the model id and snapshot commit (read from the server's
  `/v1/models` root), max length, server version, dtype and quantization,
  the prompt version and the whole request skeleton, schema, sampling and
  seed, thinking, the retry policy, the clip and fit limits, the filter,
  scrub and check versions, the build and `[llm.summaries]`. Each run is a
  `runs` row whose stats are rewritten as it goes and when it ends.
- **Freshness**: the sync shows a live summary (and its asks) over the
  seed while its content key is the entity's, so a changed file falls back
  to its seed; its card is queued for embedding as any changed card is.
- **A dead server**: 8 connection errors in a row, or `/health` failing 3
  times, stop the run within seconds; it saves its stats, a sync shows
  what was written, and it exits 1. A resume does the rest.

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

## Iterations

Each lands on its own, measured, and said plainly when it does:

1. Postgres store and the crawl writing to it; full text, trigram and
   vectors on the code corpus; RRF in SQL; the eval running against it.
   **Built** (the eval run is next): see "Iteration 1, as built".
2. The warm process and the reranker. **Built** (the eval with and
   without the reranker is next).
3. The prose, memory and friction corpora; grouped answers.
4. Sessions as full scrubbed text.
5. Summaries and asks for all of the dev tree, after preflight and the
   config review.
6. Query expansion and learned labels, from the eval's misses.

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
  which older binaries refuse; it comes with the next one).

The sections:

| section | what it tunes |
|---|---|
| `[search]` | RRF's `rrf_k` (60); the default `limit` (10, what `--limit` 0 or none gives); `list_depth` (300 a list); `per_file` (2); `files_depth_factor` (8, for `--kind file`); `local_lists` (the copies scoped to the caller's project); the rarest-term budgets `chunk_budget` (4,000), `card_budget` (8,000), `trigram_terms` (4); `trigram_threshold` (0.6); `ts_rank_cd`'s `rank_weights` (D, C, B, A: 0.1, 0.2, 0.4, 1.0) and `rank_normalization` (1); `hnsw_ef_search` (100); the label n-grams' `label_max_words` (6) and `labels_per_run` (25); `exact_limit` (20); `rare_files` (3) |
| `[lists]` | each list's weight in fusion, by name (`lexical`, `all words`, `full text`, `trigram`, `semantic`, `label`, `bm25`, `bm25 full text`); a `here` copy takes its list's; 1 when unnamed |
| `[priors]` | the pins (`pin_exact` 2, `pin_label` 1, `pin_rare` 1.5), `section` (−0.002), `importance` (0.004), `out_of_the_way` (−0.008), `fixture` (−0.03), `test` (−0.004), and the multipliers `own_project` (3) and `dependency` (2) |
| `[corpora.NAME]` | `boost`, multiplying a positive score in that corpus (1; only `code` exists yet) |
| `[chunks]` | the cut (`max_lines` 400, `max_bytes` 32,768), `lexeme_floor` (50) and the `file_kinds` cut into chunks. A cut other than the default becomes part of each file's chunk key, so the next sync cuts every file again; a new floor recounts `lexemes` |
| `[cards]` | `children` named in a card's search text (200), `symbols` after `defines:` in the embedded card (60), `max_chars` (2,000); a change rewrites the cards and re-queues their vectors at the next sync |
| `[embedder]` | `model` (this build runs `bge-small-en-v1.5` only and refuses another), `model_dir` (the Hugging Face cache's when empty; `embed --model-dir` wins), `batch` (512) |
| `[reranker]` | `enabled` (false), `endpoint` (`http://127.0.0.1:8100`), `model` (`Qwen/Qwen3-Reranker-4B`), `instruction`, `timeout_ms`, and `depth` per corpus (code 50, prose 20, session 20, memory 10, friction 10), `doc_chars` (1,500), `max_tokens_per_doc` (512), `keep_pins` (true); see "Reranker" |
| `[serve]` | `workers` (4) and `socket` ("" for `index.sock` beside `index.db`) of `agent index serve` |
| `[bm25]` | the BM25 card list's field boosts (`terms` 10, `labels` 10, `title` 4, `summary` 4, `asks` 4, `seed` 2, `why` 2, `symbols` 1), for `--lexical bm25|both` |
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

### ParadeDB `pg_search`: an A/B, not the default yet

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
