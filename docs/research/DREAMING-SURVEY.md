# Offline Memory Consolidation ("Dreaming"): Field Survey

How other systems consolidate agent memory offline, prevent rot, and test whether memory helped—and what `agent dream` (`docs/DREAMING.md`) can borrow. `threadlane`, `OpenClaw`, and `Devin` are covered in `docs/DREAMING.md`.

## Surveyed systems

### 1. Letta / MemGPT sleep-time agents & MemFS
[Memory & dreaming](https://docs.letta.com/configuration/memory), [MemFS](https://docs.letta.com/concepts/memfs), [Blog](https://www.letta.com/blog/sleep-time-compute/)
- **Offline:** Subagents run every $N$ steps (`step-count`), on `compaction-event`, or via `/sleeptime` and `/doctor`.
- **I/O:** Reads transcripts and memory; writes shared blocks (`rethink_memory`) or **MemFS**, a git-backed Markdown repo (`MEMORY.md` index, root files, on-demand subfolders, `skills/`).
- **Selection:** Splits root rules/persona (token-budgeted) from sub-indexed reference notes; no numeric decay.
- **Guards:** Dreaming and `/doctor` run in **git worktrees** so edits are reversible commits. Optional *"Agent reviews before applying"* critiques edits in a second background pass; `/doctor` audits drift and duplicates.
- **Eval:** No built-in post-session telemetry; evaluated via the sleep-time compute benchmarks below.

### 2. "Sleep-time Compute" (Lin et al., 2025)
[arXiv:2504.13171](https://arxiv.org/abs/2504.13171), [Code](https://github.com/letta-ai/sleep-time-compute)
- **Offline:** Runs while stateful context $c$ sits idle before queries arrive.
- **I/O:** Reads raw context $c$; writes re-represented $c'$ with pre-computed inferences, repo maps, and anticipated Q&A via `rethink_memory`.
- **Selection:** Anticipates likely queries; gains scale monotonically with **query predictability** (query log-likelihood given $c$).
- **Guards:** Keeps raw context alongside $c'$ rather than discarding source facts.
- **Eval:** ~5x less test-time compute at matched accuracy (+13–18% ceiling) on Stateful GSM-Symbolic and AIME, 2.5x lower multi-query cost, and fewer exploration tokens on SWE-Features.

### 3. Generative Agents (Park et al., 2023)
[arXiv:2304.03442](https://arxiv.org/abs/2304.03442)
- **Offline:** Fires when cumulative LLM-rated event poignancy (1–10) crosses **150** (~2–3x/day).
- **I/O:** Reads the latest 100 stream records, generates three salient questions, retrieves matches, and appends reflection trees back to the stream.
- **Selection:** Ranks retrieval by weighted `recency + importance + relevance`, with $0.995^{\Delta t}$ hourly exponential recency decay since last access.
- **Guards:** Append-only stream; every reflection cites its supporting child record IDs.
- **Eval:** Ablating reflection, importance, or recency sharply degraded blinded human coherence ratings.

### 4. Mem0 (Chhikara et al., 2025)
[arXiv:2504.19413](https://arxiv.org/abs/2504.19413)
- **Offline:** Async post-turn Extraction + Update pipeline backed by a rolling summary.
- **I/O:** Reads latest turn, summary, and top-10 vector neighbors; calls `ADD`, `UPDATE`, `DELETE`, or `NOOP` on vector facts (and triplets in $\text{Mem0}^g$).
- **Selection:** LLM salience extraction plus cosine similarity; no age decay.
- **Guards:** Passing top-10 neighbors into Update prevents duplicates; $\text{Mem0}^g$ marks contradicted edges invalid instead of deleting history.
- **Eval:** +26% LLM-judge accuracy over OpenAI memory on LOCOMO, 91% lower p95 latency, and >90% fewer tokens than full-context replay.

### 5. Zep / Graphiti (Rasmussen et al., 2025)
[arXiv:2501.13956](https://arxiv.org/abs/2501.13956)
- **Offline:** Async workers maintain a three-tier graph (episodes, entities, communities).
- **I/O:** Reads episodes; writes entity/community summaries and **bi-temporal** edges tracking transaction (`created_at`, `expired_at`) and validity (`valid_at`, `invalid_at`) times.
- **Selection:** Hybrid cosine + BM25 + graph-distance reranking filtered by validity.
- **Guards:** Never overwrites facts; sets `invalid_at` on contradicted edges, keeping current state clean while preserving history.
- **Eval:** Beats MemGPT on Deep Memory Retrieval and lifts temporal accuracy on LongMemEval.

### 6. LangMem (LangChain)
[Docs](https://langchain-ai.github.io/langmem/) · [Code](https://github.com/langchain-ai/langmem)
- **Offline:** `ReflectionExecutor` runs `create_memory_store_manager` and `create_prompt_optimizer` asynchronously, debounced via `after_seconds`.
- **I/O:** Reads trajectories and feedback scores; writes Pydantic-typed items to LangGraph `BaseStore` and rewrites procedural prompts (`metaprompt` or `gradient`).
- **Selection:** Schema-guided salience and embedding search; no built-in decay.
- **Guards:** Strict schemas and item IDs bound fact patches; prompt rewrites lack built-in rollback guards.
- **Eval:** Takes trajectory `{"score", "comment"}` inputs, delegating measurement to external evals.

### 7. A-MEM (Xu et al., 2025)
[arXiv:2502.12110](https://arxiv.org/abs/2502.12110) · [Code](https://github.com/agiresearch/A-mem)
- **Offline:** Async per-turn Zettelkasten indexing and neighbor-evolution pass.
- **I/O:** Reads new interaction + top-$k$ notes; writes an atomic note (raw text, timestamp, keywords, tags, summary, links) and **rewrites neighbor tags/summaries**.
- **Selection:** Cosine retrieval plus link traversal; no pruning or decay.
- **Guards:** Raw text stays immutable inside each note while metadata evolves—though unguarded neighbor mutation risks cascading drift.
- **Eval:** Beats MemGPT and flat RAG across six models on LoCoMo multi-hop QA at lower token cost.

### 8. Claude Code (`CLAUDE.md`, Auto Memory, AutoDream) & Anthropic Memory Tool
[Claude Code memory](https://code.claude.com/docs/en/memory) · [API memory tool](https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool)
- **Offline:** Inline writes (Auto Memory; client-side `memory_20250818` with Context Editing pre-clear warnings), plus **AutoDream**—an idle subagent gated on $\ge 24\text{h}$ *and* $\ge 5$ sessions (or `/dream`)—and `/doctor` checkups.
- **I/O:** Human `CLAUDE.md`/`AGENTS.md` and glob-scoped `.claude/rules/*.md` are read-only; Auto Memory/AutoDream read transcripts and write per-repo `memory/MEMORY.md` plus typed topic files (`user`, `feedback`, `project`, `reference`); `memory_20250818` runs `/memories` CRUD.
- **Selection:** **200-line / 25 KB** startup cap on `MEMORY.md` (topic files load on demand), ISO-8601 `modified` stamps, and skips anything derivable from code, git, or `CLAUDE.md`.
- **Guards:** AutoDream holds a CAS lock, stays read-only on code, rewrites relative dates ("yesterday") to absolute dates, and prunes contradictions; `/doctor` cuts code-derivable bloat; over-cap writes trigger a compression error.
- **Eval:** Context-editing evals show `memory_20250818` + tool clearing lifts long-horizon completion while saving tokens; Claude Code logs `InstructionsLoaded` hooks without scoring compliance.

### 9. ChatGPT Memory
[Announcement](https://openai.com/index/memory-and-new-controls-for-chatgpt/) · [FAQ](https://help.openai.com/en/articles/8590148-memory-in-chatgpt)
- **Offline:** Inline `bio` saves plus background jobs refreshing cross-session "Reference chat history" profiles.
- **I/O:** Reads non-temporary chats; writes flat `bio` facts and hidden rolling preference/topic summaries into system context.
- **Selection:** Durability classifier (`bio` slot cap) plus recency/frequency weighting in rolling summaries.
- **Guards:** Splits editable `bio` facts from synthesized history, isolates Temporary Chats, and blocks sensitive traits; full `bio` slots require pruning.
- **Eval:** Online preference A/B tests; LOCOMO shows flat `bio` lists trail temporal graphs when facts change.

### 10. Cursor & Windsurf Memories
[Cursor rules](https://cursor.com/docs/rules) · [Windsurf / Devin Local](https://docs.devin.ai/desktop/devin-local/customizations)
- **Offline:** Cursor ran a per-turn sidecar proposing memories; Windsurf Cascade saved memories inline. **Cursor removed native Memories in v2.1** (`Export memories` to `.mdc`), and Windsurf's Devin Local docs now redirect memories to Rules and Skills.
- **I/O:** Read chat corrections into workspace stores—now superseded by tracked `AGENTS.md`, glob/model-triggered `.cursor/rules/*.mdc` / `.devin/rules/*.md`, and progressive-disclosure `SKILL.md` folders.
- **Selection:** Single-turn heuristic (with optional approval in Cursor); no counts or decay.
- **Guards:** Workspace scoping stopped cross-repo bleed, yet unversioned snippets drifted from code and collided with repo rules, driving removal.
- **Eval:** None published; deprecation shows ungrounded auto-snippets lose to versioned, path-scoped rules and skills.

### 11. Fleet Transcript & Trajectory Miners
[`trace2rules`](https://github.com/rkkautsar/trace2rules) · [Docent](https://transluce.org/docent/blog/introducing-docent) · [`claude-reflect`](https://github.com/BayramAnnakov/claude-reflect) · [ExpeL](https://arxiv.org/abs/2308.10144) · [ReasoningBank](https://arxiv.org/abs/2509.25140)
- **Offline:** Batch analyzers (`trace2rules`, `Docent`), hook-queued `/reflect` sweeps (`claude-reflect`), and post-rollout learners (`ExpeL`, `ReasoningBank`).
- **I/O:** Read JSONL trajectories and CI/hooks; emit proposed `AGENTS.md` rules (`trace2rules`), failure clusters (`Docent`), `/commands` (`claude-reflect`), or contrastive strategies from **successes and failures** (`ExpeL`, `ReasoningBank`).
- **Selection:** `ExpeL` adjusts weights via `ADD`/`EDIT`/`UPVOTE`/`DOWNVOTE`, **deleting rules at 0**; `trace2rules` scores coverage/confidence across 10+ sessions; `ReasoningBank` records *why* failures failed.
- **Guards:** `trace2rules audit --proposal` tests candidate rules against past logs before human review and skips CI-enforced rules; `Docent` cites exact transcript spans.
- **Eval:** `trace2rules audit` deterministically counts *Followed vs. Violated* sessions per rule (catching prose rules violated 30–100% of runs); `ReasoningBank` and `ExpeL` lift SWE-Bench-Verified, WebArena, and ALFWorld pass rates.

## Ideas for us (ranked for waking agents)

Waking agents have scarce time and context. Ranked by wakeup value:

1. **Closed-loop tripwire telemetry (`trace2rules` + ExpeL).** `evidence.rs` counts trips in *this* batch, not whether `start_here.md` Tripwires cut trips in *later* batches. Matching normalized error signatures across committed dreams lets the digest report pre/post trip rates: a prose-resistant tripwire auto-promotes to an `agent friction` candidate for a hook/CLI guard (Act layer), while one at zero after a cited commit retires cleanly, freeing `start_here.md` lines.
2. **Pre-computed orientation from predictable wakeup queries + derivability lint ("Sleep-time Compute" + Claude `/doctor`).** Mine the first ~10 read/grep calls per session in `evidence.rs` to find what waking agents hunt for before editing, keeping a 5–8 line **Entrypoints** map in `start_here.md`—paired with a `memory check` / `brief.md` rule rejecting directory trees or facts already in `CLAUDE.md`/`AGENTS.md`.
3. **Path- and task-scoped wakeup slices (Claude `.claude/rules/`, Windsurf `glob`, Cursor's post-mortem).** Add optional `paths:` globs and `tasks:` (`docs/ROSTER.md` kinds) to memory frontmatter (`memory.rs`). When `agent spawn` knows `--task`/`--pick` or target paths, `start_here.rs` appends matching topic summaries under a tight line cap without taxing unrelated spawns.
4. **Structured `superseded_by` metadata and relative-date linting (Zep/Graphiti + AutoDream).** Prose history slows hurried readers, and words like "recently" rot silently. Add `superseded_by: commit:<hash>` (like `roster_notes --supersedes`) in `memory.rs`, flag relative dates in `memory check`, and omit superseded notes from wakeup briefings while keeping them on disk.
5. **Distill repeated command sequences into progressive-disclosure skills (`claude-reflect`, Letta MemFS `skills/`).** Multi-step rituals bloat `start_here.md`. Detect recurring command pipelines across 3+ sessions in `evidence.rs` and draft cited `SKILL.md` files—costing one index line at wakeup until invoked, checked by `stale.rs`.
6. **Dual-gate auto-triggers + cheap critic pass before commit (AutoDream + Letta worktree review).** Pair the timer (`schedule.rs`) with an idle session-count gate ($\ge 5$ ended sessions *and* cooldown) so sprints get timely tripwires, plus an optional cross-model reviewer (`agent roster pick --task review`) checking `changed_files` against the three-session preference rule before `agent dream commit`.
