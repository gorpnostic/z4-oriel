# AI features: research notes (2026-09-25)

Source-backed notes for the `agents` (orchestrator) and `your AIs` apps. **U** = unverified.

## Installed on the dev machine (Windows)
- **Installed:** claude 2.1.282 · codex-cli 0.147.0 · **Kimi Code** 0.29.1 (`~\.kimi-code\bin\kimi.exe`) · aider 0.86.2 · Ollama 0.34.0.
- **herdr 0.8.2** is installed too, and hooks into Codex via `~\.codex\hooks.json`.
- **Not on PATH:** gemini, opencode, copilot, cursor `agent`, qwen, amp, droid, crush, goose.
- `~\.claude\.credentials.json` exists (OAuth). Never read or print its values.

## 1. AI coding CLIs: install · verify · sign in · headless
| CLI (bin) | Windows | macOS/Linux | Arch | verify | sign-in | headless + JSON |
|---|---|---|---|---|---|---|
| Claude Code (`claude`) | `irm https://claude.ai/install.ps1 \| iex` · `winget install Anthropic.ClaudeCode` | `curl -fsSL https://claude.ai/install.sh \| bash` · npm `@anthropic-ai/claude-code` | (AUR U) | `claude --version` | run `claude` (browser OAuth, `/login`) or `ANTHROPIC_API_KEY` | `claude -p --output-format stream-json --verbose` |
| Codex (`codex`) | `irm https://chatgpt.com/codex/install.ps1 \| iex` · npm `@openai/codex` · winget `OpenAI.Codex` | `curl -fsSL https://chatgpt.com/codex/install.sh \| sh` | `pacman -S openai-codex` | `codex --version` | `codex login` (browser; `--device-auth`) · `codex login status` | `codex exec --json` |
| Kimi Code (`kimi`) | `irm https://code.kimi.com/kimi-code/install.ps1 \| iex` (needs Git for Windows) | `curl -fsSL https://code.kimi.com/kimi-code/install.sh \| bash` · npm `@moonshot-ai/kimi-code` | U | `kimi --version` (also check `~/.kimi-code/bin`) | `kimi login` (device code URL) | `kimi -p "…" --output-format stream-json` |
| Gemini CLI (`gemini`) | npm `@google/gemini-cli` | npm · brew `gemini-cli` | `pacman -S gemini-cli` | `gemini --version` | run `gemini` → Sign in with Google, or `GEMINI_API_KEY` | `gemini -p "…" --output-format stream-json` |
| OpenCode (`opencode`) | npm `opencode-ai` · scoop/choco `opencode` | `curl -fsSL https://opencode.ai/install \| bash` | `pacman -S opencode` | `opencode --version` | `opencode auth login` | `opencode run --format json "…"` |
| Aider (`aider`) | `irm https://aider.chat/install.ps1 \| iex` | `curl -LsSf https://aider.chat/install.sh \| sh` · `uv tool install aider-chat` | AUR U | `aider --version` | API keys only | `aider --message "…" --yes-always --no-pretty` (no JSON) |
| Copilot CLI (`copilot`) | `winget install GitHub.Copilot` · npm `@github/copilot` | `curl -fsSL https://gh.io/copilot-install \| bash` | U | `copilot version` | `copilot login` (device flow) | `copilot -p "…" --allow-all-tools --output-format json` |
| Cursor Agent (`agent`) | `irm 'https://cursor.com/install?win32=true' \| iex` | `curl https://cursor.com/install -fsS \| bash` | U | `agent --version` | `agent login` | `agent -p "…" --output-format stream-json` |
| Qwen Code (`qwen`) | npm `@qwen-code/qwen-code` | npm · brew `qwen-code` | `pacman -S qwen-code` | `qwen --version` | `/auth` in TUI | `qwen -p "…" --output-format stream-json` |
| Amp (`amp`) | WSL only | `curl -fsSL https://ampcode.com/install.sh \| bash` · npm `@ampcode/cli` | U | `amp version` | `amp login` | `amp -x "…" --stream-json` |
| Factory Droid (`droid`) | `irm https://app.factory.ai/cli/windows \| iex` | `curl -fsSL https://app.factory.ai/cli \| sh` | U | `droid --version` | run `droid` (browser) | `droid exec "…" -o stream-jsonrpc` |
| Crush (`crush`) | `winget install charmbracelet.crush` · npm `@charmland/crush` | brew `charmbracelet/tap/crush` | AUR `crush-bin` | `crush --version` | provider env vars | `crush run -q "…"` (no JSON) |
| Goose (`goose`) | `download_cli.ps1` from github.com/aaif-goose/goose releases | `curl -fsSL https://github.com/aaif-goose/goose/releases/download/stable/download_cli.sh \| bash` | AUR U | `goose --version` | `goose configure` | `goose run -t "…" --output-format stream-json` |

**How installs should work:**
- Run installs and sign-ins in a visible terminal pane, since several are interactive (browser or device code).
- Order of preference: the native installer or winget on Windows; pacman `extra` on Arch, then the script, then npm.
- npm on the dev machine may block package install scripts, which is another reason to prefer native installers.
- Detection: look for the binary on PATH, then in known folders (`~/.kimi-code/bin`, `~/.local/bin`, `%LOCALAPPDATA%\Programs`). Run `--version` with a 3 s timeout.

## 2. Usage & limits

### Claude Code
**A. Local transcripts**
- **Where:** `~/.claude/projects/<cwd-slug>/<sessionId>.jsonl`, plus `<sessionId>/subagents/**/*.jsonl`. Also check `CLAUDE_CONFIG_DIR` and `~/.config/claude/projects`.
- **Records:** `type:"assistant"` lines with `requestId`, `timestamp`, `cwd`, `message.{id, model, usage}`. The `usage` object has:
  - `input_tokens`, `output_tokens`
  - `cache_creation_input_tokens`, `cache_read_input_tokens`
  - `cache_creation.{ephemeral_5m_input_tokens, ephemeral_1h_input_tokens}`
  - `service_tier`, `inference_geo`
- **Dedupe is required:** one response is written as several lines. Key on `message.id + ":" + requestId` and keep the first.
- **Cost** = `in*P_in + out*P_out + 5m*P_in*1.25 + 1h*P_in*2 + read*P_read`, times 1.1 if `inference_geo` is US-only.
- **Prices ($/MTok), in / 5m-write / 1h-write / cache-read / out:**

  | Model | in | 5m write | 1h write | cache read | out |
  |---|---|---|---|---|---|
  | Opus 5.5 | 4 | 5 | 8 | 0.20 | 20 |
  | Opus 5 / 4.x | 5 | 6.25 | 10 | 0.50 | 25 |
  | Sonnet 5 | 2 | 2.5 | 4 | 0.20 | 10 |
  | Sonnet 4.6 | 3 | 3.75 | 6 | 0.30 | 15 |
  | Haiku 4.5 | 1 | 1.25 | 2 | 0.10 | 5 |
  | Fable 5.1 | 10 | 12.5 | 20 | 0.25 | 50 |

- **5-hour blocks (ccusage style):** a block starts at the first message, floored to the hour, and lasts 5 h. It's an estimate, since it can't see other devices.

**B. Real plan percentages (official):** the statusLine command gets JSON on stdin with:
- `rate_limits.{five_hour, seven_day, spend_limit}.{used_percentage, resets_at}` (`resets_at` in epoch seconds). Only on Pro/Max, only after the first response, and each window may be missing.
- `cost.total_cost_usd`, `context_window.{used_percentage, …}`, `prompt_cache.{warm, hit_ratio, expires_at, misses}`, `effort.level`, `model`.
- The plan: `oriel usage-sink` (Claude's `statusLine.command`) writes that JSON to `<data dir>/usage/claude.json` and prints a short status line.
- Docs: https://code.claude.com/docs/en/statusline

**C. Headless stream:** `{type:"rate_limit_event", rate_limit_info:{status:"allowed"|"allowed_warning"|"rejected", resetsAt?, utilization?, rateLimitType?, unifiedWindows?.{five_hour,seven_day}.{utilization,resetsAt}}}`. Parse every field as optional. The `result` event has `total_cost_usd` and `modelUsage[model].{inputTokens, outputTokens, cacheReadInputTokens, cacheCreationInputTokens, costUSD}`.

**D. Don't use:** the community OAuth endpoint `api.anthropic.com/api/oauth/usage`. It isn't endorsed, it rate-limits hard, and it would mean using the user's subscription token.

### Codex (confirmed on real files)
- **Where:** `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, or `$CODEX_HOME`.
- **Line types:** `session_meta` (`cli_version`, `cwd`), `turn_context` (`model` e.g. `gpt-5.6-terra`, `effort`), `event_msg`.
- **Token events:** `event_msg` with `payload.type:"token_count"`.
  - `info.total_token_usage` and `info.last_token_usage`, each with `{input_tokens, cached_input_tokens, cache_write_input_tokens, output_tokens, reasoning_output_tokens, total_tokens}`
  - `info.model_context_window`
  - `payload.rate_limits` = `{limit_id, plan_type, primary:{used_percentage, window_minutes, resets_at}, secondary, credits:{has_credits, unlimited, balance}, rate_limit_reached_type}`
- **Windows vary by plan:** a free plan showed a 43200-minute window, others 299 or 10079. `resets_at` is epoch seconds or null.
- **Recipe:** add up `last_token_usage` per turn, priced with the model from the nearest earlier `turn_context`. Take limits from the newest `token_count` in the newest file.
- **Prices ($/MTok), in / cached / out:**
  - gpt-5.6-sol: 4 / 0.40 / 20
  - gpt-5.6-terra: 2 / 0.20 / 12
  - gpt-5.6-luna: 0.20 / 0.02 / 1.20
  - gpt-5.3-codex: 1.75 / 0.175 / 14

  On a ChatGPT plan these are "API-equivalent" figures.

### Others
- **Kimi Code:** `~/.kimi-code/sessions/<ws>/<session>/agents/<agent>/wire.jsonl`, `StatusUpdate` records with `usage.{inputOther, output, inputCacheRead, inputCacheCreation}`. Use turn-scoped records only.
- **Gemini:** `~/.gemini/tmp/*/chats/*.json(l)` with `tokens.{input, output, cached, thoughts, tool, total}`. `input` includes cached.
- **OpenCode:** `~/.local/share/opencode/storage/message/<sid>/msg_*.json`. Price it from the tokens.
- **Ollama:** no limits.
  - `GET http://127.0.0.1:11434/api/tags` lists `models[].{name, size, details.{parameter_size, quantization_level}}`.
  - `GET /api/ps` shows what's loaded; `GET /api/version` gives the version.
- **Cross-check:** `npx ccusage@latest daily --json`.

**Presentation:** plan-% bars are authoritative. Label dollar figures "API-equivalent".

## 3. Orchestrator

### Patterns
Every tool converges on the same design:
- a git worktree per agent;
- a small state machine (`todo → running → blocked → review → merged | discarded | failed`);
- a diff review, with comments sent back to the agent;
- squash-merge or a PR, then archive;
- hooks first, with screen-scraping as the fallback;
- a notification on blocked or done.

**The gap:** none of these tools show per-agent cost.

### Design for oriel's `agents` app
**Task data:** `{ id, repo, title, prompt, agent (claude|codex|kimi…), model, effort, mode (interactive|headless), status, branch, base_sha, worktree, tag (oriel pane tag), session_id, transcript_path, tokens, cost_usd, created/started/finished }`.

**Spawning:**
- `git worktree add -b oriel/<slug> <wt> <base>`, with `<wt>` under `<data dir>/wt/<repo>/<slug>`.
- **Interactive (default):** a real terminal pane in the worktree running `claude "<prompt>"` or `codex "<prompt>"`. Both accept an initial prompt as a positional argument.
- **Headless (later):** `claude -p --output-format stream-json --verbose --model sonnet --max-turns N --max-budget-usd B --permission-mode acceptEdits` and `codex exec --json -C <wt>`.

**Status, hooks first:** give each task `ORIEL_TASK_ID`. Write a per-worktree `.claude/settings.local.json`, so the user's global settings are never touched, with these hooks:

| Hook | Reports |
|---|---|
| `UserPromptSubmit`, `PreToolUse` | running |
| `Notification` | blocked |
| `Stop` | idle → review |
| `SessionStart` | the `session_id` and `transcript_path` |

Each hook runs `oriel report --task <id> --state <s>` (stdin carries the hook JSON). Fallback: terminal screen-scraping, which oriel already has via `Pane::activity()` (Working / Blocked / Idle).

**Review:**
- The diff is `git -C <wt> diff <base_sha>` (committed plus uncommitted), with the file list on the left and hunks on the right.
- Keys:

  | Key | Does |
  |---|---|
  | `c` | comment: sent to the agent as the next prompt |
  | `m` | merge: auto-commit leftovers, `git merge --squash oriel/<slug>` into base, then `git worktree remove` + branch delete |
  | `x` | discard |
  | `t` | run tests |

- Show conflicts before merging with `git merge-tree`.

**Lead planner:** a headless `claude -p` run with read-only tools returns `[{title, prompt, agent, model, depends_on}]`. The cards land in Todo for the user to approve.

**Mockup:**
```
┌ agents ─ repo: oriel (main) ───────────── 3 running · 1 blocked · $1.84 today ┐
│ TODO            │ RUNNING               │ REVIEW             │ DONE           │
│ ▢ theme toggle  │ ● fix pty resize      │ ◆ usage parser     │ ✓ font cache   │
│   claude·sonnet │   codex·terra  4m     │   claude·opus5.5   │   merged 10:02 │
│                 │ ⚠ ais pane   BLOCKED  │   +212 −40 · $0.61 │                │
│ [n]ew [P]lan [enter] open pane [d]iff [c]omment [m]erge [x]discard            │
```

**MVP:** the board, new task → worktree + interactive pane (Claude and Codex), hook status plus the scrape fallback, diff with squash-merge and discard, cost per task from its transcript, and toasts. **Later:** headless mode, the planner, best-of-N, more agents, agent-to-agent messaging.

## 4. Token-efficiency toggles

### Claude Code (https://code.claude.com/docs/en/costs, /env-vars, /settings-reference)
- **Model:** `model`, `CLAUDE_CODE_SUBAGENT_MODEL=haiku` (plus `CLAUDE_CODE_SUBAGENT_MODEL_FORCE=1`).
- **Effort:** `effortLevel`, or `CLAUDE_CODE_EFFORT_LEVEL`.
- **Output cap:** `CLAUDE_CODE_MAX_OUTPUT_TOKENS`.
- **Compaction:** `autoCompactEnabled`, `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`, `CLAUDE_CODE_DISABLE_1M_CONTEXT=1`.
- **Caching:** keep `DISABLE_PROMPT_CACHING` off.
- **Output limits:** `MAX_MCP_OUTPUT_TOKENS`, `BASH_MAX_OUTPUT_LENGTH`.
- **Extra requests:** `promptSuggestionEnabled:false`.
- **Headless runs:** `--max-turns`, `--max-budget-usd`, `--bare`.
- **Hygiene:** CLAUDE.md under 200 lines. Use `/context` and `/mcp` to see what costs space.

### Codex `~/.codex/config.toml` (https://learn.chatgpt.com/docs/config-file/config-reference)
- `model`, `model_reasoning_effort` (`low|medium|high|xhigh…`), `model_reasoning_summary`, `model_verbosity`
- `model_auto_compact_token_limit`, `tool_output_token_limit`, `web_search`
- `agents.max_threads`, `agents.default_subagent_model`
- `profiles.<name>`, launched with `codex -p <name>`

### Presets for oriel
Frugal / Balanced / Max.
- **Claude side:** merge only `effortLevel`, `model`, `autoCompactEnabled` and the `env` keys into `~/.claude/settings.json`. Never overwrite other keys, and always show a diff and ask before writing. That file holds the user's hooks.
- **Codex side:** add a `[profiles.oriel-frugal]` block.
- **Live readouts:** CLAUDE.md line count, MCP servers enabled, cache hit %.

## 5. Lead mode (2026-09-25)

One goal; a **lead** agent the user picks (Claude Code, Codex or Kimi Code, remembered in `[lead] agent`) plans it and hands tasks to **workers** from the roster (`[[roster]]`), all headless. Code: `src/panes/agents/{lead,mcp,plan,run,stream,roster,lead_view}.rs`.

### Verified on the dev machine
Flags checked against `--help` of the installed builds, and live where noted.

| | Claude Code 2.1.283 | Codex 0.157.0 | Kimi Code 0.29.1 |
|---|---|---|---|
| headless | `-p --output-format stream-json --verbose` | `exec --json` | `-p <prompt> --output-format stream-json` |
| per-run MCP | `--mcp-config <file>` + `--strict-mcp-config` | `-c mcp_servers.oriel.{command,args,required,startup_timeout_sec,tool_timeout_sec,default_tools_approval_mode}` | no flag: `[mcp_servers]` in config.toml or a project `.kimi-code/mcp.json` |
| lead can't edit | `--permission-mode default --allowedTools Read,Grep,Glob,LS,mcp__oriel__… --disallowedTools Edit,Write,…,Bash,Agent` | `--sandbox read-only` | prompt only (`-p` always runs with auto permissions) |
| caps | `--max-turns` (not in `--help`, but honoured: result subtype `error_max_turns`), `--max-budget-usd` | none, so oriel prices `turn.completed` and tails the rollout file | none, so oriel prices what it can and enforces |
| structured report | `--json-schema`, which arrives as a `StructuredOutput` tool call (live) | `--output-schema <file>` | a ```json block |
| resume | `--session-id <uuid>` up front, then `--resume` | `exec resume <id>` (no `-C`/`-s` there, so `-c sandbox_mode=…` and cwd) | `-S <id>` (with `-p`: **U**) |
| minimal profile | `--exclude-dynamic-system-prompt-sections`, no MCP servers, `Agent` disallowed | `--ignore-user-config -c agents.enabled=false` | `KIMI_CODE_AGENT_SWARM_MAX_CONCURRENCY=2` |

- **No `codex mcp-server` subcommand** in 0.157 (it isn't in `--help`). oriel's own server is `oriel mcp-lead <port> <token>`.
- **Codex over MCP (live, `agents_lead_codex_mcp_live`):** the exact lead command line, through the npm `.cmd` shim, loaded `oriel mcp-lead` and called `roster` through the broker. $0.03 API-equivalent, 10 s. TOML **literal** strings (`'…'`) are what survive cmd.exe quoting.
- **Kimi:** not signed in on the dev machine, so its lead and worker paths are unverified live. A lead whose MCP tools never reach oriel (zero calls) is switched to the text protocol automatically.
- **Claude lead + worker (live, `agents_lead_live_claude_haiku`, haiku):** MCP worked end to end.
  - The first run cost **$1.15 in 7.7 min** because acceptance commands ran in `cmd.exe` while the model wrote bash. The run bounced the task, retried and re-planned 5×.
  - Fixed: gates now run in Git Bash on Windows (never the System32 WSL launcher), else PowerShell, else cmd. The lead is told which shell. A command that fails to *run* (`shell_trouble`) is reported to the lead as `acceptance_broken` and isn't bounced to the worker.
- Each fresh Claude session costs about 30k cache-write tokens of base prompt (~$0.04 on haiku). Fewer, bigger tasks beat many tiny ones.

### How a run works
1. **Setup.** `L` opens a form: goal, lead agent/model, workers at once (1-5), run budget.
   - It creates the integration branch `oriel/lead-<slug>` off the checked-out branch, plus a detached, read-only checkout of it for the lead.
   - The lead runs there with oriel's instructions (Claude: `--append-system-prompt-file`; others at the top of the first prompt).
   - The instructions are byte-stable apart from a closing "THIS RUN" block.
2. **Tools** (MCP, or JSON actions in the text protocol, same handler):

   | Tool | Does |
   |---|---|
   | `roster` | workers, their tier, plan usage, track record, rate-limit pause |
   | `plan` | typed tasks |
   | `wait` | blocks on events |
   | `task_status` | compact cards |
   | `task_diff` | paged diff |
   | `merge` | queues a merge |
   | `send_followup` | more instructions to a finished worker |
   | `resolve_conflicts` | merges the integration branch into a task and hands the markers to its worker |
   | `spawn_task` | adds one task outside a plan |
   | `discard` | throws a task away |
   | `note` | a note on the board (`needs_user` notifies the user) |
   | `done` | finishes the run |

   `wait` sends MCP progress notifications every 25 s. Timeouts: `MCP_TOOL_TIMEOUT=1800000`, Codex `tool_timeout_sec=1800`, Kimi `toolTimeoutMs`.
3. **Plan checks** (`plan.rs`), done mechanically:
   - Every task `owns` globs.
   - Tasks that can run at the same time can't own overlapping files.
   - Hotspot files (manifests, lockfiles, `mod.rs`/`lib.rs`/`index.ts`, migrations) go to one scaffold task, and every other task depends on it.
   - Size S or M, capped by the number of owned files. L is refused.
   - Deps must exist and have no cycles.
   - Solo gate: 2 tasks run one after the other.
   - Tasks start once their deps are merged and a slot is free. Same-model starts are staggered 5 s so the prompt cache warms.
4. **Workers.** Each worker runs in its own worktree off the integration branch. Its prompt is the shared preamble plus a task block (OWNS, ACCEPTANCE and the shell it runs in, SIZE, earlier attempts). Workers never run git; oriel commits.
5. **Merge queue**, one at a time:
   1. Commit the worker's leftovers.
   2. `git merge-tree --write-tree` against the integration tip.
   3. One candidate `commit-tree` with an `Oriel-Task: <id>` trailer.
   4. The gate on that exact tree in a reusable scratch worktree. The gate is `[lead] gate`, or auto-detected (`cargo check --quiet`, `go build ./...`), plus the task's acceptance command.
   5. `update-ref` as a compare-and-swap.
   6. The lead's checkout moves to the new tip.

   If a merge fails:
   - **Conflict:** oriel merges the integration branch into the task's worktree and hands the markers to the same worker session.
   - **Gate failure:** the same session gets the first 20 lines of the failure.
   - Either way it gets 2 fix rounds, then a fresh re-dispatch (premium tier or another vendor, with notes about the earlier attempts), then it's blocked.
6. **Watchdog**, in the same session: a nudge first, then a fresh worker. It fires on:
   - the same call 4×
   - the same failing command 3×
   - 25 calls with no edit (builds and tests excluded)
   - 5 minutes with no output

   A `rate_limit_event` with status `rejected` pauses that vendor until it resets.
7. **Budgets.** The run cap covers the lead plus every worker; no new work starts once it's hit. Per-task caps come from the roster (Claude `--max-budget-usd`; others stopped by oriel). The lead has its own cap (`[lead] budget_usd`).
8. **Finish.** The lead calls `done`, and the run waits for review. The user reviews the integration branch with `d` and squash-merges it with `m` (clean tree, same branch). `x` discards the run. oriel never pushes.
9. **Crash safety.** The plan, task states, session ids (Claude's are chosen up front) and records are saved on every change. After a restart the run shows as stopped. `r` resumes the lead's session with the current board and continues cut-off workers.

**Watching:**
- `w` tiles the lead and its workers side by side, live.
- `enter` opens one agent's full transcript, with diffs and output.
- `t` moves a headless worker into a real terminal tab (`claude --resume` / `codex resume` / `kimi -S`), and the card follows the tab.

**Skipped for now:**
- the cross-vendor reviewer and best-of-2 (research items 9-10)
- tailing Kimi's `wire.jsonl`
- a time-based "spinning" rule
- Windows Job Objects (`taskkill /T` kills the process tree instead)
