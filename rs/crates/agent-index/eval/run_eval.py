#!/usr/bin/env python3
"""Score `agent index search` against what an agent does with rg and fd.

Query sets (data that stays out of the repo):

  questions  plain questions with graded answers, {qid, query, stratum,
             rel: {PROJECT:PATH: grade}}. The headline plain-words set.
  sessions   past sessions as {session, cwd, searches, edits} and optionally
             {episode, ask, reads, harness}, one row per episode: what the
             person asked, the patterns the agent then searched for, and the
             files it edited (or, with no edits, read). A query is right when
             a target file is in the top k.
  lexicon    a glossary as {lay_phrasings, provenance}: each lay phrasing is
             a query, the files its provenance names are the targets.
             Secondary: a few long files hold most of its targets.

Two kinds of query, each given to rg and to the index alike:

  plain words        a question, an ask or a lay phrasing, as content words
  the agent's own    the patterns the agent really ran: it had already turned
  patterns           the ask into code words

Systems (the part after @ is where it looked):

  rg_words@project, @tree       rg -i -w on each plain word, files ranked by
                                distinct words matched, then matching lines
  rg_patterns@project, @tree    the agent's pattern with rg (fd for a glob),
                                files ranked by matching lines
  index_words@..., index_patterns@...
      @project   agent index search, only hits under the project (--under):
                 the same scope rg@project has
      @cwd       run in the session's directory, every project searched, the
                 index's own prior for the caller's project applied
      @root      run in the dev root: no project known, like rg@tree

Stdlib only; run with `python3 -I run_eval.py --help`. README.md beside this
file says what each choice does to fairness.
"""
import argparse
import csv
import json
import math
import os
import random
import re
import subprocess
import sys
import time
from collections import Counter, OrderedDict

STOP = set(
    "a about all also an and any are as at be been but by can could did do does each for "
    "from get had has have how i if in into is it its just let like make me more my need "
    "not now of on one only or our out please see should so some than that the their them "
    "then there these they this to up us use vs want was we what when where which who why "
    "will with would you your".split()
)
# Words of a pattern that are the language's, not the agent's.
KEYWORDS = set(
    "fn pub struct impl let mut use mod enum trait const static func type var def class "
    "import export function return async await self crate super".split()
)
EXTENSIONS = "rs|py|ts|js|go|md|mjs|cjs|jsx|tsx|toml|json|yaml|yml|txt|sh|wgsl|html|css|swift|c|h"
# What `agent index crawl` never reads (agent-index/src/files.rs): rg and fd
# skip the same wherever they stand in for a search of what the index holds.
SKIP_DIRS = (".git", "target", "node_modules", ".venv", "venv", "__pycache__", ".pixi",
             ".mypy_cache", ".pytest_cache", ".tox", "dist", "build", ".gomuxai", ".cache")
MAX_FILESIZE = "1M"
# Directories a find/fd pattern names when the search is about the build or
# the VCS, not about the code.
TOOLING_DIRS = {".git", "target", "node_modules"}
WIDE_CONTEXT = re.compile(r"\.\{\d{3,}\}")  # .{240}word.{600}: reading a bundle
GLOB_KINDS = ("find", "fd", "glob")
MIN_ASK_WORDS = 5  # fewer content words than this is a greeting
TASK_FILE = re.compile(r"\btask\.md\b", re.I)  # "your task is in .task.md"
NOT_FILES = ("dir", "project", "root")  # index hits that no file target can match


# ---------------------------------------------------------------- paths

class Paths:
    """Maps any path to the same file in its repo's main checkout."""

    def __init__(self, dev_root):
        self.dev_root = os.path.realpath(dev_root)
        self._repo = {}

    def repo_of(self, d):
        """(checkout top, main checkout top) for the repo holding directory d."""
        chain, cur, found = [], d, None
        while True:
            if cur in self._repo:
                found = self._repo[cur]
                break
            chain.append(cur)
            dotgit = os.path.join(cur, ".git")
            if os.path.isdir(dotgit):
                found = (cur, cur)
                break
            if os.path.isfile(dotgit):
                found = (cur, self._main_of_worktree(dotgit) or cur)
                break
            parent = os.path.dirname(cur)
            if parent == cur:
                break
            cur = parent
        for c in chain:
            self._repo[c] = found
        return found

    @staticmethod
    def _main_of_worktree(dotgit):
        try:
            with open(dotgit, encoding="utf-8") as fh:
                line = fh.readline().strip()
        except OSError:
            return None
        m = re.match(r"gitdir: (.*)/\.git/worktrees/[^/]+/?$", line)
        return m.group(1) if m else None

    def _by_convention(self, path):
        """A worktree that no longer exists, mapped by how its path is named:
        REPO/.claude/worktrees/NAME, REPO-wt/NAME, ANY/REPO-wt-NAME, and a
        sibling REPO-NAME, for a REPO in the dev root."""
        m = re.match(r"^(.+)/\.claude/worktrees/([^/]+)(/.*)?$", path)
        if m and not os.path.isdir(os.path.join(m.group(1), ".claude/worktrees", m.group(2))):
            return m.group(1) + (m.group(3) or "")
        m = re.match(r"^(.+)-wt/([^/]+)(/.*)?$", path)
        if m and os.path.isdir(m.group(1)) and not os.path.isdir(f"{m.group(1)}-wt/{m.group(2)}"):
            return m.group(1) + (m.group(3) or "")
        m = re.match(r"^(.*)/([^/]+?)-wt-([^/]+)(/.*)?$", path)
        if m and not os.path.isdir(f"{m.group(1)}/{m.group(2)}-wt-{m.group(3)}"):
            main = os.path.join(self.dev_root, m.group(2))
            if os.path.isdir(main):
                return main + (m.group(4) or "")
        if self.in_dev(path) and path != self.dev_root:
            first, _, rest = os.path.relpath(path, self.dev_root).partition("/")
            if not os.path.isdir(os.path.join(self.dev_root, first)):
                cut = first.rfind("-")
                while cut > 0:
                    main = os.path.join(self.dev_root, first[:cut])
                    if os.path.exists(os.path.join(main, ".git")):
                        return os.path.join(main, rest) if rest else main
                    cut = first.rfind("-", 0, cut)
        return path

    def canon(self, path):
        """The path in the main checkout (which may not exist there)."""
        path = self._by_convention(os.path.normpath(path))
        d = path if os.path.isdir(path) else os.path.dirname(path)
        while d and not os.path.isdir(d):
            d = os.path.dirname(d)
        repo = self.repo_of(d) if d else None
        if repo and repo[0] != repo[1]:
            path = os.path.join(repo[1], os.path.relpath(path, repo[0]))
        return os.path.normpath(path)

    def in_dev(self, path):
        return path == self.dev_root or path.startswith(self.dev_root + os.sep)

    def project_of(self, cwd):
        """(directory the baseline searches, whether it is a project)."""
        c = self.canon(cwd)
        while c and not os.path.isdir(c):
            c = os.path.dirname(c)
        if not c or not self.in_dev(c):
            return self.dev_root, False
        repo = self.repo_of(c)
        if repo and self.in_dev(repo[1]):
            return repo[1], True
        return c, c != self.dev_root


def inside(path, d):
    return path == d or path.startswith(d.rstrip(os.sep) + os.sep)


# ---------------------------------------------------------------- words

def pattern_words(kind, pat):
    """A search pattern as the words a person would give the index: regex or
    glob syntax, file extensions, language keywords and words under three
    letters dropped; underscores kept; [Dd]eliver read as Deliver."""
    s = pat
    if kind not in GLOB_KINDS:
        def one_letter(m):
            c = m.group(1)
            return c[0] if c.isalpha() and len(set(c.lower())) == 1 else " "
        s = re.sub(r"\[([^\]]*)\]", one_letter, s)   # character classes
        s = re.sub(r"\\[A-Za-z]", " ", s)            # \b \s \d \w
        s = re.sub(r"\{\d*,?\d*\}", " ", s)          # {2,5}
    s = re.sub(r"\\?\.(?:%s)\b" % EXTENSIONS, " ", s)
    words = []
    for w in re.findall(r"[A-Za-z0-9_]+", s):
        if len(w.strip("_")) >= 3 and not w.isdigit() and w.lower() not in KEYWORDS and w not in words:
            words.append(w)
    return words


def content_words(phrase):
    """A plain sentence as the words worth searching for. Case is kept (rg
    runs with -i; an identifier in the ask reaches the index as typed)."""
    toks = re.findall(r"[A-Za-z0-9_]+", phrase)
    words = [t for t in toks if len(t) >= 3 and t.lower() not in STOP and not t.isdigit()]
    out, seen = [], set()
    for w in words or toks:
        if w.lower() not in seen:
            seen.add(w.lower())
            out.append(w)
    return out


def bre_to_ere(pat):
    """grep's basic syntax as rg reads it: \\( \\) \\{ \\} \\? \\+ \\| swap
    meaning with their bare forms."""
    out, i = [], 0
    while i < len(pat):
        c = pat[i]
        if c == "\\" and i + 1 < len(pat):
            n = pat[i + 1]
            out.append(n if n in "(){}?+|" else c + n)
            i += 2
        elif c in "(){}?+|":
            out.append("\\" + c)
            i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


# ---------------------------------------------------------------- systems

class Runner:
    """Each search returns (top files, how many it found, seconds, how, the
    index's own milliseconds or None)."""

    WORD_CACHE_FILES = 4_000_000  # file entries kept across the word cache

    def __init__(self, paths, a):
        self.paths = paths
        self.timeout = a.timeout
        self.limit = a.limit
        self.index_bin = a.index_bin
        self.index_limit = a.index_limit
        self.index_args = a.index_arg
        self.max_words = a.max_words
        self.exclude = []  # canonical files and directories that never count as a hit
        self.word_cache = OrderedDict()
        self.word_cache_files = 0
        self.pattern_cache = {}
        self.index_cache = {}
        # rg, held to what the index reads.
        self.like_index = ["--max-filesize", MAX_FILESIZE]
        for d in SKIP_DIRS:
            self.like_index += ["-g", f"!**/{d}/"]

    def _run(self, argv, cwd=None):
        t0 = time.monotonic()
        try:
            p = subprocess.run(argv, cwd=cwd, capture_output=True, timeout=self.timeout)
            out, code = p.stdout.decode("utf-8", "replace"), p.returncode
            err = p.stderr.decode("utf-8", "replace")
        except subprocess.TimeoutExpired:
            out, code, err = "", 124, "timeout"
        except OSError as e:
            out, code, err = "", 127, str(e)
        return out, code, err, time.monotonic() - t0

    def _counts(self, flags, pat, root):
        """rg -c: {file: matching lines}, exit code, seconds."""
        out, code, _, dt = self._run(["rg", "-c", "--no-messages", *flags, "-e", pat, root])
        counts = {}
        for line in out.splitlines():
            f, _, n = line.rpartition(":")
            if f and n.isdigit():
                counts[f] = int(n)
        return counts, code, dt

    def _top(self, ranked):
        """Ranked paths, each mapped to its main checkout, first copy kept,
        excluded ones passed over before the list is cut."""
        seen, out = set(), []
        for f in ranked:
            c = self.paths.canon(f)
            if c in seen or any(inside(c, x) for x in self.exclude):
                continue
            seen.add(c)
            out.append(c)
            if len(out) >= self.limit:
                break
        return out

    def _rg_pattern(self, kind, pat, root, flags):
        """The agent's own pattern. A `grep:` pattern came from either a grep
        tool (rg syntax) or shell grep (basic syntax), so the baseline gets
        each reading in turn, then the fixed string, until one matches."""
        tries = [("regex", [], pat)]
        if kind != "rg" and bre_to_ere(pat) != pat:
            tries.append(("basic", [], bre_to_ere(pat)))
        tries.append(("fixed", ["-F"], pat))
        total, counts, mode = 0.0, {}, "regex"
        for mode, how, p in tries:
            counts, code, dt = self._counts([*flags, *how], p, root)
            total += dt
            if counts or code == 124:
                break
        ranked = sorted(counts, key=lambda f: (-counts[f], f))
        return self._top(ranked), len(counts), total, mode, None

    def _fd_pattern(self, kind, pat, root, like_index):
        argv = ["fd", "-t", "f"]
        if like_index:
            argv += ["--size", "-1mi"]
            for d in SKIP_DIRS:
                argv += ["--exclude", d]
        if kind != "fd":
            argv.append("--glob")
            if "/" in pat:
                argv.append("--full-path")
                if not pat.startswith(("*", "/")):
                    pat = "**/" + pat.lstrip("./")
        out, _, _, dt = self._run([*argv, "--", pat, root])
        files = out.splitlines()
        ranked = sorted(files, key=lambda f: (f.count("/"), f))
        return self._top(ranked), len(files), dt, "fd", None

    def rg_pattern(self, kind, pat, root, like_index):
        """In the project: as the agent ran it. Over the tree (like_index):
        skipping what the index skips, or dumps and bundles nobody would grep
        take the top."""
        key = (kind, pat, root, like_index)
        if key not in self.pattern_cache:
            if kind in GLOB_KINDS:
                self.pattern_cache[key] = self._fd_pattern(kind, pat, root, like_index)
            else:
                self.pattern_cache[key] = self._rg_pattern(kind, pat, root, self.like_index if like_index else [])
        return self.pattern_cache[key]

    def rg_words(self, words, root):
        """rg -i -w for each word, skipping what the index skips; files ranked
        by distinct words matched, then by matching lines. A word's result is
        reused, and its first cost is charged to every query that uses it."""
        per_file, total = {}, 0.0
        for w in words[:self.max_words]:
            key = (root, w.lower())
            if key in self.word_cache:
                self.word_cache.move_to_end(key)
            else:
                counts, _, dt = self._counts(["-i", "-F", "-w", *self.like_index], w, root)
                self.word_cache[key] = (counts, dt)
                self.word_cache_files += len(counts)
                while self.word_cache_files > self.WORD_CACHE_FILES and len(self.word_cache) > 1:
                    _, (old, _) = self.word_cache.popitem(last=False)
                    self.word_cache_files -= len(old)
            counts, dt = self.word_cache[key]
            total += dt
            for f, n in counts.items():
                e = per_file.setdefault(f, [0, 0])
                e[0] += 1
                e[1] += n
        ranked = sorted(per_file, key=lambda f: (-per_file[f][0], -per_file[f][1], f))
        return self._top(ranked), len(per_file), total, "words", None

    def index(self, words, cwd, under=None):
        """agent index search, run in cwd; with under, only hits inside that
        directory (what rg in the project is limited to). It is asked for
        more entities than files wanted, since several can share a file."""
        if not self.index_bin:
            return None
        if not os.path.isdir(cwd):
            cwd = self.paths.dev_root
        words = words[:self.max_words]
        key = (tuple(words), cwd, under)
        if key not in self.index_cache:
            argv = [self.index_bin, "-q", "index", "search", *words, "--json",
                    "--limit", str(self.index_limit), *self.index_args]
            if under:
                argv += ["--under", under]
            out, code, err, dt = self._run(argv, cwd=cwd)
            hits, own, mode = [], None, "index"
            if code != 0:
                mode = f"exit {code}: {err.strip()[:80]}"
            else:
                try:
                    doc = json.loads(out)
                    hits = index_hit_paths(doc)
                    if isinstance(doc, dict) and isinstance(doc.get("ms"), dict):
                        own = sum(v for v in doc["ms"].values() if isinstance(v, (int, float)))
                except ValueError:
                    mode = "bad json"
            self.index_cache[key] = (hits, dt, mode, own)
        hits, dt, mode, own = self.index_cache[key]
        return self._top(hits), len(hits), dt, mode, own


def index_hit_paths(doc):
    """Paths of a search reply's hits, best first: {"hits": [{"kind": ...,
    "path": ...}]}. A symbol or a section counts as its file. A directory or
    project hit is passed over: no file target can match it."""
    hits = doc.get("hits") if isinstance(doc, dict) else doc
    out = []
    for h in hits if isinstance(hits, list) else []:
        if isinstance(h, dict) and h.get("kind") not in NOT_FILES and os.path.isabs(h.get("path") or ""):
            out.append(h["path"])
    return out


# ---------------------------------------------------------------- query sets

def rank_of(top, targets):
    for i, f in enumerate(top):
        if f in targets:
            return i + 1
    return 0


def interleave(lists, limit):
    out, i = [], 0
    while len(out) < limit and any(i < len(l) for l in lists):
        for l in lists:
            if i < len(l) and l[i] not in out:
                out.append(l[i])
        i += 1
    return out[:limit]


def load_sessions(path, tag, paths, drops, seen, clusters, skip_cwd):
    """Rows of one sessions file. `seen` and `clusters` are shared across
    files: a session an earlier file holds is not read again."""
    rows = []
    with open(path, encoding="utf-8") as fh:
        raw = [json.loads(line) for line in fh if line.strip()]
    earlier = {s for s in seen if isinstance(s, str)}
    for n, r in enumerate(raw):
        # A pattern repeated straight after itself is one search, not two.
        searches = [s for i, s in enumerate(r.get("searches") or [])
                    if i == 0 or s != r["searches"][i - 1]]
        wanted = [os.path.join(r["cwd"], os.path.expanduser(p))
                  for p in (r.get("edits") or r.get("reads") or [])]
        if any(inside(r["cwd"], d) for d in skip_cwd) or "/.gomuxai/" in r["cwd"] + "/":
            drops["row: ran in a temp dir or in the agent tool's own data"] += 1
            continue
        if r.get("session") in earlier:
            drops["row: its session is in an earlier file"] += 1
            continue
        key = (r.get("session"), r["cwd"], tuple(searches)) if searches else \
            (r.get("session"), r["cwd"], r.get("ask"), tuple(wanted))
        if key in seen:
            drops["row: same session and searches as an earlier row"] += 1
            continue
        seen.add(key)
        if r.get("session"):
            seen.add(r["session"])
        targets = set()
        for e in wanted:
            c = paths.canon(e)
            if not paths.in_dev(c):
                drops["target: outside the dev root (memory, scratch, tool state)"] += 1
            elif not os.path.isfile(c):
                drops["target: file is not in its repo's main checkout now"] += 1
            else:
                targets.add(c)
        if not targets:
            drops["row: no target file left"] += 1
            continue
        scored, had = [], set()
        for s in searches:
            kind, _, pat = s.partition(":")
            if (kind, pat) in had:
                continue
            had.add((kind, pat))
            words = pattern_words(kind, pat)
            if WIDE_CONTEXT.search(pat):
                drops["pattern: wide-context read of a bundle (.{N}word.{N})"] += 1
            elif kind in GLOB_KINDS and TOOLING_DIRS & set(re.split(r"[/*]+", pat)):
                drops["pattern: names a build or VCS directory"] += 1
            elif not words:
                drops["pattern: no word in it (^#, *.go, pub fn)"] += 1
            else:
                scored.append((len(had), kind, pat, words))
        ask = content_words(r.get("ask") or "")
        if ask and (len(ask) < MIN_ASK_WORDS or TASK_FILE.search(r["ask"])):
            drops["ask: a greeting or a pointer to a task file, not a request"] += 1
            ask = []
        if not scored and not ask:
            drops["row: no usable pattern and no ask"] += 1
            continue
        project, known = paths.project_of(r["cwd"])
        cluster = clusters.setdefault(r.get("session") or f"{tag}{n}", len(clusters))
        rows.append({"id": f"{tag}{n:04d}", "cluster": f"s{cluster}", "opening": not r.get("episode"),
                     "cwd": paths.canon(r["cwd"]), "project": project,
                     "project_known": known, "scored": scored,
                     "elsewhere": known and not any(inside(t, project) for t in targets),
                     "ask": ask, "ask_text": r.get("ask") or "", "targets": targets,
                     "harness": r.get("harness") or "claude",
                     "target_kind": "edits" if r.get("edits") else "reads"})
    return rows


def load_lexicon(path, root, paths, drops):
    rows = []
    root = os.path.realpath(root)
    with open(path, encoding="utf-8") as fh:
        raw = [json.loads(line) for line in fh if line.strip()]
    for n, r in enumerate(raw):
        targets = set()
        for ref in r.get("provenance") or []:
            f = os.path.join(root, re.sub(r"(:\d+)+$", "", ref))
            if os.path.isfile(f):
                targets.add(paths.canon(f))
            else:
                drops["provenance: file is not in the repo now"] += 1
        if not targets:
            drops["term: no provenance file left"] += 1
            continue
        for k, phrase in enumerate(r.get("lay_phrasings") or []):
            words = content_words(phrase)
            if words:
                rows.append({"id": f"l{n:03d}.{k}", "cluster": f"l{n}", "phrase": phrase, "words": words,
                             "term": r.get("term", ""), "targets": targets})
    return rows, root


def load_questions(path, paths, drops):
    """Graded questions: {qid, query, stratum, rel: {PROJECT:PATH: grade}}.
    A relevant entity is a file in a project directly under the dev root."""
    rows = []
    with open(path, encoding="utf-8") as fh:
        raw = [json.loads(line) for line in fh if line.strip()]
    for r in raw:
        grades, projects = {}, Counter()
        for ent, grade in (r.get("rel") or {}).items():
            project, _, rel = ent.partition(":")
            f = os.path.join(paths.dev_root, project, rel.split("#")[0])
            if os.path.isfile(f):
                c = paths.canon(f)
                grades[c] = max(grades.get(c, 0), int(grade))
                projects[os.path.join(paths.dev_root, project)] += 1
            else:
                drops["relevant entity: not a file in the tree now"] += 1
        words = content_words(r.get("query") or "")
        if not grades or not words:
            drops["question: no relevant file left, or no words"] += 1
            continue
        named = os.path.join(paths.dev_root, str(r.get("project") or ""))
        rows.append({"id": str(r["qid"]), "query": r["query"], "words": words, "grades": grades,
                     "stratum": r.get("stratum", ""),
                     "project": named if r.get("project") and os.path.isdir(named)
                     else max(projects, key=lambda p: (projects[p], p))})
    return rows


# ---------------------------------------------------------------- scoring

class Table:
    def __init__(self, writer, limit):
        self.writer = writer
        self.limit = limit
        self.ranks = {}   # (set, system) -> {query id: rank, 0 = not found}
        self.times = {}   # (set, system) -> [wall milliseconds]
        self.own = {}     # (set, system) -> [the index's own milliseconds]
        self.turns = {}   # system -> {episode id: searches to reach a target, 0 = never}
        self.cluster = {}  # query id -> what is resampled: its session, its term, itself
        self.graded = {}  # (set, system) -> {query id: (nDCG@10, share of relevant files in top 5)}

    def add(self, qset, qid, system, query, res, targets, grades=None):
        """Scores one system's answer to one query. With grades ({file:
        grade}), also its nDCG at 10 and the share of relevant files in the
        top 5."""
        if res is None:
            return None
        top, found, dt, mode, own = res
        rank = rank_of(top, targets)
        if grades:
            dcg = sum((2 ** grades.get(f, 0) - 1) / math.log2(i + 2) for i, f in enumerate(top[:10]))
            ideal = sum((2 ** g - 1) / math.log2(i + 2)
                        for i, g in enumerate(sorted(grades.values(), reverse=True)[:10]))
            share = sum(1 for f in top[:5] if f in grades) / len(grades)
            self.graded.setdefault((qset, system), {})[qid] = (dcg / ideal if ideal else 0.0, share)
        self.put(qset, qid, system, query, rank, found, dt, mode, top[0] if top else "", targets, own)
        return rank

    def put(self, qset, qid, system, query, rank, found, dt, mode, first, targets, own=None):
        self.ranks.setdefault((qset, system), {})[qid] = rank
        self.times.setdefault((qset, system), []).append(dt * 1000)
        if own is not None:
            self.own.setdefault((qset, system), []).append(own)
        self.writer.writerow([qset, qid, system, query, mode, rank, found, f"{dt * 1000:.0f}",
                              "" if own is None else f"{own:.1f}", first, " ".join(sorted(targets))])

    def _ms(self, key):
        times = sorted(self.times[key])
        own = sorted(self.own.get(key) or [])
        return f"{times[len(times) // 2]:.0f}", f"{own[len(own) // 2]:.0f}" if own else "-"

    def summary(self):
        lines = ["| set | system | n | recall@1 | recall@5 | recall@10 | MRR | median ms, whole command | median ms, inside the index |",
                 "|---|---|---|---|---|---|---|---|---|"]
        for key in sorted(self.ranks):
            ranks = list(self.ranks[key].values())
            n = len(ranks)

            def at(k):
                c = sum(1 for r in ranks if 0 < r <= k)
                return f"{c / n:.2f} ({c})"

            mrr = sum(1 / r for r in ranks if r) / n
            wall, own = self._ms(key)
            lines.append(f"| {key[0]} | {key[1]} | {n} | {at(1)} | {at(5)} | {at(10)} | {mrr:.3f} | "
                         f"{wall} | {own} |")
        return "\n".join(lines)

    def graded_summary(self):
        lines = ["| set | system | n | nDCG@10 | MRR@10 | a relevant file in top 5 | share of relevant files in top 5 | median ms, whole command | median ms, inside the index |",
                 "|---|---|---|---|---|---|---|---|---|"]
        for key in sorted(self.graded):
            g = self.graded[key]
            ranks = [self.ranks[key][q] for q in g]
            n = len(g)
            c5 = sum(1 for r in ranks if 0 < r <= 5)
            wall, own = self._ms(key)
            lines.append(f"| {key[0]} | {key[1]} | {n} | {sum(v[0] for v in g.values()) / n:.3f} | "
                         f"{sum(1 / r for r in ranks if r) / n:.3f} | {c5 / n:.2f} ({c5}) | "
                         f"{sum(v[1] for v in g.values()) / n:.2f} | {wall} | {own} |")
        return "\n".join(lines)

    def compare(self, a, b, resamples, seed):
        """Paired bootstrap of a minus b over the queries both answered:
        (n, clusters, recall@5 difference, its 95% interval, MRR difference,
        its interval, nDCG@10 difference, its interval; the last three are 0
        without grades). Whole sessions (for the lexicon, whole terms; for
        questions, each question) are resampled, since one session's queries
        aren't independent."""
        ra, rb = self.ranks.get(a), self.ranks.get(b)
        if not ra or not rb:
            return None
        ids = sorted(set(ra) & set(rb))
        if not ids:
            return None
        ga, gb = self.graded.get(a, {}), self.graded.get(b, {})
        groups = {}
        for q in ids:
            g = groups.setdefault(self.cluster.get(q.split("#")[0], q), [0, 0.0, 0, 0.0])
            g[0] += (0 < ra[q] <= 5) - (0 < rb[q] <= 5)
            g[1] += (1 / ra[q] if ra[q] else 0) - (1 / rb[q] if rb[q] else 0)
            g[2] += 1
            g[3] += ga.get(q, (0.0,))[0] - gb.get(q, (0.0,))[0]
        cells = list(groups.values())
        n = len(ids)
        rng = random.Random(seed)
        d5, dm, dn = [], [], []
        for _ in range(resamples):
            pick = rng.choices(cells, k=len(cells))
            size = sum(c[2] for c in pick)
            d5.append(sum(c[0] for c in pick) / size)
            dm.append(sum(c[1] for c in pick) / size)
            dn.append(sum(c[3] for c in pick) / size)
        d5.sort()
        dm.sort()
        dn.sort()
        lo, hi = int(0.025 * resamples), min(resamples - 1, int(0.975 * resamples))
        return (n, len(cells), sum(c[0] for c in cells) / n, d5[lo], d5[hi],
                sum(c[1] for c in cells) / n, dm[lo], dm[hi],
                sum(c[3] for c in cells) / n, dn[lo], dn[hi])

    def turns_table(self):
        lines = ["| system | episodes | 1 | 2 | 3 | 4 | 5+ | never |",
                 "|---|---|---|---|---|---|---|---|"]
        for system in sorted(self.turns):
            t = list(self.turns[system].values())
            cells = [sum(1 for x in t if x == k) for k in (1, 2, 3, 4)]
            cells.append(sum(1 for x in t if x >= 5))
            lines.append(f"| {system} | {len(t)} | " + " | ".join(str(c) for c in cells)
                         + f" | {sum(1 for x in t if not x)} |")
        lines += ["", "On the episodes both of a pair reached, the mean number of searches each took:", "",
                  "| pair | episodes both reached | index | rg |", "|---|---|---|---|"]
        for ia, ib in TURN_PAIRS:
            ta, tb = self.turns.get(ia), self.turns.get(ib)
            if not ta or not tb:
                continue
            both = [q for q in ta if ta[q] and tb.get(q)]
            if both:
                lines.append(f"| {ia} and {ib} | {len(both)} | {sum(ta[q] for q in both) / len(both):.2f} | "
                             f"{sum(tb[q] for q in both) / len(both):.2f} |")
            else:
                lines.append(f"| {ia} and {ib} | 0 | - | - |")
        return "\n".join(lines)


TURN_PAIRS = [("index@project", "rg_patterns@project"), ("index@cwd", "rg_patterns@project"),
              ("index@root", "rg_patterns@tree")]

COMPARISONS = [
    # (what it says, set and system of the index, set and system of the baseline)
    ("each agent pattern: index limited to the project vs rg in the project", ("pattern", "index_patterns@project"), ("pattern", "rg_patterns@project")),
    ("each agent pattern: index with its own project prior vs rg in the project", ("pattern", "index_patterns@cwd"), ("pattern", "rg_patterns@project")),
    ("each agent pattern: index vs rg, whole tree", ("pattern", "index_patterns@root"), ("pattern", "rg_patterns@tree")),
    ("an episode's patterns as one list: index limited to the project vs rg in the project", ("episode", "index_patterns@project"), ("episode", "rg_patterns@project")),
    ("an episode's patterns as one list: index with its own project prior vs rg in the project", ("episode", "index_patterns@cwd"), ("episode", "rg_patterns@project")),
    ("an episode's patterns as one list: index vs rg, whole tree", ("episode", "index_patterns@root"), ("episode", "rg_patterns@tree")),
    ("lay phrasing (skewed set): index limited to the project vs rg on plain words", ("lexicon", "index_words@project"), ("lexicon", "rg_words@project")),
    ("lay phrasing (skewed set): index with its own project prior vs rg on plain words in the project", ("lexicon", "index_words@cwd"), ("lexicon", "rg_words@project")),
    ("lay phrasing (skewed set): index vs rg on plain words, whole tree", ("lexicon", "index_words@root"), ("lexicon", "rg_words@tree")),
    ("lay phrasing (skewed set): index limited to the project vs the most-cited files", ("lexicon", "index_words@project"), ("lexicon", "most_cited_files")),
    ("lay phrasing, the two most-cited files left out: index limited to the project vs rg on plain words", ("lexicon_rest", "index_words@project"), ("lexicon_rest", "rg_words@project")),
    ("lay phrasing, the two most-cited files left out: index with its own project prior vs rg on plain words in the project", ("lexicon_rest", "index_words@cwd"), ("lexicon_rest", "rg_words@project")),
    ("lay phrasing, the two most-cited files left out: index vs rg on plain words, whole tree", ("lexicon_rest", "index_words@root"), ("lexicon_rest", "rg_words@tree")),
    ("opening ask (noisy, few): index limited to the project vs rg on plain words", ("ask_opening", "index_words@project"), ("ask_opening", "rg_words@project")),
    ("opening ask (noisy, few): index vs rg on plain words, whole tree", ("ask_opening", "index_words@root"), ("ask_opening", "rg_words@tree")),
    ("footnote, every ask: index limited to the project vs rg on plain words", ("ask_all", "index_words@project"), ("ask_all", "rg_words@project")),
    ("footnote, every ask: index vs rg on plain words, whole tree", ("ask_all", "index_words@root"), ("ask_all", "rg_words@tree")),
]

GRADED_COMPARISONS = [
    # (what it says, the first system, the second), per question set
    ("index limited to the project vs rg on plain words in the project", "index_words@project", "rg_words@project"),
    ("index with its own project prior vs rg on plain words in the project", "index_words@cwd", "rg_words@project"),
    ("index on the question as typed, limited to the project, vs rg on plain words in the project", "index_sentence@project", "rg_words@project"),
    ("index vs rg on plain words, whole tree", "index_words@root", "rg_words@tree"),
    ("index on the question as typed vs rg on plain words, whole tree", "index_sentence@root", "rg_words@tree"),
    ("index limited to the project vs the most-cited files", "index_words@project", "most_cited_files"),
    ("rg on plain words in the project vs the most-cited files", "rg_words@project", "most_cited_files"),
]


def most_cited(rows, key, limit):
    """A reference that reads no query: the files a set cites most, the same
    list for every query. A system that can't beat it has learned only which
    files are popular."""
    cited = Counter()
    for q in rows:
        for t in q[key]:
            cited[t] += 1
    return sorted(cited, key=lambda t: (-cited[t], t))[:limit], len(cited)


def run_questions(name, rows, run, table, dev, limit):
    """rg on plain words against the index, on questions with graded answers.
    The index is asked twice: with the content words rg gets, and with the
    question as a person would type it."""
    popular, cited = most_cited(rows, "grades", limit)
    strata = {q["stratum"] for q in rows}
    for q in rows:
        words, sentence, root = q["words"], q["query"].split(), q["project"]
        table.cluster[q["id"]] = q["id"]
        # The whole set, and (when the file has several) the question's stratum.
        sets = [name, f"{name}:{q['stratum']}"] if len(strata) > 1 else [name]
        for system, res in (
                ("most_cited_files", (popular, cited, 0.0, "constant", None)),
                ("rg_words@project", run.rg_words(words, root)),
                ("rg_words@tree", run.rg_words(words, dev)),
                ("index_words@project", run.index(words, root, root)),
                ("index_words@cwd", run.index(words, root)),
                ("index_words@root", run.index(words, dev)),
                ("index_sentence@project", run.index(sentence, root, root)),
                ("index_sentence@cwd", run.index(sentence, root)),
                ("index_sentence@root", run.index(sentence, dev))):
            for qset in sets:
                table.add(qset, q["id"], system, q["query"], res, set(q["grades"]), grades=q["grades"])
        sys.stderr.write(".")
        sys.stderr.flush()


def run_session(s, run, table, dev, limit, tree=True):
    targets, sid = s["targets"], s["id"]
    table.cluster[sid] = s["cluster"]
    # (rg's name, where rg looks, held to what the index reads,
    #  [(the index's name, its cwd, its --under)])
    scopes = []
    if s["project_known"]:
        scopes.append(("project", s["project"], False,
                       [("project", s["cwd"], s["project"]), ("cwd", s["cwd"], None)]))
    if tree:
        scopes.append(("tree", dev, True, [("root", dev, None)]))
    shown = " | ".join(f"{k}:{p}" for _, k, p, _ in s["scored"])
    for scope, root, like_index, variants in scopes:
        # The plain ask.
        if s["ask"]:
            for qset in ("ask_all", "ask_opening") if s["opening"] else ("ask_all",):
                table.add(qset, sid, f"rg_words@{scope}", s["ask_text"], run.rg_words(s["ask"], root), targets)
                for name, icwd, under in variants:
                    table.add(qset, sid, f"index_words@{name}", " ".join(s["ask"]),
                              run.index(s["ask"], icwd, under), targets)
        if not s["scored"]:
            continue
        # Each pattern the agent ran; then the episode's patterns as one list,
        # the per-pattern lists interleaved, for rg and the index alike.
        systems = [(f"rg_patterns@{scope}", lambda kind, pat, words: run.rg_pattern(kind, pat, root, like_index))]
        for name, icwd, under in variants:
            if run.index_bin:
                systems.append((f"index_patterns@{name}",
                                lambda kind, pat, words, c=icwd, u=under: run.index(words, c, u)))
        for system, search in systems:
            results = []
            for pos, kind, pat, words in s["scored"]:
                res = search(kind, pat, words)
                results.append(res)
                query = f"{kind}:{pat}" if system.startswith("rg") else " ".join(words)
                table.add("pattern", f"{sid}#{pos}", system, query, res, targets)
            dt = sum(r[2] for r in results)
            own = [r[4] for r in results if r[4] is not None]
            merged = interleave([r[0] for r in results], limit)
            table.add("episode", sid, system, shown, (merged, 0, dt, "interleaved", sum(own) if own else None),
                      targets)
            # Searches to reach a target: the same patterns, in the agent's
            # order, for both.
            hits = [i + 1 for i, r in enumerate(results) if rank_of(r[0], targets)]
            table.turns.setdefault(system.replace("index_patterns", "index"), {})[sid] = hits[0] if hits else 0
            if system.startswith("rg"):
                ranks = [rank_of(r[0], targets) for r in results]
                table.put("episode", sid, system.replace("rg_patterns", "rg_patterns_best"), shown,
                          min([r for r in ranks if r], default=0), "", dt, "best pattern", "", targets)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dev-root", required=True, help="the tree the index crawled")
    ap.add_argument("--qrels", action="append", default=[],
                    help="graded questions JSONL (repeatable): {qid, query, stratum, rel: {PROJECT:PATH: grade}}")
    ap.add_argument("--evalset", action="append", default=[], help="sessions JSONL (repeatable)")
    ap.add_argument("--lexicon", help="lexicon JSONL")
    ap.add_argument("--lexicon-root", help="the repo the lexicon's provenance paths are relative to")
    ap.add_argument("--exclude-hit", action="append", default=[],
                    help="lexicon: a file or directory that holds the answer key (the lexicon's sources); "
                         "hits in it never count and take no rank (repeatable; the lexicon file always)")
    ap.add_argument("--index-bin", help="agent binary with a crawled index; without it, baselines only")
    ap.add_argument("--index-limit", type=int, default=50,
                    help="entities asked of index search; they are cut to --limit files")
    ap.add_argument("--index-arg", action="append", default=[],
                    help="an extra argument for index search, e.g. --index-arg=--kind --index-arg=file")
    ap.add_argument("--no-tree", action="store_true",
                    help="sessions and lexicon: skip the whole-tree systems (the slow part), for a quick rerun")
    ap.add_argument("--out", default="eval-out", help="directory for queries.csv and summary.md")
    ap.add_argument("--limit", type=int, default=10, help="files kept per query")
    ap.add_argument("--max-words", type=int, default=12, help="words of a plain query given to either system")
    ap.add_argument("--skip-cwd", action="append", default=["/tmp", "/run"],
                    help="drop rows that ran under this directory (repeatable; /tmp and /run always)")
    ap.add_argument("--timeout", type=float, default=120, help="seconds per command")
    ap.add_argument("--resamples", type=int, default=10000, help="bootstrap resamples")
    ap.add_argument("--seed", type=int, default=20261123, help="bootstrap seed")
    ap.add_argument("--max-queries", type=int, default=0, help="per set, for a quick trial")
    a = ap.parse_args()

    paths = Paths(a.dev_root)
    run = Runner(paths, a)
    os.makedirs(a.out, exist_ok=True)
    fh = open(os.path.join(a.out, "queries.csv"), "w", newline="", encoding="utf-8")
    w = csv.writer(fh)
    w.writerow(["set", "id", "system", "query", "mode", "rank", "found", "ms", "index_ms", "top", "targets"])
    table = Table(w, a.limit)
    dev = paths.dev_root
    notes = []
    seen, clusters = set(), {}

    for path in a.qrels:
        drops = Counter()
        name = os.path.splitext(os.path.basename(path))[0]
        questions = load_questions(path, paths, drops)
        if a.max_queries:
            questions = questions[:a.max_queries]
        notes.append(f"questions {name}: {len(questions)} kept "
                     f"(by stratum {dict(Counter(q['stratum'] for q in questions))}). Dropped:")
        notes += [f"  {c:4d}  {why}" for why, c in sorted(drops.items())] or ["     0"]
        run_questions(name, questions, run, table, dev, a.limit)
        fh.flush()
    if a.qrels and (a.evalset or a.lexicon):
        # The questions' numbers, before the slower parts.
        report(table, notes, a, "summary-questions.md")

    for i, path in enumerate(a.evalset):
        drops = Counter()
        sessions = load_sessions(path, "abcdefgh"[i % 8], paths, drops, seen, clusters, a.skip_cwd)
        if a.max_queries:
            sessions = sessions[:a.max_queries]
        notes.append(f"sessions file {i + 1}: {len(sessions)} rows kept from "
                     f"{len({s['cluster'] for s in sessions})} sessions "
                     f"({sum(1 for s in sessions if s['ask'])} with an ask, "
                     f"{sum(1 for s in sessions if s['ask'] and s['opening'])} of them opening a session; "
                     f"{sum(len(s['scored']) for s in sessions)} patterns scored; "
                     f"project unknown for {sum(1 for s in sessions if not s['project_known'])}, "
                     f"which are scored over the tree only; "
                     f"{sum(1 for s in sessions if s['elsewhere'])} have every target outside the "
                     f"session's project, where nothing limited to the project can reach; "
                     f"by harness {dict(Counter(s['harness'] for s in sessions))}; "
                     f"targets {dict(Counter(s['target_kind'] for s in sessions))}). Dropped:")
        notes += [f"  {c:4d}  {why}" for why, c in sorted(drops.items())] or ["     0"]
        for s in sessions:
            run_session(s, run, table, dev, a.limit, not a.no_tree)
            fh.flush()
            sys.stderr.write(".")
            sys.stderr.flush()

    if a.evalset and a.lexicon:
        # The sessions' numbers, before the slower lexicon part.
        report(table, notes, a, "summary-sessions.md")

    if a.lexicon:
        if not a.lexicon_root:
            ap.error("--lexicon needs --lexicon-root")
        drops = Counter()
        lex, root = load_lexicon(a.lexicon, a.lexicon_root, paths, drops)
        if a.max_queries:
            lex = lex[:a.max_queries]
        notes.append(f"lexicon: {len(lex)} lay phrasings kept. Dropped:")
        notes += [f"  {c:4d}  {why}" for why, c in sorted(drops.items())] or ["     0"]
        # The lexicon file and its sources hold every phrasing beside its
        # term: a hit on them is the answer key, for any system, so it never
        # counts and takes no rank.
        run.exclude = [paths.canon(os.path.realpath(p)) for p in [a.lexicon, *a.exclude_hit]]
        popular, cited = most_cited(lex, "targets", a.limit)
        top2 = set(popular[:2])
        notes.append(f"  lexicon_rest: the {sum(1 for q in lex if not q['targets'] & top2)} phrasings whose "
                     f"targets are neither of the two most-cited files "
                     f"({', '.join(os.path.basename(p) for p in popular[:2])})")
        for q in lex:
            table.cluster[q["id"]] = q["cluster"]
            systems = [("most_cited_files", (popular, cited, 0.0, "constant", None)),
                       ("rg_words@project", run.rg_words(q["words"], root)),
                       ("index_words@project", run.index(q["words"], root, root)),
                       ("index_words@cwd", run.index(q["words"], root))]
            if not a.no_tree:
                systems += [("rg_words@tree", run.rg_words(q["words"], dev)),
                            ("index_words@root", run.index(q["words"], dev))]
            for qset in ("lexicon",) if q["targets"] & top2 else ("lexicon", "lexicon_rest"):
                for name, res in systems:
                    table.add(qset, q["id"], name, q["phrase"], res, q["targets"])
            fh.flush()
            sys.stderr.write(".")
            sys.stderr.flush()
        run.exclude = []

    fh.close()
    sys.stderr.write("\n")
    print(report(table, notes, a, "summary.md"))


def report(table, notes, a, name):
    """The summary so far, written to OUT/name and returned."""
    out = []
    if table.graded:
        rows = []
        for qset in sorted({k[0] for k in table.graded}):
            for label, ia, ib in GRADED_COMPARISONS:
                c = table.compare((qset, ia), (qset, ib), a.resamples, a.seed)
                if c:
                    rows.append(f"| {qset} | {label} | {c[0]} | {c[8]:+.3f} [{c[9]:+.3f}, {c[10]:+.3f}] | "
                                f"{c[5]:+.3f} [{c[6]:+.3f}, {c[7]:+.3f}] | {c[2]:+.3f} [{c[3]:+.3f}, {c[4]:+.3f}] |")
        out += ["## Plain questions with graded answers", "", table.graded_summary(), "",
                f"First minus second, paired by question (bootstrap, {a.resamples} resamples, 95% interval):", "",
                "| set | comparison | questions | nDCG@10 difference | MRR@10 difference | recall@5 difference |",
                "|---|---|---|---|---|---|", *rows, ""]
    out += ["## Recall and time", "", table.summary(), ""]
    rows = []
    for label, ia, ib in COMPARISONS:
        c = table.compare(ia, ib, a.resamples, a.seed)
        if c:
            rows.append(f"| {label} | {c[0]} | {c[1]} | {c[2]:+.3f} [{c[3]:+.3f}, {c[4]:+.3f}] | "
                        f"{c[5]:+.3f} [{c[6]:+.3f}, {c[7]:+.3f}] |")
    if rows:
        out += [f"## Index minus baseline (paired bootstrap, {a.resamples} resamples, 95% interval)", "",
                "Whole sessions (for the lexicon, whole terms) are resampled.", "",
                "| comparison | queries | resampled units | recall@5 difference | MRR difference |",
                "|---|---|---|---|---|", *rows, ""]
    if table.turns:
        out += ["## Searches to reach a target file (top 10), the agent's patterns in its order", "",
                table.turns_table(), ""]
    out += ["## Rows", "", *notes]
    text = "\n".join(out) + "\n"
    with open(os.path.join(a.out, name), "w", encoding="utf-8") as sf:
        sf.write(text)
    return text


if __name__ == "__main__":
    main()
