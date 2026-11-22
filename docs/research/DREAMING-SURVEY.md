# Offline Memory Consolidation ("Dreaming"): Field Survey

How other systems consolidate agent memory offline, prevent memory rot, and test whether memory helped—and what `agent dream` (`docs/DREAMING.md`) could borrow. `threadlane`, `OpenClaw`, and `Devin` are covered in `docs/DREAMING.md` and skipped here.

## Surveyed systems

### 1. Letta / MemGPT sleep-time agents & MemFS
[Memory & dreaming](https://docs.letta.com/configuration/memory) · [MemFS](https://docs.letta.com/concepts/memfs) · [Blog](https://www.letta.com/blog/sleep-time-compute/)
- **Offline:** Background dreaming subagents run every $N$ steps (`step-count`), on `compaction-event`, or via `/sleeptime` and `/doctor`.
- **I/O:** Reads recent transcripts and memory; writes shared blocks (`rethink_memory`) or **MemFS**, a git-backed Markdown repo (`MEMORY.md` index, always-on root files, on-demand subfolders, `skills/`).
- **Selection:** Prompt-driven split between root rules/persona (capped by system-prompt token budget) and sub-indexed reference notes; no numeric decay.
- **Guards:** Dreaming and `/doctor` run in isolated **git worktrees** so every edit is a reversible commit. Optional *"Agent reviews before applying"* runs a second background pass to critique edits before merge; `/doctor` audits hierarchy drift and duplicates.
- **Evaluation:** No built-in post-session telemetry; evaluated via the sleep-time compute benchmarks below.

### 2. "Sleep-time Compute" (Lin et al., 2025)
[arXiv:2504.13171](https://arxiv.org/abs/2504.13171) · [Code](https://github.com/letta-ai/sleep-time-compute)
- **Offline:** Runs while a stateful context $c$ (repo or history) sits idle before queries arrive.
- **I/O:** Reads raw context $c$; writes re-represented context $c'$ with pre-computed inferences, repo maps, and anticipated Q&A via `rethink_memory`.
- **Selection:** Prompted to anticipate likely user queries; gains scale monotonically with **query predictability** (query log-likelihood given $c$).
- **Guards:** Retains raw/verifiable context alongside $c'$ rather than destructively discarding source facts.
- **Evaluation:** ~5x less test-time compute at matched accuracy (+13% to +18% accuracy ceiling) on Stateful GSM-Symbolic and AIME, 2.5x lower multi-query cost, and fewer exploration tokens on SWE-Features.

### 3. Generative Agents (Park et al., 2023)
[arXiv:2304.03442](https://arxiv.org/abs/2304.03442)
- **Offline:** Reflection fires when cumulative LLM-rated event poignancy (1–10) crosses **150** (~2–3x/day).
- **I/O:** Reads the latest 100 stream records, generates three salient questions, retrieves matches, and appends reflection nodes (including reflections over reflections) back to the stream.
- **Selection:** Ranks retrieval by weighted `recency + importance + relevance`, with $0.995^{\Delta t}$ hourly exponential recency decay since last access.
- **Guards:** Append-only stream (no overwrites); every reflection cites its supporting child record IDs.
- **Evaluation:** Ablating reflection, importance, or recency sharply degraded blinded human ratings of multi-day coherence.

### 4. Mem0 (Chhikara et al., 2025)
[arXiv:2504.19413](https://arxiv.org/abs/2504.19413)
- **Offline:** Async post-turn Extraction + Update pipeline backed by a rolling summary.
- **I/O:** Reads the latest turn, summary, and top-10 vector neighbors; calls `ADD`, `UPDATE`, `DELETE`, or `NOOP` on vector facts (and entity-relation triplets in $\text{Mem0}^g$).
- **Selection:** LLM salience extraction plus cosine similarity; no age decay.
- **Guards:** Showing top-10 neighbors to Update prevents blind duplicates; $\text{Mem0}^g$ marks contradicted graph edges invalid instead of deleting history.
- **Evaluation:** +26% LLM-judge accuracy over OpenAI memory on LOCOMO, with 91% lower p95 latency and >90% fewer tokens than full-context replay.

### 5. Zep / Graphiti (Rasmussen et al., 2025)
[arXiv:2501.13956](https://arxiv.org/abs/2501.13956)
- **Offline:** Async workers maintain a three-tier graph (episodes, entities, communities).
- **I/O:** Reads chat/data episodes; writes entity/community summaries and **bi-temporal** edges tracking transaction time (`created_at`, `expired_at`) and world validity (`valid_at`, `invalid_at`).
- **Selection:** Hybrid cosine + BM25 + graph-distance reranking filtered by temporal validity.
- **Guards:** Contradictions never overwrite facts; edge invalidation sets `invalid_at` on superseded claims, keeping "now" clean while preserving history.
- **Evaluation:** Beats MemGPT on Deep Memory Retrieval and improves temporal accuracy on LongMemEval.

### 6. LangMem (LangChain)
[Docs](https://langchain-ai.github.io/langmem/) · [Code](https://github.com/langchain-ai/langmem)
- **Offline:** `ReflectionExecutor` runs `create_memory_store_manager` and `create_prompt_optimizer` asynchronously (local threads or remote LangGraph), debounced via `after_seconds`.
- **I/O:** Reads trajectories and feedback scores; writes Pydantic-typed items to LangGraph `BaseStore` namespaces and rewrites procedural system prompts (`metaprompt` or `gradient`).
- **Selection:** Schema-guided salience and embedding search; no built-in decay.
- **Guards:** Strict schemas and item IDs bound fact patches, though procedural prompt rewrites lack built-in rollback guards.
- **Evaluation:** Accepts trajectory `{"score", "comment"}` inputs, leaving measurement to external eval harnesses.

### 7. A-MEM (Xu et al., 2025)
[arXiv:2502.12110](https://arxiv.org/abs/2502.12110) · [Code](https://github.com/agiresearch/A-mem)
- **Offline:** Async per-turn Zettelkasten indexing and neighbor-evolution pass.
- **I/O:** Reads new interaction + top-$k$ vector notes; writes an atomic note (raw text, timestamp, keywords, tags, summary, links) and **rewrites neighbor notes' tags and summaries**.
- **Selection:** Cosine retrieval plus link traversal; no pruning or decay.
- **Guards:** Raw interaction text stays immutable inside each note while metadata evolves—though unguarded neighbor mutation risks cascading drift.
- **Evaluation:** Beats MemGPT and flat RAG across six models on LoCoMo multi-hop QA at lower token cost.

### 8. Claude Code (`CLAUDE.md`, Auto Memory, AutoDream) & Anthropic Memory Tool
[Claude Code memory](https://code.claude.com/docs/en/memory) · [API memory tool](https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool)
- **Offline:** Inline session writes (Auto Memory; client-side `memory_20250818` with Context Editing pre-clear warnings), plus **AutoDream**—an idle background subagent behind a dual gate ($\ge 24\text{h}$ *and* $\ge 5$ sessions, or `/dream`)—and manual `/doctor` audits.
- **I/O:** Human `CLAUDE.md`/`AGENTS.md` and glob-scoped `.claude/rules/*.md` are read-only; Auto Memory/AutoDream read transcripts and write per-repo `memory/MEMORY.md` plus typed topic files (`user`, `feedback`, `project`, `reference`); `memory_20250818` runs `/memories` CRUD.
- **Selection:** Enforces a **200-line / 25 KB** startup cap on `MEMORY.md` (topic files load on demand), stamps ISO-8601 `modified`, and skips facts derivable from code, git, or `CLAUDE.md`.
- **Guards:** AutoDream uses a CAS lock, stays read-only on code, converts relative dates ("yesterday") to absolute dates, and drops contradicted notes; `/doctor` cuts code-derivable `CLAUDE.md` bloat; over-cap writes return a compression error.
- **Evaluation:** Context-editing benchmarks show `memory_20250818` + tool clearing lifts long-horizon completion while saving tokens; Claude Code logs `InstructionsLoaded` hooks without scoring compliance.

### 9. ChatGPT Memory
[Announcement](https://openai.com/index/memory-and-new-controls-for-chatgpt/) · [FAQ](https://help.openai.com/en/articles/8590148-memory-in-chatgpt)
- **Offline:** Inline `bio` saves paired with background jobs refreshing cross-session "Reference chat history" profiles.
- **I/O:** Reads non-temporary chats; writes user-visible flat `bio` facts and hidden rolling preference/topic summaries into system context.
- **Selection:** Durability classifier (`bio` has a hard slot cap) plus recency/frequency weighting in rolling summaries.
- **Guards:** Separates editable `bio` facts from implicit history, isolates Temporary Chats, and blocks sensitive traits, though full `bio` slots require manual/model pruning.
- **Evaluation:** Online preference A/B tests; LOCOMO evals show flat `bio` lists trail temporal graphs when facts change.

### 10. Cursor & Windsurf Memories
[Cursor rules](https://cursor.com/docs/rules) · [Windsurf / Devin Local](https://docs.devin.ai/desktop/devin-local/customizations)
- **Offline:** Cursor ran a per-turn sidecar proposing workspace memories; Windsurf Cascade saved workspace memories inline at zero credit cost. **Cursor removed native Memories in v2.1** (`Export memories` to `.mdc`), and Windsurf's Devin Local docs now redirect memories to Rules and Skills.
- **I/O:** Read chat corrections into workspace snippet stores—now superseded by tracked `AGENTS.md`, glob/model-triggered `.cursor/rules/*.mdc` / `.devin/rules/*.md`, and progressive-disclosure `SKILL.md` folders.
- **Selection:** Single-turn LLM heuristic (plus optional user approval in Cursor); no counts or decay.
- **Guards:** Workspace scoping prevented cross-repo bleed, yet unversioned auto-snippets drifted from code and collided with repo rules, driving removal.
- **Evaluation:** None published; Cursor's deprecation shows ungrounded auto-snippets lose to versioned, path-scoped rules and skills.

### 11. Fleet Transcript & Trajectory Miners
[`trace2rules`](https://github.com/rkkautsar/trace2rules) · [Docent](https://transluce.org/docent/blog/introducing-docent) · [`claude-reflect`](https://github.com/BayramAnnakov/claude-reflect) · [ExpeL](https://arxiv.org/abs/2308.10144) · [ReasoningBank](https://arxiv.org/abs/2509.25140)
- **Offline:** Batch transcript analyzers (`trace2rules`, `Docent`), hook-queued `/reflect` passes (`claude-reflect`), and post-rollout learners (`ExpeL`, `ReasoningBank`).
- **I/O:** Read JSONL trajectories and CI/hook files; output proposed `AGENTS.md` rules (`trace2rules`), clustered failure trees (`Docent`), `/commands` (`claude-reflect`), or contrastive strategies mined from **successes and failures** (`ExpeL`, `ReasoningBank`).
- **Selection:** `ExpeL` tracks integer weights via `ADD`/`EDIT`/`UPVOTE`/`DOWNVOTE`, **deleting rules at 0**; `trace2rules` wants ~10+ sessions and scores coverage/confidence; `ReasoningBank` records *why* failures failed.
- **Guards:** `trace2rules audit --proposal` validates candidate rules against past logs before human review and skips CI-enforced rules; `Docent` cites exact transcript spans.
- **Evaluation:** `trace2rules audit` deterministically counts *Followed vs. Violated* sessions per rule (exposing prose rules violated 30–100% of runs); `ReasoningBank` and `ExpeL` lift pass rates on SWE-Bench-Verified, WebArena, and ALFWorld.

## Ideas for us (ranked for waking agents)

Waking agents are short on time and context. Ranked by wakeup leverage:

1. **Closed-loop tripwire telemetry (`trace2rules` + ExpeL).** `evidence.rs` counts trips in *this* batch, not whether existing Tripwires in `start_here.md` cut trips in *later* batches. Matching normalized error signatures across committed dreams lets the digest report pre/post trip rates per tripwire: a prose-resistant tripwire (still violated often) auto-promotes to an `agent friction` candidate for a hook/CLI guard (the Act layer), while one at zero after a cited commit retires cleanly, freeing `start_here.md` lines.
2. **Pre-computed orientation from predictable wakeup queries + derivability lint ("Sleep-time Compute" + Claude `/doctor`).** Mine the first ~10 read/grep calls of each session in `evidence.rs` to see what waking agents hunt for before their first edit, and keep a 5–8 line **Entrypoints** map in `start_here.md` answering those lookups—paired with a `memory check` / `brief.md` rule rejecting directory trees or facts already in `CLAUDE.md`/`AGENTS.md`.
3. **Path- and task-scoped wakeup slices (Claude `.claude/rules/`, Windsurf `glob`, Cursor's post-mortem).** Add optional `paths:` globs and `tasks:` (`docs/ROSTER.md` task kinds) to memory frontmatter (`memory.rs`). When `agent spawn` knows `--task`/`--pick` or target paths, `start_here.rs` appends matching topic summaries under a tight line cap without taxing unrelated spawns.
4. **Structured `superseded_by` metadata and relative-date linting (Zep/Graphiti + AutoDream).** Prose history mixed into topic notes slows hurried readers, and words like "recently" rot silently. Add `superseded_by: commit:<hash>` (like `roster_notes --supersedes`) in `memory.rs`, flag relative dates in `memory check`, and omit superseded notes from wakeup briefings while keeping them on disk.
5. **Distill repeated command sequences into progressive-disclosure skills (`claude-reflect`, Letta MemFS `skills/`).** Multi-step rituals (releases, local-model launches, cross-host tests) bloat `start_here.md`. Detect recurring command pipelines across 3+ sessions in `evidence.rs` and let dreams draft cited `SKILL.md` files—costing one index line at wakeup until invoked, checked by `stale.rs`.
6. **Dual-gate auto-triggers + cheap critic pass before commit (AutoDream + Letta worktree review).** Pair the daily timer (`schedule.rs`) with an idle session-count gate ($\ge 5$ ended sessions *and* cooldown) so active sprints get fast tripwires, plus an optional cross-model reviewer (`agent roster pick --task review`) checking `changed_files` against the three-session preference rule before `agent dream commit`.
