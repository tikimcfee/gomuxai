# Evaluating `agent index search` against `rg` and `fd`

`docs/INDEX.md` keeps version 1 only if search clearly beats what an agent
already does with `rg` and `fd` on the same words. `run_eval.py` measures
that. It is stdlib Python 3 and reads data that stays out of the repo.

```bash
python3 -I run_eval.py --dev-root ~/dev \
    --evalset sessions.jsonl --evalset more-sessions.jsonl \
    --lexicon lexicon.jsonl --lexicon-root ~/dev/some-repo \
    --index-bin /path/to/agent --out eval-out
```

Without `--index-bin` it scores the baselines alone. It writes
`eval-out/queries.csv` (one row per query and system: rank of the first
target, files found, milliseconds, the top file) and `eval-out/summary.md`,
which it also prints. Use a copy of `agent` with its own `.gomuxai`, crawled
over `--dev-root`, never the installed one.

## Data

- **Sessions** (`--evalset`, JSON lines, repeatable): `{session, cwd,
  searches, edits}`, and optionally `episode`, `ask`, `reads`, `harness`. A
  row is one episode of a session: what the person asked, the patterns the
  agent then searched for (`grep:PATTERN`, `rg:`, `find:`, `fd:` or `glob:`),
  and the files it went on to edit. The targets are the edited files, or the
  read files when nothing was edited. Paths may be relative to `cwd`. Give
  the file with asks first: a row with the same session and searches as an
  earlier one is dropped.
- **Lexicon** (`--lexicon`): `{term, lay_phrasings, provenance}` with
  provenance as `path:line` under `--lexicon-root`. Each lay phrasing is a
  query; the provenance files are its targets.

## What is compared

Two kinds of query, each given to both `rg` and the index:

| query | `rg` baseline | index |
|---|---|---|
| plain words: the ask, or a lay phrasing | `rg_words`: `rg -i -F` per content word, files ranked by distinct words matched, then matching lines | `index_words` |
| the agent's own patterns | `rg_patterns`: the pattern as the agent ran it (`fd` for a glob), files ranked by matching lines | `index_patterns`: the pattern's words, regex syntax stripped |

`rg` on plain words is the weak baseline: plain words often aren't in the
code. `rg` on the agent's patterns is the strong one: the agent had already
turned the ask into code words, which cost it turns the numbers don't show.

Each runs in two places: `@project` (the session's repo, for `rg`) and `@cwd`
(the session's directory, for the index, so its project prior applies), or
`@tree` and `@root`, the whole dev root with no project known. Compare like
with like: `index_*@cwd` with `rg_*@project`, `index_*@root` with `rg_*@tree`.

Sets in the summary:

- `lexicon`: one query per lay phrasing. The main plain-words set. Its
  `most_cited_files` row is a reference that reads no query: the files the
  lexicon cites most, the same list for every phrasing. When a few files
  hold most targets it scores high, and `rg`'s ranking by match count
  (which favors long files) rides on the same thing.
- `ask_opening`: the ask that opens a session. Noisy: many asks are
  conversation, not a request. `ask_all` adds asks from later episodes, and
  is a footnote.
- `pattern`: one query per distinct pattern of an episode.
- `episode`: an episode's patterns as one list: joined words for the index,
  the per-pattern lists interleaved for `rg`. `rg_patterns_best` is the best
  rank any single pattern reached, an upper bound no single list achieves.

## Numbers

- recall@1, @5, @10: the share of queries with a target file in the top k,
  with the count. MRR: the mean of 1/rank of the first target, 0 when absent.
- Index minus baseline, per comparison, with a 95% interval from a paired
  bootstrap (10,000 resamples, fixed seed) over the queries both answered.
  Whole sessions are resampled (for the lexicon, whole terms), since one
  session's queries aren't independent. An interval that excludes 0 is a
  difference.
- Searches to reach a target: for `rg`, the position in the agent's own
  sequence of the first pattern whose top 10 holds a target. For the index,
  1 if the plain ask finds it, else the ask plus each pattern tried in turn.
- Milliseconds per query, median and mean.

## What tilts it

Toward the baseline:

- A `grep:` pattern is tried as `rg` regex, then as basic `grep` syntax,
  then as a fixed string, until one matches: whichever the agent meant.
- It ran in the right repo, which the agent knew only because it was there.
- A copy of a target in a worktree counts as the target, and copies of one
  file take one rank.
- Index hits are entities. Several in one file leave fewer than ten files in
  a `--limit 10` reply, and a directory or project hit takes a rank no file
  target can match; `--index-limit` asks for more, and `--limit` still keeps
  ten paths.
- A pattern's words keep language keywords (`fn`, `func`, `pub`) that a
  person asking the index wouldn't type.

Toward the index:

- The files exist now; the sessions ran against older trees. A target that
  is gone from its repo's main checkout is dropped for both, but the index
  was crawled after the edits, so text the session added can match.
- `rg` ranks only by match count, a crude order; an agent reads match lines
  and picks. Recall at 10 is fairer to it than recall at 1.
- In searches-to-reach, patterns with no word in them (`^#`, `*.go`) cost
  `rg` a search and the index none.

Either way:

- The lexicon file itself holds every phrasing beside its term, so a hit on
  it never counts and takes no rank, for any system. If the index has
  imported that lexicon (`agent index lexicon import`), it holds the answer
  key: say so beside the numbers, or score it before importing.
- Plain queries are cut to `--max-words` (12) words for both.

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
  repeats of an earlier row; a pattern repeated straight after itself.
- From the `pattern` set only (they still count as searches in the agent's
  sequence): patterns with no word of three letters (`^#`, `^$`, `*.go`),
  find patterns naming `.git`, `target` or `node_modules`, and wide-context
  reads of a bundle (`.{240}word.{600}`).
- Asks with fewer than five content words, or that only point at a task
  file.
