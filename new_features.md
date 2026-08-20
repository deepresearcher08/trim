# trim — Required New Features

Priorities: P0 = blocks trust/security, P1 = core capability gap, P2 = polish/game-changer.

---

## P0 — Security & Trust

### 1. Secret scanning (critical — currently leaks credentials)
- **Problem:** At high budgets trim dumps live API keys, tokens, and private keys straight into the prompt. Verified: a 100k-budget run on `bob` leaked a Groq key + 3 Gemini keys. The CLI already has a `--scan-secrets` flag but the **MCP server does not expose it**, and it is off by default.
- **Fix:**
  - Expose `scan_secrets` in the MCP tool schema (mirror the CLI flag).
  - Default to ON, with an opt-out — never default to off for credential-bearing repos.
  - Match patterns: `AIza…`, `gsk_…`, `sk-…`, `ghp_…`, `AKIA…`, PEM blocks, `api_key =`, generic `key=…` in config files.
  - Redact value → `[REDACTED]` rather than silently dropping the unit (drops hide important code).
  - Emit a scan report: file + line + pattern class + count, to stderr / MCP log.
- **Verify:** `trim --scan-secrets -b 100000` on a repo with planted keys returns zero raw matches.

### 2. Honest dependency graph (currently identifier-overlap, not a call graph)
- **Problem:** `.trim_cache` "references" are identifiers mentioned in the body (`.cuda.memory_allocated()` → `["cuda","memory_allocated"]`), NOT units the function calls. `--deps` therefore "works" only via token-overlap coincidence. This is marketing-as-feature.
- **Fix:**
  - Build a true call graph from Tree-Sitter: resolve call expressions to unit definitions across files (name + module + import-aware).
  - `--why` must show actual edges: `A calls B (edge: fileA.py:42)`.
  - `deps=true` should only pull units connected by real edges.
  - Cache must store real edge lists, not token bags.
- **Verify:** `trim --deps --why` shows a unit marked "pulled because X calls it at line N" — reproducible, not coincidental.

---

## P1 — Core Capability Gaps

### 3. Gitignore / ignore-file support (blocks real-world use)
- **Problem:** Node_modules-heavy and binary-laden trees time out. Verified: `subetex` (23k files) and the full `projects` folder (7GB) both timed out. trim ignores `.gitignore`, so it parses junk that repomix skips.
- **Fix:**
  - Respect `.gitignore`, `.trimignore`, and `trim.config.toml` exclude globs.
  - Default ignore set: `node_modules`, `.git`, `dist`, `build`, `venv`, `.venv`, `__pycache__`, `.pytest_cache`, `*.min.js`.
  - Skip binary files by magic bytes before Tree-Sitter parse (the 3.9GB `relic.gguf` should cost nothing).
  - Report skipped files/dirs + reason in `--stats`.
- **Verify:** full `projects` folder completes within budget/time; `--stats` lists what was skipped.

### 4. Score transparency & honest ranking (currently a hidden +2.5 function bias)
- **Problem:** `--why` reveals functions get a ~+2.5 bonus and classes ~0, at identical centrality. With empty intent, helper functions outrank config classes. Selection is therefore skewed and unexplained.
- **Fix:**
  - One scoring formula: `score = w_lex * lexical + w_central * centrality + w_sig * structural_signal`, all weights explicit.
  - Remove the implicit kind-bonus, or make it a named, documented signal ("function_likelihood").
  - Every unit's `--why` line must total exactly to its score (today they don't).
- **Verify:** two units with equal centrality but different kinds produce near-equal scores.

### 5. Budget-degradation sanity at high `graph_weight`
- **Problem:** `graph_weight=3` + small budget returned **1 unit** using 99% of budget. High centrality cannibalizes the entire allocation.
- **Fix:**
  - Cap centrality contribution per unit.
  - Reserve a floor: always include at least N highest-lexical units even when centrality dominates.
  - Warn when >60% of budget is consumed by a single unit.
- **Verify:** `graph_weight=3, budget=1200` returns several units, not one.

### 6. Intent recall without intent
- **Problem:** With empty intent, `Matched terms: [none]` for everything and lexical = 0 — selection is pure centrality. A generic "explore" mode should still surface a diverse, structurally informative slice.
- **Fix:**
  - Empty-intent fallback: coverage-first selection (one high-value unit per file/module before repeats).
  - Optionally auto-derive a weak intent from repo name, README, or package manifest.
- **Verify:** empty-intent run on `optimisedLLM` includes config classes AND core functions, not just functions.

---

## P2 — Game-Changers

### 7. MCP parity & loop-integration
- **Problem:** The MCP server is a thin, weaker wrapper: no secret scan, no ignore support, no `--why`/`--stats`, no config file. The CLI is the real tool.
- **Fix:**
  - MCP must expose every CLI flag.
  - Add streaming/partial results so large scans don't hit MCP timeouts.
  - Add a `trim.plan(task)` tool: accepts a natural-language task, returns pre-selected context for the agent loop.
- **Verify:** run the same scan through MCP and CLI — identical payload and options.

### 8. Continuous / incremental selection (the "agent memory" model)
- **Problem:** trim is one-shot. It can't track relevance across a session or over time.
- **Fix:**
  - Session state: keep a budgeted "hot set" that the agent can top-up instead of re-scanning.
  - Watch mode: on file change, invalidate only the affected units in `.trim_cache` (already mtime-keyed — extend to directory watches).
- **Verify:** edit one file; re-trim reflects only that delta.

### 9. Behavioral relevance signals
- **Problem:** Selection ignores what actually matters — what you're working on.
- **Fix:**
  - Git-aware ranking: recently-committed/recently-touched units rank higher (weight by commit recency).
  - Co-edit correlation: files edited together historically cluster and rank together.
  - Test/coverage signal: tested code ranks higher for "explain" intents.
- **Verify:** `git log` freshness measurably reorders output.

### 10. Fix the cache-file trust problem
- **Problem:** `.trim_cache` (302KB JSON) was flagged as "suspicious" by repomix's secret scan; it stores full unit text and is easy to corrupt/go stale.
- **Fix:**
  - Version + checksum the cache, self-heal on mismatch.
  - Store it in the OS cache dir or honor an explicit `--cache-file` everywhere.
  - Never include literal credential values in cache text (redact at cache-write time, before leak by tools that ingest the tree).
- **Verify:** cache survives a partial write; no secrets appear in cache.

---

## Suggested delivery order
1. **P0-1 secrets** (security) → 2. **P1-3 ignores/binaries** (scale) → 3. **P0-2 real call graph** (trust) → 4. **P1-4 honest scores** → 5. **P1-5 budget floor** → 6. **P2-7 MCP parity** → 7. **P2-8/9/10** (game-changers).