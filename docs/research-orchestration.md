# Lead mode: research notes on speed, tokens and reliability (2026-09-25)

Sources are linked inline. **U** means unverified.

**CLI versions checked:**
- claude 2.1.283
- codex-cli 0.157.0
- Kimi Code 0.29.1. Its docs are already at 2.1.1, so check Kimi flags against the installed build.

## Top 10, by impact
1. **Mechanical plan validation.**
   - The lead submits a typed plan: `{id,title,goal,owns:[globs],reads,depends_on,acceptance:"<cmd>",size:S|M|L,kind,tier}`.
   - Reject overlapping `owns` between tasks that can run at the same time, or add a `depends_on` edge.
   - Send "hotspot" files to one scaffold task that merges first: lockfiles, Cargo.toml/package.json, migrations, mod.rs/route/registry/barrel files.
   - Size caps: S ≈100 lines/≤3 files, M ≈400/≤8, L = split it.
   - Solo gate: with ≤2 tasks, or tasks that mostly share files, run one worker and don't fan out.
2. **Serial merge queue on an integration branch.** For each task: `git merge-tree --write-tree`, then a squash candidate, then the **gate on the merged tree** in a scratch worktree, then fast-forward int. One commit per task, with an `Oriel-Task: <id>` trailer. Conflicts and gate failures go back to **the same worker session**, 2 tries, then re-dispatch fresh.
3. **Event-driven, read-only lead.**
   - A blocking `oriel_wait(events, timeout_s)` means no polling.
   - Results come back as compact cards: `{status, summary≤120 words, files:[{path,+,-}], gate:{pass, first_failure≤20 lines}, tokens, cost, questions[]}`. The full diff only via `task_diff(id, path?, page)`.
   - The lead can't edit files. For Claude: `--disallowedTools "Edit Write NotebookEdit"`.
4. **Short, byte-stable briefs (≤1.5k tokens).** Shared preamble first, the task block last. Never hand over a transcript. Include "outside `owns` → stop, report blocked".
5. **Minimal launch profile per agent:**
   - **Claude:**
     ```
     -p --output-format stream-json --verbose --session-id <uuid> --model <m> --max-turns <n> --max-budget-usd <b> --permission-mode acceptEdits --strict-mcp-config --mcp-config <f> --exclude-dynamic-system-prompt-sections --json-schema <report.json>
     ```
     - `--exclude-dynamic-system-prompt-sections` matters because the cache is per directory, so each worktree starts cold without it.
     - `--bare` only works with an API key.
     - Start same-model workers about 5 s apart so later ones can reuse the first one's cache.
   - **Codex:**
     ```
     exec --json -C <wt> -s workspace-write -m <m> -c model_reasoning_effort="medium" --ignore-user-config -c 'mcp_servers.oriel.command="oriel"' -c 'mcp_servers.oriel.args=[...]' -c mcp_servers.oriel.tool_timeout_sec=1800 --output-schema report.json -o <wt>/.oriel/last.json
     ```
   - **Kimi:** `kimi -p "<brief>" --output-format stream-json -m kimi-code/kimi-for-coding`.
     - MCP servers only load from `<wt>/.kimi-code/mcp.json`. Add that file to `.git/info/exclude`.
     - There's no budget flag, so oriel enforces limits.
     - Set `KIMI_CODE_AGENT_SWARM_MAX_CONCURRENCY=2`.
   - **No nested fan-out in workers:** Codex `-c agents.enabled=false`, Claude disallow `Agent`.
6. **Watchdog.**
   - **Stuck:** the same tool call and result 4 times, the same failing command 3 times, or an A/B alternation 6 cycles. Thresholds from OpenHands.
   - **Spinning:** 15 calls or 10 min with no worktree change. Exclude builds, tests and sleeps.
   - **Hung:** 5 min with no stream event.
   - **Escalation:** nudge, then stop and resume once, then kill the process tree (Windows Job Object) and re-dispatch to another agent or tier with attempt notes, then mark it blocked.
   - **Rate limits:** a Claude `rate_limit_event` with status `rejected`, or a Codex `rate_limit_reached_type`, pauses that vendor until `resets_at`.
7. **Routing.** Tier policy in the lead prompt:
   ```
   Tiers: premium=opus|gpt-5.6-sol; standard=sonnet|gpt-5.6-terra|kimi-for-coding; cheap=haiku|gpt-5.6-luna|kimi-for-coding-highspeed
   - mechanical (rename, docs, tests-by-pattern, lint/deprecations): cheap, effort low
   - feature within owned files + acceptance test: standard, effort medium
   - cross-cutting refactor, concurrency, unclear-root-cause bug, perf: premium, effort high
   - frontend/UI: kimi if on roster; shell/CI/build scripts: codex
   - reviewer vendor != author vendor; spread concurrent tasks across vendors (separate limits)
   - skip an agent at 5h>=85% or weekly>=90%; weekly remaining <25% => S tasks only; prefer the window that resets soonest
   ```
   Also keep a per-agent track record (first-try merges, gate failures, retries, tokens, time) and show it in `roster()`.
8. **oriel enforces budgets and timeouts for Codex and Kimi.** Tail the rollout / `wire.jsonl` files for live token counts.
9. **Reviewer from a different vendor, only on big or hotspot diffs.** Trigger when the diff is over 150 lines, touches hotspots, the task is size M or larger, or it's security/concurrency.
   - Claude-written diffs: `codex exec review --commit <sha>`.
   - Codex-written diffs: `claude -p` with the schema `{blocking[], optional[]}`.
   - Only blocking findings go back to the worker.
10. **Best-of-2 across two vendors** for hard or already-failed tasks. Pick by gate, then smallest diff, then reviewer. If a worker fails twice, re-dispatch fresh on the premium tier.

## Why (sources)
- **Keep it simple; orchestrator-workers fits unpredictable coding subtasks.** https://www.anthropic.com/engineering/building-effective-agents
- **Multi-agent is expensive.** About 15× chat tokens; an Opus lead with Sonnet workers beat solo Opus by 90%. Coding parallelizes worse than research, and vague briefs cause duplicated work. https://www.anthropic.com/engineering/multi-agent-research-system
- **Claude Code agent teams:** about 7× tokens, 3–5 teammates. Two agents editing one file overwrite each other, and the lead implements work itself instead of waiting. https://code.claude.com/docs/en/agent-teams · https://code.claude.com/docs/en/costs
- **Parallel agents diverge on unstated assumptions.** https://cognition.com/blog/dont-build-multi-agents
- **The C compiler run:** 16 agents, lock files to claim tasks, frequent conflicts, and CI needed because agents broke existing work. https://www.anthropic.com/engineering/building-c-compiler
- **Human review is the bottleneck; 3–5 parallel is the sweet spot.** https://simonwillison.net/2025/Oct/5/parallel-coding-agents/
- **Semantic conflicts merge cleanly; serialize the landmine files.** https://sanudesk.com/blog/merge-conflicts-parallel-ai-agents
- **Predict conflicts early** (paths, then hunks, then symbols). https://codeongrass.com/blog/parallel-worktrees-conflict-prediction/
- **Prompt caching:** Claude's cache is per directory, including worktrees; OpenAI caches prefixes of ≥1024 tokens automatically. https://code.claude.com/docs/en/prompt-caching · https://developers.openai.com/api/docs/guides/prompt-caching
- **Keep what workers return small** (about 1–2k-token summaries). https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents · https://www.anthropic.com/engineering/writing-tools-for-agents
- **Give the agent a check it can run;** reviewers limited to correctness. https://code.claude.com/docs/en/best-practices
- **Stuck detection thresholds** (and don't kill agents that are waiting). https://docs.openhands.dev/sdk/guides/agent-stuck-detector
- **Headless Claude:** SIGINT ends the turn cleanly; SIGTERM exits 143. https://code.claude.com/docs/en/headless

## CLI capability table
| | Claude Code | Codex | Kimi Code |
|---|---|---|---|
| headless | `-p --output-format stream-json --verbose` | `exec --json` (`turn.*`, items incl. `mcp_tool_call`) | `-p --output-format stream-json` |
| MCP | `--mcp-config` + `--strict-mcp-config` | `-c mcp_servers.x.*` (`required=true` fails fast) | `.kimi-code/mcp.json` only |
| structured output | `--json-schema` | `--output-schema`, `-o` | none → action protocol |
| resume | `--resume`, `--session-id`, `--fork-session` | `exec resume <id>\|--last`, `exec fork` | `-S <id>` (U with `-p`) |
| caps | `--max-turns`, `--max-budget-usd` | none → oriel enforces | `loop_control.max_steps_per_turn` |
| minimal | `--bare` (API key only) | `--ignore-user-config`, `--ephemeral` | `--skills-dir` |
