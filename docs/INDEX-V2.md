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
piece is marked **built** when it is. Nothing is built yet.

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

- **full text**: a `tsvector` per row with weighted parts (A: title,
  name and labels; B: summary and asks; C: seed and why; D: body),
  ranked by `ts_rank_cd`, queried with `websearch_to_tsquery`. The
  default parser doesn't split identifiers, so Rust adds the split forms
  (`gather_global` and `gatherGlobal` both give `gather global`) to a
  `terms` column that feeds the vector beside the whole identifier.
- **fuzzy**: `pg_trgm` on names, paths and labels (`word_similarity` for
  a short query inside a longer name), with a trigram GIN index.
- **vectors**: `pgvector` `halfvec` columns with HNSW indexes, and
  iterative index scans, so a filtered query (one project, one corpus)
  still returns enough rows.
- **graph walks**: recursive CTEs over `edges` (`related`, graph
  expansion, "what depends on this").
- **fusion**: RRF over the lists as CTEs in one statement, so a search
  is one round trip.

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

## Reranker

A cross-encoder on the GPU scores (query, candidate) pairs for the top
50 to 100 of each corpus. The model is chosen from the current
state of the art that fits beside the other resident models (see
"Feature check"), served by a local endpoint, and is part of the tuning
loop: the eval measures the pipeline with and without it, and at
several candidate depths.

## A warm query process

`agent index serve` keeps a database pool, the query embedder and the
reranker client warm, behind a local socket; `agent index search` sends
it the query and prints the answer, and runs cold (slower, same answer)
when it isn't up. No search pays model load or connection setup.

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
2. The warm process and the reranker.
3. The prose, memory and friction corpora; grouped answers.
4. Sessions as full scrubbed text.
5. Summaries and asks for all of the dev tree, after preflight and the
   config review.
6. Query expansion and learned labels, from the eval's misses.

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

The rest (current releases and their sources: Postgres 18's new
features, ParadeDB `pg_search`, VectorChord, hybrid search in SQL, local
rerankers) comes from a check of the current releases, below.
