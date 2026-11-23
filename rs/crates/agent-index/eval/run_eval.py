#!/usr/bin/env python3
"""Score `agent index search` against what an agent does with rg and fd.

Query sets (data that stays out of the repo):

  sessions  past sessions as {session, cwd, searches, edits} and optionally
            {episode, ask, reads, harness}, one row per episode: what the
            person asked in plain words, the patterns the agent then searched
            for, and the files it edited (or, with no edits, read). A query is
            right when a target file is in the top k.
  lexicon   a glossary as {lay_phrasings, provenance}: each lay phrasing is a
            query, and the files its provenance names are the targets.

Two kinds of query, each given to rg and to the index alike:

  plain words        the ask, or a lay phrasing, as its content words
  the agent's own    the patterns the agent really ran: it had already turned
  patterns           the ask into code words

Systems (the part after @ is where it ran):

  rg_words@project, @tree       rg -i on each plain word, files ranked by
                                distinct words matched, then matching lines
  rg_patterns@project, @tree    the agent's pattern with rg (fd for a glob),
                                files ranked by matching lines
  index_words@cwd, @root        agent index search WORDS --json --limit K
  index_patterns@cwd, @root     the same, on a pattern's words

@project is the session's repo and @cwd its directory (so the index's project
prior can apply); @tree and @root are the whole dev root, no project known.

Stdlib only; run with `python3 -I run_eval.py --help`. README.md beside this
file says what each choice does to fairness.
"""
import argparse
import csv
import json
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
# Directories a find/fd pattern names when the search is about the build or
# the VCS, not about the code.
TOOLING_DIRS = {".git", "target", "node_modules"}
WIDE_CONTEXT = re.compile(r"\.\{\d{3,}\}")  # .{240}word.{600}: reading a bundle
GLOB_KINDS = ("find", "fd", "glob")
MIN_ASK_WORDS = 5  # fewer content words than this is a greeting
TASK_FILE = re.compile(r"\btask\.md\b", re.I)  # "your task is in .task.md"


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


# ---------------------------------------------------------------- words

def pattern_words(kind, pat):
    """A search pattern stripped of regex or glob syntax into words."""
    s = pat
    if kind not in GLOB_KINDS:
        s = re.sub(r"\[[^\]]*\]", " ", s)          # character classes
        s = re.sub(r"\\[A-Za-z]", " ", s)           # \b \s \d \w
        s = re.sub(r"\{\d*,?\d*\}", " ", s)         # {2,5}
    words = []
    for w in re.findall(r"[A-Za-z0-9_]+", s):
        w = w.strip("_")
        if len(w) >= 2 and not w.isdigit() and w not in words:
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
    """Each method returns (top files, how many it found, seconds, how)."""

    WORD_CACHE_FILES = 4_000_000  # file entries kept across the word cache

    def __init__(self, paths, timeout, limit, index_bin, index_limit, max_words):
        self.paths = paths
        self.timeout = timeout
        self.limit = limit
        self.index_bin = index_bin
        self.index_limit = index_limit
        self.max_words = max_words
        self.word_cache = OrderedDict()
        self.word_cache_files = 0
        self.pattern_cache = {}
        self.index_cache = {}

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
        """Ranked paths, each mapped to its main checkout, first copy kept."""
        seen, out = set(), []
        for f in ranked:
            c = self.paths.canon(f)
            if c not in seen:
                seen.add(c)
                out.append(c)
                if len(out) >= self.limit:
                    break
        return out

    def _rg_pattern(self, kind, pat, root):
        """The agent's own pattern. A `grep:` pattern came from either a grep
        tool (rg syntax) or shell grep (basic syntax), so the baseline gets
        each reading in turn, then the fixed string, until one matches."""
        tries = [("regex", [], pat)]
        if kind != "rg" and bre_to_ere(pat) != pat:
            tries.append(("basic", [], bre_to_ere(pat)))
        tries.append(("fixed", ["-F"], pat))
        total, counts, mode = 0.0, {}, "regex"
        for mode, flags, p in tries:
            counts, code, dt = self._counts(flags, p, root)
            total += dt
            if counts or code == 124:
                break
        ranked = sorted(counts, key=lambda f: (-counts[f], f))
        return self._top(ranked), len(counts), total, mode

    def _fd_pattern(self, kind, pat, root):
        argv = ["fd", "-t", "f"]
        if kind != "fd":
            argv.append("--glob")
            if "/" in pat:
                argv.append("--full-path")
                if not pat.startswith(("*", "/")):
                    pat = "**/" + pat.lstrip("./")
        out, _, _, dt = self._run([*argv, "--", pat, root])
        files = out.splitlines()
        ranked = sorted(files, key=lambda f: (f.count("/"), f))
        return self._top(ranked), len(files), dt, "fd"

    def rg_pattern(self, kind, pat, root):
        key = (kind, pat, root)
        if key not in self.pattern_cache:
            fn = self._fd_pattern if kind in GLOB_KINDS else self._rg_pattern
            self.pattern_cache[key] = fn(kind, pat, root)
        return self.pattern_cache[key]

    def rg_words(self, words, root):
        """rg -i for each word; files ranked by distinct words matched, then
        by matching lines. A word's result is reused, and its first cost is
        charged to every query that uses it."""
        per_file, total = {}, 0.0
        for w in words[:self.max_words]:
            key = (root, w.lower())
            if key in self.word_cache:
                self.word_cache.move_to_end(key)
            else:
                counts, _, dt = self._counts(["-i", "-F"], w, root)
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
        return self._top(ranked), len(per_file), total, "words"

    def index(self, words, cwd):
        if not self.index_bin:
            return None
        if not os.path.isdir(cwd):
            cwd = self.paths.dev_root
        words = words[:self.max_words]
        key = (tuple(words), cwd)
        if key in self.index_cache:
            return self.index_cache[key]
        argv = [self.index_bin, "-q", "index", "search", *words, "--json", "--limit", str(self.index_limit)]
        out, code, err, dt = self._run(argv, cwd=cwd)
        if code != 0:
            res = ([], 0, dt, f"exit {code}: {err.strip()[:80]}")
        else:
            try:
                hits = index_hit_paths(json.loads(out))
                res = (self._top(hits), len(hits), dt, "index")
            except ValueError:
                res = ([], 0, dt, "bad json")
        self.index_cache[key] = res
        return res


def index_hit_paths(doc):
    """Paths of a search reply's hits, best first: {"hits": [{"path": ...}]}.
    A symbol or a section counts as its file. A directory or project hit
    keeps its rank and matches no target."""
    hits = doc
    if isinstance(doc, dict):
        for k in ("hits", "results", "entities", "matches"):
            if isinstance(doc.get(k), list):
                hits = doc[k]
                break
    out = []
    for h in hits if isinstance(hits, list) else []:
        if not isinstance(h, dict):
            continue
        ent = h.get("entity") if isinstance(h.get("entity"), dict) else h
        p = ent.get("abs_path") or ent.get("file") or ent.get("path")
        if not p:
            continue
        if not os.path.isabs(p):
            root = ent.get("root") or ent.get("project_root") or ent.get("project_path")
            if not root:
                continue
            p = os.path.join(root, p)
        out.append(p)
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
    files: a session mined twice is scored once and resampled as one."""
    rows = []
    with open(path, encoding="utf-8") as fh:
        raw = [json.loads(line) for line in fh if line.strip()]
    for n, r in enumerate(raw):
        # A pattern repeated straight after itself is one search, not two.
        searches = [s for i, s in enumerate(r.get("searches") or [])
                    if i == 0 or s != r["searches"][i - 1]]
        wanted = [os.path.join(r["cwd"], os.path.expanduser(p))
                  for p in (r.get("edits") or r.get("reads") or [])]
        if any(r["cwd"] == d or r["cwd"].startswith(d.rstrip("/") + "/") for d in skip_cwd) \
                or "/.gomuxai/" in r["cwd"] + "/":
            drops["row: ran in a temp dir or in the agent tool's own data"] += 1
            continue
        key = (r.get("session"), r["cwd"], tuple(searches)) if searches else \
            (r.get("session"), r["cwd"], r.get("ask"), tuple(wanted))
        if key in seen:
            drops["row: same session and searches as an earlier row"] += 1
            continue
        seen.add(key)
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
        sequence, scored = [], []
        for s in searches:
            kind, _, pat = s.partition(":")
            words = pattern_words(kind, pat)
            usable = any(len(w) >= 3 for w in words)
            if (kind, pat) in [(k, p) for k, p, _, _ in sequence]:
                sequence.append((kind, pat, words, usable))
                continue
            sequence.append((kind, pat, words, usable))
            if WIDE_CONTEXT.search(pat):
                drops["pattern: wide-context read of a bundle (.{N}word.{N})"] += 1
            elif kind in GLOB_KINDS and TOOLING_DIRS & set(re.split(r"[/*]+", pat)):
                drops["pattern: names a build or VCS directory"] += 1
            elif not usable:
                drops["pattern: no word in it (^#, ^$, *.go)"] += 1
            else:
                scored.append((len(sequence), kind, pat, words))
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
                     "project_known": known, "sequence": sequence, "scored": scored,
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
            f = os.path.join(root, ref.rsplit(":", 1)[0])
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


# ---------------------------------------------------------------- scoring

class Table:
    def __init__(self, writer, limit):
        self.writer = writer
        self.limit = limit
        self.ranks = {}   # (set, system) -> {query id: rank, 0 = not found}
        self.times = {}
        self.turns = {}   # system -> [searches to reach a target, 0 = never]
        self.cluster = {}  # query id -> what is resampled: its session, or its term

    def add(self, qset, qid, system, query, res, targets, exclude=()):
        if res is None:
            return None
        top, found, dt, mode = res
        top = [f for f in top if f not in exclude]
        rank = rank_of(top, targets)
        self.put(qset, qid, system, query, rank, found, dt, mode, top[0] if top else "", targets)
        return rank

    def put(self, qset, qid, system, query, rank, found, dt, mode, first, targets):
        self.ranks.setdefault((qset, system), {})[qid] = rank
        self.times.setdefault((qset, system), []).append(dt * 1000)
        self.writer.writerow([qset, qid, system, query, mode, rank, found, f"{dt * 1000:.0f}",
                              first, " ".join(sorted(targets))])

    def summary(self):
        lines = ["| set | system | n | recall@1 | recall@5 | recall@10 | MRR | median ms | mean ms |",
                 "|---|---|---|---|---|---|---|---|---|"]
        for key in sorted(self.ranks):
            ranks = list(self.ranks[key].values())
            times = sorted(self.times[key])
            n = len(ranks)

            def at(k):
                c = sum(1 for r in ranks if 0 < r <= k)
                return f"{c / n:.2f} ({c})"

            mrr = sum(1 / r for r in ranks if r) / n
            lines.append(f"| {key[0]} | {key[1]} | {n} | {at(1)} | {at(5)} | {at(10)} | {mrr:.3f} | "
                         f"{times[len(times) // 2]:.0f} | {sum(times) / len(times):.0f} |")
        return "\n".join(lines)

    def compare(self, a, b, resamples, seed):
        """Paired bootstrap of a minus b over the queries both answered:
        (n, clusters, recall@5 difference, its 95% interval, MRR difference,
        its interval). Whole sessions (for the lexicon, whole terms) are
        resampled, since one session's queries aren't independent."""
        ra, rb = self.ranks.get(a), self.ranks.get(b)
        if not ra or not rb:
            return None
        ids = sorted(set(ra) & set(rb))
        if not ids:
            return None
        groups = {}
        for q in ids:
            g = groups.setdefault(self.cluster.get(q.split("#")[0], q), [0, 0.0, 0])
            g[0] += (0 < ra[q] <= 5) - (0 < rb[q] <= 5)
            g[1] += (1 / ra[q] if ra[q] else 0) - (1 / rb[q] if rb[q] else 0)
            g[2] += 1
        cells = list(groups.values())
        n = len(ids)
        rng = random.Random(seed)
        d5, dm = [], []
        for _ in range(resamples):
            pick = rng.choices(cells, k=len(cells))
            size = sum(c[2] for c in pick)
            d5.append(sum(c[0] for c in pick) / size)
            dm.append(sum(c[1] for c in pick) / size)
        d5.sort()
        dm.sort()
        lo, hi = int(0.025 * resamples), min(resamples - 1, int(0.975 * resamples))
        return (n, len(cells), sum(c[0] for c in cells) / n, d5[lo], d5[hi],
                sum(c[1] for c in cells) / n, dm[lo], dm[hi])

    def turns_table(self):
        lines = ["| system | episodes | 1 | 2 | 3 | 4 | 5+ | never | mean when found |",
                 "|---|---|---|---|---|---|---|---|---|"]
        for system in sorted(self.turns):
            t = self.turns[system]
            found = [x for x in t if x]
            cells = [sum(1 for x in t if x == k) for k in (1, 2, 3, 4)]
            cells.append(sum(1 for x in t if x >= 5))
            mean = f"{sum(found) / len(found):.2f}" if found else "-"
            lines.append(f"| {system} | {len(t)} | " + " | ".join(str(c) for c in cells)
                         + f" | {len(t) - len(found)} | {mean} |")
        return "\n".join(lines)


COMPARISONS = [
    # (what it says, set and system of the index, set and system of the baseline)
    ("lay phrasing: index vs rg on plain words, project known", ("lexicon", "index_words@cwd"), ("lexicon", "rg_words@project")),
    ("lay phrasing: index vs rg on plain words, whole tree", ("lexicon", "index_words@root"), ("lexicon", "rg_words@tree")),
    ("opening ask (noisy): index vs rg on plain words, project known", ("ask_opening", "index_words@cwd"), ("ask_opening", "rg_words@project")),
    ("opening ask (noisy): index vs rg on plain words, whole tree", ("ask_opening", "index_words@root"), ("ask_opening", "rg_words@tree")),
    ("each agent pattern: index vs rg, project known", ("pattern", "index_patterns@cwd"), ("pattern", "rg_patterns@project")),
    ("each agent pattern: index vs rg, whole tree", ("pattern", "index_patterns@root"), ("pattern", "rg_patterns@tree")),
    ("an episode's patterns as one list: index vs rg, project known", ("episode", "index_patterns@cwd"), ("episode", "rg_patterns@project")),
    ("an episode's patterns as one list: index vs rg, whole tree", ("episode", "index_patterns@root"), ("episode", "rg_patterns@tree")),
    ("index on the opening ask vs rg on the agent's patterns, project known", ("ask_opening", "index_words@cwd"), ("episode", "rg_patterns@project")),
    ("footnote, every ask: index vs rg on plain words, project known", ("ask_all", "index_words@cwd"), ("ask_all", "rg_words@project")),
    ("footnote, every ask: index vs rg on plain words, whole tree", ("ask_all", "index_words@root"), ("ask_all", "rg_words@tree")),
    ("footnote, every ask: index on the ask vs rg on the agent's patterns, project known", ("ask_all", "index_words@cwd"), ("episode", "rg_patterns@project")),
]


def run_session(s, run, table, dev, limit):
    targets, sid = s["targets"], s["id"]
    table.cluster[sid] = s["cluster"]
    scopes = (("project", s["project"], "cwd", s["cwd"]), ("tree", dev, "root", dev))
    # The plain ask.
    ask_rank = {}
    if s["ask"]:
        for scope, root, iscope, icwd in scopes:
            for qset in ("ask_all", "ask_opening") if s["opening"] else ("ask_all",):
                table.add(qset, sid, f"rg_words@{scope}", s["ask_text"], run.rg_words(s["ask"], root), targets)
                ask_rank[iscope] = table.add(qset, sid, f"index_words@{iscope}", " ".join(s["ask"]),
                                             run.index(s["ask"], icwd), targets)
    # Each pattern the agent ran, and the session's patterns as one list.
    joined = []
    for _, _, _, words in s["scored"]:
        joined += [w for w in words if w not in joined]
    shown = " | ".join(f"{k}:{p}" for _, k, p, _ in s["scored"])
    for scope, root, iscope, icwd in scopes:
        results = []
        for pos, kind, pat, words in s["scored"]:
            qid = f"{sid}#{pos}"
            res = run.rg_pattern(kind, pat, root)
            results.append(res)
            table.add("pattern", qid, f"rg_patterns@{scope}", f"{kind}:{pat}", res, targets)
            table.add("pattern", qid, f"index_patterns@{iscope}", " ".join(words), run.index(words, icwd), targets)
        if results:
            dt = sum(r[2] for r in results)
            merged = interleave([r[0] for r in results], limit)
            table.add("episode", sid, f"rg_patterns@{scope}", shown, (merged, 0, dt, "interleaved"), targets)
            best = min(rank_of(r[0], targets) or 99 for r in results)
            table.put("episode", sid, f"rg_patterns_best@{scope}", shown, 0 if best == 99 else best, "",
                      dt, "best pattern", "", targets)
            table.add("episode", sid, f"index_patterns@{iscope}", " ".join(joined), run.index(joined, icwd), targets)
        # Searches to reach a target. rg follows the agent's own sequence,
        # every pattern counted. The index asks once with the plain ask, then
        # tries each pattern that has a word in it.
        n = 0
        for i, (kind, pat, _, _) in enumerate(s["sequence"]):
            if rank_of(run.rg_pattern(kind, pat, root)[0], targets):
                n = i + 1
                break
        table.turns.setdefault(f"rg_patterns@{scope}", []).append(n)
        if run.index_bin:
            n, tried, asked = 0, 0, set()
            if s["ask"]:
                tried = 1
                n = 1 if ask_rank.get(iscope) else 0
            for _, _, words, usable in s["sequence"]:
                if n:
                    break
                if not usable or tuple(words) in asked:
                    continue
                asked.add(tuple(words))
                tried += 1
                if rank_of(run.index(words, icwd)[0], targets):
                    n = tried
            table.turns.setdefault(f"index@{iscope}", []).append(n)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dev-root", required=True, help="the tree the index crawled")
    ap.add_argument("--evalset", action="append", default=[], help="sessions JSONL (repeatable)")
    ap.add_argument("--lexicon", help="lexicon JSONL")
    ap.add_argument("--lexicon-root", help="the repo the lexicon's provenance paths are relative to")
    ap.add_argument("--index-bin", help="agent binary with a crawled index; without it, baselines only")
    ap.add_argument("--index-limit", type=int, default=10, help="--limit passed to index search")
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
    run = Runner(paths, a.timeout, a.limit, a.index_bin, a.index_limit, a.max_words)
    os.makedirs(a.out, exist_ok=True)
    fh = open(os.path.join(a.out, "queries.csv"), "w", newline="", encoding="utf-8")
    w = csv.writer(fh)
    w.writerow(["set", "id", "system", "query", "mode", "rank", "found", "ms", "top", "targets"])
    table = Table(w, a.limit)
    dev = paths.dev_root
    notes = []
    seen, clusters = set(), {}

    for i, path in enumerate(a.evalset):
        drops = Counter()
        sessions = load_sessions(path, "abcdefgh"[i % 8], paths, drops, seen, clusters, a.skip_cwd)
        if a.max_queries:
            sessions = sessions[:a.max_queries]
        notes.append(f"sessions file {i + 1}: {len(sessions)} rows kept from "
                     f"{len({s['cluster'] for s in sessions})} sessions "
                     f"({sum(1 for s in sessions if s['ask'])} with an ask, "
                     f"{sum(1 for s in sessions if s['ask'] and s['opening'])} of them opening a session; "
                     f"{sum(len(s['scored']) for s in sessions)} patterns scored, "
                     f"project known for {sum(1 for s in sessions if s['project_known'])}; "
                     f"by harness {dict(Counter(s['harness'] for s in sessions))}; "
                     f"targets {dict(Counter(s['target_kind'] for s in sessions))}). Dropped:")
        notes += [f"  {c:4d}  {why}" for why, c in sorted(drops.items())] or ["     0"]
        for s in sessions:
            run_session(s, run, table, dev, a.limit)
            fh.flush()
            sys.stderr.write(".")
            sys.stderr.flush()

    if a.lexicon:
        if not a.lexicon_root:
            ap.error("--lexicon needs --lexicon-root")
        drops = Counter()
        lex, root = load_lexicon(a.lexicon, a.lexicon_root, paths, drops)
        if a.max_queries:
            lex = lex[:a.max_queries]
        notes.append(f"lexicon: {len(lex)} lay phrasings kept. Dropped:")
        notes += [f"  {c:4d}  {why}" for why, c in sorted(drops.items())] or ["     0"]
        # The lexicon file holds every phrasing beside its term: a hit on it
        # is the answer key, for either system, so it never counts as a hit.
        exclude = {paths.canon(os.path.realpath(a.lexicon))}
        for q in lex:
            table.cluster[q["id"]] = q["cluster"]
            for name, res in (("rg_words@project", run.rg_words(q["words"], root)),
                              ("rg_words@tree", run.rg_words(q["words"], dev)),
                              ("index_words@cwd", run.index(q["words"], root)),
                              ("index_words@root", run.index(q["words"], dev))):
                table.add("lexicon", q["id"], name, q["phrase"], res, q["targets"], exclude)
            fh.flush()
            sys.stderr.write(".")
            sys.stderr.flush()

    fh.close()
    sys.stderr.write("\n")
    out = ["## Recall and time", "", table.summary(), ""]
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
        out += ["## Searches to reach a target file (top 10)", "", table.turns_table(), ""]
    out += ["## Rows", "", *notes]
    text = "\n".join(out) + "\n"
    with open(os.path.join(a.out, "summary.md"), "w", encoding="utf-8") as sf:
        sf.write(text)
    print(text)


if __name__ == "__main__":
    main()
