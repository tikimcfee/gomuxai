# Evaluating `agent index search` against `rg` and `fd`

`docs/INDEX.md` keeps version 1 only if search clearly beats what an agent
already does with `rg` and `fd` on the same words. `run_eval.py` measures
that. It is stdlib Python 3 and reads data that stays out of the repo.

```bash
python3 -I run_eval.py --dev-root ~/dev \
    --qrels questions.jsonl \
    --evalset sessions.jsonl --evalset older-sessions.jsonl \
    --lexicon lexicon.jsonl --lexicon-root ~/dev/some-repo \
    --exclude-hit ~/dev/where-the-lexicon-was-built \
    --index-bin /path/to/agent --out eval-out
```

Without `--index-bin` it scores the baselines alone. It writes
`eval-out/queries.csv` (one row per query and system: rank of the first
target, files found, milliseconds, the top file) and `eval-out/summary.md`,
which it also prints; `summary-questions.md` and `summary-sessions.md` appear
as those parts finish. Use a copy of `agent` with its own `.gomuxai`, crawled
over `--dev-root`, never the installed one. Compare runs only when the index
was crawled over the same tree with the same settings.

## Data

- **Questions** (`--qrels`, JSON lines, repeatable): `{qid, query, stratum,
  rel: {"PROJECT:PATH": grade}}`: a plain question, and the files that
  answer it, graded 1 or 2 by someone who didn't see the systems' answers.
  This is the headline plain-words set.
- **Sessions** (`--evalset`, repeatable): `{session, cwd, searches, edits}`,
  and optionally `episode`, `ask`, `reads`, `harness`. A row is one episode
  of a session: what the person asked, the patterns the agent then searched
  for (`grep:PATTERN`, `rg:`, `find:`, `fd:` or `glob:`), and the files it
  went on to edit. The targets are the edited files, or the read files when
  nothing was edited. Paths may be relative to `cwd`. A session an earlier
  file holds is not read again from a later one.
- **Lexicon** (`--lexicon`): `{term, lay_phrasings, provenance}` with
  provenance as `path:line` under `--lexicon-root`. Each lay phrasing is a
  query; the provenance files are its targets. Secondary: see "The lexicon".

## What is compared

Two kinds of query, each given to both `rg` and the index:

| query | `rg` baseline | index |
|---|---|---|
| plain words: a question, an ask, a lay phrasing | `rg_words`: `rg -i -w -F` per content word, files ranked by distinct words matched, then matching lines | `index_words` (and `index_sentence`: the question as typed) |
| the agent's own patterns | `rg_patterns`: the pattern as the agent ran it (`fd` for a glob), files ranked by matching lines | `index_patterns`: the pattern's words |

`rg` on plain words is the weak baseline: plain words often aren't in the
code. `rg` on the agent's patterns is the strong one: the agent had already
turned the ask into code words, and the patterns were written for `rg`.

Where each looks is the part after `@`:

| `rg` | index | scope |
|---|---|---|
| `@project` | `@project` | only the session's repo: `rg` runs there, the index gets `--under` it. Like with like. |
| `@project` | `@cwd` | the index searches every project from the session's directory and applies its own prior for the caller's project. What an agent gets by default; it can lose a file to another project, which `rg@project` can't. |
| `@tree` | `@root` | the whole dev root, no project known. |

The index is asked for 50 entities (`--index-limit`), symbols and sections
count as their file, directory and project hits are passed over, and the
first ten files are kept (`--limit`). `--index-arg` passes extra arguments,
such as `--kind file`.

Over the tree, and for `rg_words` everywhere, `rg` and `fd` skip what the
index doesn't read: ignored files, files over 1 MB, and the index's skipped
directories (`SKIP_DIRS` in the script mirrors `agent-index/src/files.rs`).
Without that, package lists and data dumps take the top of every word
search. `rg_patterns@project` runs as the agent ran it.

Sets in the summary:

- the questions, one set per `--qrels` file, named for the file;
- `pattern`: one query per distinct pattern of an episode;
- `episode`: an episode's patterns as one list, the per-pattern lists
  interleaved, for `rg` and the index alike. `rg_patterns_best` is the best
  rank any single pattern reached, an upper bound no single list achieves;
- `lexicon`, and `lexicon_rest`: the phrasings whose targets are neither of
  the two most-cited files;
- `ask_opening`: the ask that opens a session; `ask_all` adds later ones.
  Both are few and noisy (many asks are conversation, not a request).

`most_cited_files` is a reference that reads no query: the files a set cites
most, the same list for every query. A system that can't beat it has learned
only which files are popular.

## Numbers

- recall@1, @5, @10: the share of queries with a target file in the top k,
  with the count. MRR: the mean of 1/rank of the first target, 0 when absent
  (ten files are kept, so it is MRR@10).
- Questions also get nDCG@10 from the grades (gain 2^grade - 1), and the
  share of a question's relevant files in the top 5.
- First minus second, per comparison, with a 95% interval from a paired
  bootstrap (10,000 resamples, fixed seed) over the queries both answered.
  Whole sessions are resampled (for the lexicon whole terms, for questions
  each question), since one session's queries aren't independent. An
  interval that excludes 0 is a difference.
- Searches to reach a target: the position, in the agent's own order, of
  the first pattern whose top 10 holds a target, counted the same way for
  `rg` and the index, on episodes with a scored pattern; and for each pair
  the mean on the episodes both reached.
- Milliseconds per query, median: the whole command (for the index that
  includes starting the binary and loading its model, which a long-lived
  process wouldn't pay), and the time the index reports for the search
  itself.

## What tilts it

Toward `rg`:

- A `grep:` pattern is tried as `rg` regex, then as basic `grep` syntax,
  then as a fixed string, until one matches: whichever the agent meant.
- The `pattern` and `episode` sets are patterns written for `rg`: regexes
  and literal strings. They test whether the index can stand in for `rg`,
  not whether it finds things from plain intent.
- A target the index never crawled (a project set shallow, a file over
  1 MB, an ignored file) is a miss for the index and may be a hit for
  `rg@project`.
- A copy of a target in a worktree counts as the target, and copies of one
  file take one rank.

Toward the index:

- The files exist now; the sessions ran against older trees. A target gone
  from its repo's main checkout is dropped for both, but text the session
  added is there for both to match, and the index also holds it as symbols
  and summaries.
- `rg` ranks only by match count, which favors long files; an agent reads
  the match lines and picks. Recall at 10 is fairer to it than recall at 1.
- When every target of an episode is outside the session's project (the
  summary counts them), nothing limited to the project can reach it, and
  `@cwd` can.

Either way:

- The lexicon file, and everything under `--exclude-hit` (its sources hold
  every phrasing beside its term), never count as a hit and take no rank.
  If the index has imported that lexicon (`agent index lexicon import`), it
  holds the answer key: score it before importing.
- Plain queries are cut to `--max-words` (12) words for both.
- A few sessions supply many of the patterns; the bootstrap resamples whole
  sessions, so the intervals show it, but the means lean on those sessions.

## The lexicon

Its targets are concentrated: when a few long files are the provenance of
most terms, `most_cited_files` scores high without reading the query, and
`rg`'s rank-by-match-count rides on the same thing. Read `lexicon` beside
`most_cited_files`, and prefer `lexicon_rest`.

## What is dropped, and counted in the summary

- Targets outside the dev root (memory files, scratch, tool state), and
  targets no longer in their repo's main checkout.
- A path in a git worktree is mapped to its main checkout by the worktree's
  `.git` file; a worktree that is gone is mapped by its name
  (`REPO/.claude/worktrees/NAME`, `REPO-wt/NAME`, `REPO-wt-NAME`, a sibling
  `REPO-NAME`) when that names a repo in the dev root. The file must then
  exist in the main checkout.
- Rows that ran under `/tmp`, `/run`, a `--skip-cwd` directory, or the agent
  tool's own data directory.
- Rows left with no target, or with neither a usable pattern nor an ask;
  repeats of an earlier row; a pattern repeated in its episode.
- Patterns with no word left once regex syntax, file extensions, language
  keywords (`pub`, `fn`, `func`) and words under three letters are taken
  out (`^#`, `*.go`); find patterns naming `.git`, `target` or
  `node_modules`; wide-context reads of a bundle (`.{240}word.{600}`).
- Asks with fewer than five content words, or that only point at a task
  file.
- Sessions whose project isn't known (they ran in the dev root or outside
  it) are scored over the tree only.
