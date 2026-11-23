# The index

Agents find things the slow way: `grep`, `find`, `ls`, read a README, `cd`,
read another. Every session starts that crawl again, and every session pays
for it in tokens and in wrong turns. Notes help, but notes are scattered:
a start-here per project, a CLAUDE.md per repo, docs that drift from the
code they describe.

The index is one queryable map of a dev tree (say `~/dev`): every project,
directory, file, symbol and doc gets a stable id, its place in the tree,
its relations to other entities, and a short prose summary of what it is
and why it exists. Each summary is tied to the content hash it describes,
so whether it is stale is a lookup, not a judgment. Summaries bubble up:
files explain directories, directories explain projects, projects explain
the tree, and concepts tie projects together.

An agent asks the index first (`agent index search "held message
delivery"`), gets a short ranked list of entities with their summaries and
paths on disk, and goes there to work in the normal layout. The index
doesn't replace the files. It replaces the crawl that finds them.

Dreams keep it true: code crawls the tree and keeps the structure exact,
and dreams refresh the summaries that went stale, from the code itself.

This document is the design. Nothing here is built; names are the plan.

## Why one index

Finding things is three problems: co-location (what sits near what),
identification (what is this, and why is it here) and queryability (can I
ask for it without already knowing where it is). Directories answer the
first. Names answer some of the second, and nothing answers the third
across projects. Documentation is a prose translation layer over code:
intent, connectivity, procedure. That translation exists at every level,
from file to directory, parent, sibling, project, and across projects, and
it rots at every level at its own pace. The index holds that translation
in one place, says per level how current it is, and lets agents query it.

## Two layers, linked

The index and dreaming's memory are separate layers with links between
them, not one store:

- **The index holds the what**: what a thing is, where it is, how it is
  built, why it exists, and what it relates to. Structure and purpose.
- **Memory holds the how**: how to work here, when things run, what broke
  and what fixed it, what to avoid. Procedure, tripwires, lessons, the
  friction box.

A fact goes in one layer. The test: would it still be true after a
rewrite that kept the code's behavior and layout? Then it describes what
the thing is, and belongs in the index. If it's about doing work on the
thing, it belongs in memory. "`agent-dream` gathers a dream's evidence"
is index; "build each worktree with its own target dir, or you get
false compile errors" is memory.

The links run both ways, by reference, never by copying text:

- Memory names entities: a start-here's pointers may cite index ids
  (`index:gomuxai:rs/crates/agent-dream`), and `memory check` resolves
  them as it resolves paths. The hand-kept "entrypoints" maps the
  dreaming survey proposes become `agent index tree` output instead.
- An entity knows what talks about it: `noted-in` edges, from the same
  backtick reading as `documented-by` but over memory directories, point
  at the memory notes and friction entries that name it, and `show` lists
  them. It also lists the recent sessions that worked there (sessions
  whose cwd or edited paths fall under it, from `agent sessions`' own
  discovery), as pointers: an id, a date, a title.

So an agent that asks the index "what is this" finds, beside the answer,
where to read "how do I work on it".

## Entities

### Ids

An entity's id is its project and its path in that project:

```
gomuxai                                     a project
gomuxai:rs/crates/agent-dream               a directory
gomuxai:rs/crates/agent-dream/src/gather.rs a file
gomuxai:rs/crates/agent-dream/src/gather.rs#gather_global   a symbol
gomuxai:docs/DREAMING.md#the-global-dream   a doc section
concept:nodes-and-wires                     a concept (cross-project)
@dev                                        the root of the tree
```

Ids read like paths, so an agent can guess one, and they survive edits.
They don't survive renames on their own: a crawl that sees a rename (git's
rename detection between the commit it last crawled and now) moves the
entity's summary and edges to the new id and keeps the old one as an
alias, so `show` of the old id still answers, saying where it went.

A project's name is its root directory's name, unique on the host
(`agent index root add` refuses a clash and takes `--name`). Its identity
across hosts and clones is its root commit (`git rev-list --max-parents=0
HEAD`), so the same repo checked out elsewhere, on another host or at
another path, is the same project. A git worktree is not a project of its
own: it resolves to its main repo through `git rev-parse
--git-common-dir`, and results are shown with paths in the caller's own
worktree when the caller works in one.

### Kinds

| kind | what | from |
|------|------|------|
| `root` | the indexed tree | `agent index root add` |
| `project` | a repo, or a directory marked as one | the root list, `.git` |
| `dir` | a directory in a project | git's tree |
| `file` | a tracked file, with a file kind: `source`, `test`, `doc`, `config`, `build`, `data`, `asset` | git's tree, extension and name rules |
| `symbol` | a definition: fn, struct, enum, trait, impl, class, interface, type, const, Make target, CLI command | a parser (below) |
| `section` | a heading in a doc | the Markdown itself |
| `concept` | an idea several entities share, across projects | dreams and people (below) |

Each entity row has: id, kind, parent id, project, its absolute path on
disk, file kind and language, size and line count, its **content key**,
a title (one line), a **summary** (at most three sentences: what it is),
a **why** (at most two: why it exists, what would break without it), the
key the summary was written against, who wrote it (seed, model and dream
id, or person), and when.

### Freshness keys

The content key comes from git for free:

- a file's is its blob id (`git ls-tree -r HEAD`, or `git hash-object`
  for a file changed in the working tree);
- a directory's is its tree id, which changes whenever anything under it
  does;
- a symbol's is a hash of its signature line and doc comment, so a change
  inside a body leaves it fresh;
- outside git (a project that isn't a repo), a file's key is a hash of its
  content and a directory's a hash of its listing.

A summary records the key it was written against. Comparing the two gives
each entity a state:

- `fresh`: the keys match.
- `drifted`: they differ, but the change is small: under a fifth of the
  lines changed (`git diff --numstat` between the summary's commit and
  now) and the file's set of public symbols is the same. Shown, not
  refreshed.
- `stale`: the change is large, the public symbols changed, or it has
  drifted across 20 commits. Due for a refresh.
- `seed`: only a summary code wrote (below) exists.
- `none`: no summary at all.

A directory has a second key, its **basis**: a hash of its children's
names, kinds and summary versions. Its summary goes stale when the basis
changes (a child added, removed or renamed, or a child's summary
rewritten), not when a child's content changes under a summary that still
holds. That damps the bubbling: an edit deep in a file reaches the
project's summary only if it changed what the file is.

### Edges

One table, `edges`: source, target, kind, weight, origin (`code`, `model`,
`person`), evidence, and the keys of both ends when it was made, so an
edge goes stale as entities do. Containment is the parent column, not an
edge.

| kind | meaning | made by |
|------|---------|---------|
| `depends-on` | a project or crate uses another | manifests: `Cargo.toml` (path and workspace deps), `go.mod`, `package.json`, `pyproject.toml`, `pixi.toml`; imports later, from the parser |
| `documented-by` | a doc names this file, dir or symbol | backticked paths and names in Markdown, found as `memory check`'s stale check finds them (`agent_dream::stale`), so a doc's references and the index agree |
| `tests` | a test covers a file | naming (`x_test.rs`, `test_x.py`, `x.test.ts`), and `#[cfg(test)] #[path]` |
| `co-changes` | changed in the same commits | `git log --name-only` over the last 2,000 commits, weight a Jaccard ratio, kept above a floor |
| `implements` | code realizes a design | a design doc's "built" markers, which name their files; model-proposed otherwise |
| `about` | an entity is an instance of a concept | dreams and people |
| `same-as` | one entity in two projects (a vendored copy, a port) | equal blob ids across projects; model-proposed for ports |
| `noted-in` | a memory note or friction entry names it | backticked paths in memory directories and friction text (the layers' link, above) |

Co-change is the cheapest strong signal there is: two files edited
together forty times are related, whatever their names say.

## Summaries

### Seeds: what code writes

Before any model runs, most entities already have a usable summary in the
code itself. A crawl takes, in order:

- a Rust crate's `description` and its `//!` module doc, a Go package
  comment, a Python module docstring, a JS/TS file's leading block
  comment;
- a symbol's doc comment (`///`, `/** */`, a docstring), first sentence;
- a doc's first heading and first paragraph;
- a directory's README, first paragraph;
- a Make target's comment, a CLI command's `about` line (`agent`'s own
  commands from its command tree).

These are `seed` summaries: free, exact to the code, often terse, never
a "why". Many files in a well-kept repo need nothing more.

### Model summaries: what dreams write

A model writes a summary where the seed is missing or thin, and for every
directory, project and concept. It writes bottom-up, so a directory's
summary is written from its children's summaries, its README and its
listing, never from all the code under it; a project's from its top
directories, README, CLAUDE.md and start-here; the root's from its
projects. Each level reads a bounded input, so cost grows with the number
of entities, not their size.

A summary answers two questions in at most five sentences: what is this,
and why does it exist. It names concepts it is about, from the concept
list or as new ones with a reason, and it lists three to five **asks**:
how a person or agent who doesn't know the name would ask for this
thing, in plain words ("the part that types messages into a busy
agent"). Asks become labels (see "Query expansion"). Code checks it
before it lands:

- lengths (summary 400 characters, why 300);
- every path or symbol it names in backticks exists (the stale check
  again), so a summary can't point at something that isn't there;
- the scrub (`agent_dream::scrub`) finds nothing to drop: no keys, no
  sign-in URLs, no pasted blocks;
- its `--expect` key is still the entity's content key (below).

Old summaries are kept (`summaries` holds every version), so a bad
rewrite is undone by pointing the entity back at the last one.

### Who writes them, and with what

Two writers, by level:

1. **Files and symbols: a local model, called by code.** For the many
   leaf entities, the summarizer isn't an agent: code reads the file
   (clipped to its first 6,000 tokens, plus its symbol list and seed),
   sends it to a local OpenAI-compatible endpoint with no tools, and
   takes back JSON (`summary`, `why`, `concepts`) it validates as above.
   The model can't read or write anything; it only answers. That is the
   strongest confinement there is, and on a local GPU it costs nothing
   but time. The endpoint is a setting (`index.llm`: URL and model);
   when it doesn't answer, leaves stay `seed` or `stale` and nothing
   fails. `agent` never starts a model server: that's the person's call.
2. **Directories, projects and concepts: a dream worker.** These need
   judgment over a few dozen children, and there are few of them (a few
   hundred across a large tree). A `summarizer` worker in a project
   dream writes them, through guarded verbs (below), on a cheap model
   from the roster (`roster pick --task edit`), or the local model by the
   same code path when it is up.

### Cost

A large dev tree here: about 20,000 tracked files in the repos one would
index fully, a third of them in one UI framework, plus about 3,000
directories.

- **Crawl** (code): `git ls-tree`, manifests, seeds and symbols for every
  changed file. Seconds cold, under a second incremental. Free.
- **Leaf summaries** (local model): only entities that matter get one,
  ranked by importance (fan-in on `depends-on` and `documented-by`,
  co-change degree, and how often agents read the file, from transcripts:
  below). Say 5,000 files at about 3,000 input tokens: 15M tokens of
  prefill and 0.5M out, well under an hour on one local GPU, once; after
  that, only what goes stale.
- **Directories and projects** (dream worker): a few hundred entities at a
  few thousand tokens each, cold; tens per dream after that. Each dream
  takes at most `index.budget` entities (default 40), so the index adds a
  bounded slice to a dream, never a second dream's worth. A global dream
  already reads about 9M cached input tokens; the concept pass (below)
  adds a few hundred thousand.

## Storage

A SQLite file of its own, `.gomuxai/index.db`, beside `gomuxai.db`, with a
crate of its own, `agent-index` (model, crawl, query, ranking), that the
CLI's `index` commands and the dream code call.

Not inside `gomuxai.db`, because `gomuxai.db` is opened on every hook
event of every agent and should stay small and quick to lock, while the
index is large (on the tree above: about 25,000 entities, maybe 150,000
symbols, an FTS index, later vectors; 100 to 300 MB) and written in bulk
by crawls. The index is also mostly a cache: everything but summaries,
model edges and concepts is rebuilt from disk by `agent index crawl
--rebuild`. What can't be rebuilt (summaries, `model` and `person`
edges, concepts) is keyed by content, so it survives a rebuild.

Tables:

- `projects`: name, root path, root commit, remote, depth policy, last
  crawled commit and time.
- `entities`: the columns above; `aliases` for renamed ids.
- `summaries`: every version: entity, content key, basis, text, why,
  writer, dream id, status (`staged`, `live`, `dropped`), written at.
- `edges`: as above.
- `concepts`: name, summary, origin, dream id.
- `labels`: every name an entity or concept answers to, with its kind
  (`pref`, `alt`, `hidden`, `ask`, `old`), origin (`code`, `model`,
  `person`, `learned`), weight and evidence (see "Query expansion").
- `concept_links`: `broader`, `narrower` and `related` between concepts.
- `search`: an FTS5 table (title, path, summary, why, symbol names,
  labels) with the porter tokenizer, and `search_tri`, FTS5 with the
  trigram tokenizer over names, paths and labels, for substrings and
  typos.
- `vectors` (later): entity, embedding model, dimensions, a half-float
  blob, the key it was made from.
- `queries`: each search, its caller, results, and what the caller
  opened next (below).

Per host. Every host crawls its own checkouts. Summaries are keyed by
content and prompt version, not by path, so they can travel: later, a
read-only `index` remote op answers `search` and `show` over the bus (an
agent on a laptop queries the hub's index), and `agent index pull HOST`
copies summaries whose content keys match blobs this host has, the same
blob summarized once for every host and every worktree. A pulled
summary is data, marked with its host.

## What gets indexed

Opt-in, like dreaming. `agent index root add DIR` adds a tree (host
state `index.roots`). Every repo directly under a root is a project; a
directory that isn't a repo becomes one with `agent index project add
DIR`. A project's depth policy says how far down it goes:

- `full`: dirs, files, symbols, sections.
- `files`: dirs and files, no symbols (large vendored or upstream trees).
- `shallow`: the project and its top-level directories, from READMEs
  (archives, third-party clones kept for reference).

By default a project inside the dream scope is `full`, and the rest of a
root `shallow` until a person deepens it, so a dream never summarizes
what it can't read and the index honors the same opt-in dreams do.

Only tracked files are indexed (`git ls-tree`, so `.gitignore` already
applies); outside git, the `ignore` crate's walk with the usual ignore
files. Never indexed, tracked or not: `.env*`, key and certificate files
(`*.pem`, `*.key`, `id_*`, `*.p12`), anything the scrub's credential
markers match in its first lines, and files over 1 MB (an entity with no
content). Binary files get an entity with a file kind and no content.

## Query surface

Every read command resolves a path argument against the caller's cwd, so
`agent index show .` works anywhere, and takes `--json`. Every list takes
`--budget TOKENS` (estimated at four characters a token): it fits its
output to the budget by cutting summaries to their first sentence, then
dropping the tail, and says how many it left out and how to see more.

```bash
agent index search "held message delivery"            # ranked entities
agent index search "deliver" --kind symbol --project gomuxai
agent index search "wire layout" --under ~/dev/glyph3d-rs --budget 800
agent index show gomuxai:rs/crates/agent-dream        # one entity, in full
agent index show .                                    # where am I?
agent index tree gomuxai --depth 2                    # the map: a repo map with summaries
agent index related gomuxai:rs/crates/agent-store/src/dreams.rs
agent index stale --project gomuxai                   # what needs a refresh, by importance
```

**search** prints one entity per line block:

```
1. gomuxai:rs/crates/agent/src/cli/deliver.rs   file  fresh
   /home/user/dev/gomuxai/rs/crates/agent/src/cli/deliver.rs
   Types held messages into an agent at its prompt once its input line
   has been empty for 10s. Why: a turn that ends without a Stop hook ...
   matched: summary "held", symbols deliver_held
2. gomuxai:docs/MESSAGE-DELIVERY-DESIGN.md#3-unified-architecture   section  drifted
   ...
```

The id, kind and freshness; the absolute path, to open; the summary; and
why it matched, so a wrong hit is quick to dismiss. Default 10 results and
1,500 tokens.

**show** prints the entity: its breadcrumb up to the root with each
ancestor's one-line title (where it sits, and what that means), its
summary and why with their freshness and how far the code has moved since
(commits and lines), its children by importance, its edges by kind, and
for a file its symbols. For a stale summary it says so first: a summary
is a hint, the code is the truth.

**tree** prints a subtree to a depth, one line per entity with its title,
the largest or most important children first, cut to the budget: aider's
repo map with prose. `agent index tree gomuxai --depth 1` is a project's
entrypoints map.

**related** lists neighbors by edge, weighted: what it depends on and
what depends on it, its docs and tests, what changes with it, and through
concepts, entities in other projects that share an idea with it.

### Ranking

1. **Lexical**: FTS5's bm25 over the porter table, columns weighted
   (title and name 4, path 2, summary 2, symbols 1, why 1), with
   identifiers split (`gatherGlobal`, `gather_global` → `gather global`);
   and the trigram table for substrings and typos in names and paths. Two
   ranked lists.
2. **Structure** as priors on the fused rank: the caller's own project
   first, then projects it depends on; an entity's importance (a PageRank
   over `depends-on`, `documented-by` and `co-changes`, as aider ranks its
   repo map); kind (a file or symbol over a section, unless asked); a
   small penalty for `stale`.
3. **Semantic** (later): cosine similarity between the query's embedding
   and each entity's, a third ranked list.

The lists merge by reciprocal rank fusion (k = 60), then the priors
apply. Fusion needs no tuned score scales, and adding the semantic list
later changes nothing else.

### Query expansion

People ask at a plain level of abstraction: "I want to work on the word
wall", "the thing that wakes agents up with messages", "where's the
glyph wires stuff". The names in the code are at another level:
`word-wall/`, `deliver.rs`, `WireLayout`. Closing that gap is the
index's central job, and information retrieval has a name for each
piece of it.

**A vocabulary, in the SKOS shape.** Every entity and concept answers to
labels: a preferred label (its title), alternative labels (aliases,
other spellings), hidden labels (typos, old names: found, never shown),
and asks (plain questions it answers). Concepts also have `broader`,
`narrower` and `related` links. This is the controlled-vocabulary shape
(a thesaurus, as SKOS writes one) rather than free tags, so expansion
has structure to follow. Labels come from four places:

- **code**: name variants (`word-wall`, `word wall`, `wordwall`,
  `WordWall`), crate and package names, CLI command names, README
  titles, the repo's remote name, and old ids after a rename;
- **model, at index time**: each summary's asks. This is document
  expansion (doc2query): writing the questions a document answers into
  the index once is cheaper than expanding every query, and it works
  with lexical search alone;
- **person**: `agent index alias add <id> "the word wall"`;
- **learned from use**: when a person's prompt names something in plain
  words and the session then works in one entity (edits files under it),
  the phrase becomes a candidate alias of that entity. A candidate
  becomes a label only once two or more sessions agree, as dreaming
  needs three for a preference; a dream reviews candidates as items.
  Only the short phrase is kept (at most six words, scrubbed at the
  point of reading under the machine's transcript rule), with the
  sessions as evidence. Agents' own `agent index search` queries count
  the same way: a query, then the entity the agent opened or edited, is
  click-through evidence.

**At query time**, in stages that stop as soon as the answer is good
(the top score clears a threshold):

1. **Label match.** The query, normalized, against `labels`: an exact or
   near match on an alias ("word wall") goes straight to the top.
2. **Thesaurus expansion.** Terms that are labels of a concept add that
   concept's other labels, and its narrower concepts', at a lower weight
   (each expansion is a separate weighted query, fused by rank).
3. **Pseudo-relevance feedback.** The top results' summaries and labels
   give the terms that set them apart (RM3's idea, by term weight against
   the whole index); a second query with them added catches the entities
   that use the code's words for what the person said plainly.
   Deterministic and cheap.
4. **A model rewrite**, only when the stages above came back weak or with
   `--expand`: the local model turns the request into the technical
   terms and a short hypothetical summary of what's wanted (query2doc
   and HyDE). The terms go to lexical search, the summary to semantic
   search once embeddings exist. About a second on a local GPU.
5. **Graph expansion.** From the top hits, follow edges with a decaying
   weight (spreading activation, as in personalized PageRank seeded on
   the hits): the project that contains them, their docs and tests, the
   concepts they're `about` and those concepts' members in other
   projects, the memory notes and recent sessions that name them.

So "I want to work on the word wall" comes back grouped: the project
itself and its entrypoints; its docs and research notes; the concepts it
shares with other projects; the memory notes about working on it; the
last sessions that did. Each group is a few lines, inside the budget.
`--explain` prints which stage found each result and through what label
or edge, so a wrong expansion can be seen and its label removed.

### Embeddings, later

Embed what an entity means, not its raw code: its title, path, summary,
why and symbol names, a few hundred tokens each. A small model on the
local GPU (an 8B embedding model, or a small CPU one as the fallback)
embeds 25,000 such texts in minutes; vectors are re-made only when the
text they came from changes. Truncated to 512 dimensions and stored as
half floats, that's about 25 MB, searched by brute force in tens of
milliseconds, so no vector extension is needed until the index is far
larger. Embeddings also propose concept edges: near neighbors across a
project boundary are candidates for the concept pass.

### Learning from use

`queries` records each `agent index search` (its text is an agent's own
query, not transcript content) and, from the caller's transcript later,
which paths it opened next: paths and counts only. A search whose top
results the agent skipped, followed by a `grep` that found the file it
then edited, is a miss: the dream that refreshes that file's summary gets
the query as evidence, and may add the agent's words as a learned label
(above). The same transcripts give each file a heat, how often agents
read it, as a count of paths read and nothing else; if any transcript
text is ever kept (a learned label's phrase), it is scrubbed at the
point of reading, under the machine's rule for mining transcripts. The
first ten read-and-search calls of past sessions make the evaluation
set (below).

### In every agent's briefing

Spawn adds a short "Finding things" section after the start-here
sections: run `agent index search` before `grep`/`find`; `agent index
show .` says where you are; then the project's own `tree --depth 1`, cut
to 15 lines, so an agent starts with the map already in hand. The read
commands (`index search|show|tree|related|stale`) join the pre-allowed
list. The `agent` skill gets the same lines.

Both change every agent on the host (the briefing and the pre-allowed
list live in the shared `instructions.rs`), so each needs a person's yes
and goes in with the manager's lead, who shares that file. Until then,
agents reach the index by name, and the commands prompt. Adding them to
dreams' `DREAM_READ_COMMANDS` is dreaming's own call.

## Building and refreshing

### The crawl: code, always cheap

`agent index crawl [--project P] [--rebuild] [--dry-run]`:

1. For each project, compare HEAD with the last crawled commit. If it
   moved, `git diff -M --name-status` between them gives what was added,
   changed, renamed and removed; with no last commit, `git ls-tree -r`
   gives everything. Working-tree changes come from `git status`.
2. Update entities and keys for those paths and their ancestor
   directories (a directory's tree id is in `ls-tree`). Carry renamed
   entities' summaries and edges over, keeping aliases.
3. Re-read changed files: file kind, lines, seed summary, symbols,
   sections, backticked references (`documented-by`), test naming.
4. Re-read changed manifests (`depends-on`).
5. Every 100 commits or a week, recompute co-change and importance.
6. Recompute freshness for what changed: a file's state, and the basis of
   each ancestor directory.

Symbols come first from regular expressions per language (definitions
in Rust, Go, TypeScript and JavaScript, Python, Swift, shell, Make):
ctags-grade, no dependencies, good enough to find. tree-sitter replaces
them later where precision pays (impl blocks, nesting, imports for
`depends-on` between files). LSP indexes (SCIP, rust-analyzer) are
precise but per-language and heavy to keep running; not needed for
finding things.

A crawl runs by hand, at the start of every `agent dream run` for the
projects the dream covers, and lazily on a query: a search in a project
whose HEAD moved since its crawl runs an incremental crawl first if it
takes under a second, and otherwise answers from what it has and says
how many commits behind it is.

### Refreshing summaries: dreams

A project dream refreshes its project's summaries. Code does everything
mechanical; the model writes text only.

1. **Gather** crawls the project, then lists its `stale`, `seed` and
   `none` entities by importance, and takes the first `index.budget`
   (40) as items of kind `index`, keyed by entity id, fingerprinted by
   content key and basis: an item counts as reviewed only when its dream
   commits, and comes back if the entity changed since.
2. **Leaves first, in code.** Before any agent starts, the leaf items
   (files, symbols) go to the local endpoint if it answers, and their
   validated summaries are written `staged`, tagged with the dream.
3. **The summarizer worker** (a new role, `summarizer`) writes the rest:
   directories bottom-up, then the project. It reads the entity, its
   children's summaries (live or staged), its README and listing, and
   writes through one verb:

   ```
   agent dream index put <id> --expect <content-key> --summary "..." \
       --why "..." [--concept <name>]... [--source commit:<hash>]...
   ```

   The verb refuses if the entity's key is no longer `<content-key>`,
   if the entity isn't an item of the caller's dream, or if the text
   fails the checks above. It writes a `staged` row tagged with the
   dream. `agent dream index get <id>` and `agent index` read commands
   are its reading verbs.
   The summarizer works down its items by importance and stops at the
   budget or when time runs short: a dream worker is given up after 30
   minutes idle or 90 running (`WORKER_IDLE_LIMIT`, `WORKER_RUN_LIMIT`),
   so it says `done` with what it has, and what it didn't reach stays
   an item for the next dream. Forty entities at a minute or two each
   fit.
4. **Ordering.** Today a project dream has one dreamer that commits the
   dream itself, and `dream_steps`' step machine sequences only the
   global dream's workers. With index items, the project dream gets the
   step machine too: the dreamer, then the summarizer, then code commits.
   The dreamer's `commit` becomes `done` when a summarizer follows (the
   verb says so), so the dream can't commit before the summaries land.
   A dream with no index items runs as today.
5. **Commit** makes the dream's staged rows `live`, the prior version
   kept; **abandon** marks them `dropped`. No snapshot and restore as
   memory needs: a staged row isn't seen by queries until it commits.
   The dream's status lives in `gomuxai.db` and the rows in `index.db`,
   so the two can't change in one transaction. The dream's status is the
   truth: commit marks the dream committed first, then promotes its
   rows, and every open of `index.db` (a crawl, a query) promotes the
   staged rows of committed dreams and drops those of abandoned ones, so
   a crash between the two heals on the next use.
6. **Citations.** A summary cites the content it describes as
   `key:<entity-id>@<content-key>` (and a `commit:<hash>` when it draws
   on history). `key:` is a new citation kind in `review.rs`, and
   `unknown_citations` accepts one when the entity is an index item of
   the dream at that key, or its key now is that one.

The global dream adds the concept pass (below) and nothing else. Project
dreams already run on new work in their project, so the index refreshes
where work happens, and quiet projects cost nothing.

A project nobody dreams over (in an index root, not the dream scope) gets
crawled and seeded, never summarized by a model, unless a person runs
`agent index summarize --project P`, which runs the leaf step against the
local endpoint, by hand.

### Confinement

As in dreaming, and nothing more trusted:

- The leaf summarizer is code calling a model with no tools. The model
  sees file text and returns JSON; code decides what lands.
- The `summarizer` worker is a dream worker: Claude in `dontAsk` with
  `DREAM_READ_COMMANDS` plus the index read commands; agy with
  `confined: true`, its fail-closed hook. It reads only under its dream's
  read roots (`dream_read`), which hold every project it summarizes,
  passed through `SpawnOptions.state` so they are written before its CLI
  starts. It writes only through `agent dream index put`, which joins
  `DREAM_AGENT_COMMANDS` and `dream_command_allowed`, with the role check
  in the workers table: `index put` is a summarizer's. It never writes a
  repo.
- Index text is data, never instructions, to every agent that reads it:
  a summary that says "run this" is a summary, and the briefing says so.

## Concepts across projects

Projects in one tree share ideas that no import shows: a node-and-wire
graph in a 3D code viewer and in its port to another language, a UI
framework one project builds on and another studies, the same LLM tooling
patterns in three repos. A `concept` entity names such an idea once,
with a summary and aliases, and `about` edges tie it to the entities in
each project that embody it. Cross-project relations go through
concepts, not entity-to-entity edges, so ten projects sharing an idea are
ten edges, not forty-five.

Concepts come from three places:

- **Code proposes candidates**: the same symbol or type name in two
  projects; a `depends-on` that crosses projects (a path dependency into
  another repo); equal blob ids (`same-as`); distinctive terms (high
  TF-IDF) that several projects' summaries share; later, embedding
  neighbors across a project boundary.
- **The global dream decides**, through the idea engine it already runs:
  the synthesizer proposes a concept, or an `about` edge, as an idea
  (`kind: concept` or `about`, citing the entities as `key:` citations);
  the critic, on another model family, checks it against the entities'
  code and summaries and approves or rejects; code applies the approved
  ones on commit. `concept` and `about` join the idea kinds the `idea`
  verb validates (each entity id exists, each cited key is current) and
  the apply step's. Convergence matters here as for pitches: a
  concept proposed again by a later dream adds a vote instead of a
  duplicate.
- **People**: `agent index concept add NAME "summary"` and `agent index
  concept link NAME ID...`.

A concept's summary bubbles too: it is rewritten from its members' when
they change enough, which is where the index says what the tree as a
whole is about, and where the vision stage (`docs/DREAMING-VISION.md`)
looks to borrow from one project for another.

## Prior art

Spotlight and other OS indexes keep metadata and text for every file,
updated from file-system events, behind one query; they know files, not
code, and nothing of why. ctags and LSP indexes (SCIP, rust-analyzer,
gopls) know symbols precisely but per language and per repo, and say
nothing of intent. Sourcegraph searches code across many repos, and its
code graph adds precise cross-repo references; summaries and concepts
aren't its business. Code-graph RAG systems (GraphRAG and the code
variants that build a call and import graph, summarize communities and
retrieve over both) are closest in spirit, but rebuild in bulk and don't
tie a summary to the hash it describes. aider's repo map ranks a repo's
symbols by a PageRank over references and fits the top of it to a token
budget, which `tree` and `--budget` borrow directly. RAPTOR builds a tree
of summaries of summaries for retrieval at every level of abstraction,
which is the bubbling here, but over clusters rather than a directory
tree a person already understands. From information retrieval come the
query side's pieces, all standard: SKOS-style vocabularies (preferred,
alternative and hidden labels; broader and narrower), document expansion
(doc2query), pseudo-relevance feedback (RM3), model rewrites (query2doc,
HyDE), click-through as relevance feedback, reciprocal rank fusion, and
spreading activation over a graph. What this index adds is the
combination: one tree across projects, content-keyed summaries whose
staleness is mechanical, refreshed by confined dreams within a budget,
a vocabulary learned from how people and agents name things, and
cross-project concepts decided by a generator and a critic.

## Version 1: useful in a day

1. Crate `agent-index` and `index.db`: `projects`, `entities`,
   `summaries`, `edges`, `labels`, `search` and `search_tri`.
2. `agent index root add|remove|list`, `project add`, and `crawl` (git
   keys, file kinds, seeds, code labels, regex symbols, sections,
   `depends-on` from manifests, `documented-by` and `noted-in` from
   backticks, `tests` by naming; co-change and importance once per
   crawl).
3. `agent index search`, `show`, `tree`, `related`, `stale`, with
   `--budget`, `--explain` and `--json`; label match, then lexical
   ranking with the project and kind priors, importance from fan-in
   only; `agent index alias add` for a person's labels.
4. The briefing section and the pre-allowed read commands, drafted for a
   person's yes (they change every agent on the host).
5. An evaluation set: from past sessions in a few projects (scrubbed at
   reading), the terms of each session's first searches and the file it
   then edited. v1 is useful when that file is in search's top five for
   most of them; the set measures every change after.

Seeds alone make it useful on day one: every entity has a path and
place, most have a seed, and search reaches symbol names, doc headings
and doc comments across every project at once.

Then, in order: `agent index summarize` against the local endpoint (by
hand), with asks; pseudo-relevance feedback and graph expansion; the
`summarizer` role, `index` items, `key:` citations and `dream index put`
in project dreams, with their step machine; query logging, misses and
learned labels; the model rewrite stage; the concept pass in the global
dream; embeddings; tree-sitter symbols; the bus op and `pull`.

## Open questions

- Should a dream's worker summarize leaves too when no local endpoint
  answers, at a cloud model's cost, or leave them `seed`? The plan says
  leave them.
- Is 40 entities per dream the right budget? Measure on the first runs:
  tokens per entity, and how many go stale per day of work.
- Symbols in `search` by default, or only with `--kind symbol`? Large
  frameworks have tens of thousands; the kind prior may be enough.
- Does a doc section deserve a model summary, or is its heading and first
  paragraph always enough?
- Summaries have only mechanical checks in v1, no critic. Should a
  critic on another model family check a sample of each dream's
  summaries against the code, as the global dream's critic checks ideas?
- How far should graph expansion reach by default before it costs more
  budget than it saves?
