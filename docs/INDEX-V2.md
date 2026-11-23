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
