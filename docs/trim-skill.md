# trim: Functional Usage Skill

## Overview

`trim` is a codebase context minimization tool. It extracts structural code units (functions, classes, types) via Tree-Sitter, ranks them by intent relevance, and returns a budget-optimized payload. Use it to understand codebases without loading everything.

## Tools Available

| Tool | Purpose | Returns |
|------|---------|---------|
| `trim` (CLI) | Scan codebase, return focused payload | Plain text with `// === file ===` separators |
| `trim_plan` (MCP) | Structured context planning | JSON with metadata + payload |
| `trim_file` (MCP) | Inspect single file structure | JSON array of unit summaries |
| `list_languages` (MCP) | Show supported languages | Markdown table |
| `grep` (CLI) | Search file contents by regex pattern | Matching lines with file paths and line numbers |

## Core Principle: Iterate, Don't Dump

**Wrong:** Call `trim --budget 100000` once, get massive payload, hope for the best.

**Right:** Start broad, read output, narrow down, inspect specific files.

## Scoping: Exclude Irrelevant Directories

Large repos contain directories you don't need (tests, tools, docs, CI). Always exclude them:

```bash
# Exclude common noise directories
trim /repo --intent "error handling" --budget 4000 \
  --ignore "tools/**,evals/**,integration-tests/**,docs/**,scripts/**,*.test.ts,*.spec.ts"

# Exclude node_modules and dist (usually auto-excluded, but explicit is safer)
trim /repo --intent "auth middleware" --budget 4000 \
  --ignore "node_modules/**,dist/**,__tests__/**"
```

**Why this matters:** Without `--ignore`, trim scans everything. A 3000-file repo where only 200 are relevant wastes budget on irrelevant units, diluting the signal. With scoping, the same budget covers more of what you actually need.

## Intent Writing: The Most Important Skill

The `--intent` string determines what trim ranks as relevant. Poor intents return irrelevant code.

### Good Intents (specific, mentions concrete names)
```bash
trim /repo --intent "aboutCommand definition, getVersion function, SlashCommand interface" --budget 4000
trim /repo --intent "FatalError exit codes, isFatalToolError, ToolErrorType enum" --budget 4000
trim /repo --intent "tryParseGithubUrl, git@github.com, SCP URL parsing" --budget 4000
```

### Bad Intents (vague, over broad)
```bash
trim /repo --intent "code" --budget 4000              # Too vague
trim /repo --intent "everything about errors" --budget 4000  # Too broad
trim /repo --intent "the thing that handles user input and sends it to the model" --budget 4000  # Too wordy
```

### Intent Formula
```
<specific class/function names>, <related concepts>, <what you're trying to do>
```

Examples:
- `"BuiltinCommandLoader, allDefinitions array, command registration pattern"`
- `"LegacyAgentProtocol _runLoop, sendMessageStream, agentic loop"`
- `"config.getModel(), model configuration, current model name"`

## Workflow: The 3-Phase Pattern

### Phase 1: Discovery (Budget: 4000-8000)

Start with a broad intent to understand the codebase structure:

```bash
# CLI
trim /path/to/repo --intent "command registration, slash commands" --budget 4000 --why --stats

# MCP
trim_plan(path: "/repo", task: "command registration, slash commands", budget: 4000)
```

**Read the output carefully:**
- Look for `// === file ===` separators to find relevant files
- Check which units were included (Full/Compact/Skeleton)
- Note the matched terms in `--why` output
- If you see mostly Skeleton units, your budget is too low or intent is too broad

### Phase 2: Inspection (Budget: N/A)

After Phase 1, you have candidate files. Now inspect them:

**CLI approach (no MCP needed):**
```bash
# Use grep to read specific file contents
grep -A 50 "export function tryParseGithubUrl" /repo/packages/cli/src/config/extensions/github.ts

# Or read the file directly
cat /repo/packages/cli/src/ui/commands/modelCommand.ts
```

**MCP approach (if available):**
```bash
trim_file(path: "packages/cli/src/services/BuiltinCommandLoader.ts")
```

**What you get from trim_file:**
```json
[
  {
    "name": "BuiltinCommandLoader",
    "kind": "class",
    "signature": "export class BuiltinCommandLoader implements ICommandLoader",
    "lines": "74-130",
    "tokens_full": 1373,
    "references": ["ICommandLoader", "SlashCommand"],
    "calls_count": 5
  }
]
```

### Phase 3: Narrowing (Budget: 4000-8000)

Based on Phase 2, run a more targeted trim with specific names:

```bash
# CLI
trim /path/to/repo --intent "aboutCommand definition, getVersion function" --budget 4000 --deps

# MCP
trim_plan(path: "/repo", task: "aboutCommand definition, getVersion function", budget: 4000)
```

**Use `--deps`** to pull caller/callee dependencies of key functions.

## Supplementary Tool: grep

When trim's structural extraction misses specific patterns (string literals, error messages, enum values, configuration strings), use `grep` to find them directly.

### When to Use grep Instead of trim

| Pattern Type | Use trim? | Use grep? |
|-------------|-----------|-----------|
| Function/class structure | ✓ | |
| Enum values (`NO_SPACE_LEFT`) | Sometimes | ✓ |
| String literals (error messages) | ✗ | ✓ |
| Import paths | ✗ | ✓ |
| Configuration keys | ✗ | ✓ |
| Specific function calls | ✗ | ✓ |
| Regex patterns | ✗ | ✓ |

### grep Syntax

```bash
# Basic pattern search (always scope with --include)
grep "pattern" /repo/packages --include="*.ts"

# Regex with context lines
grep -C 3 "tryParseGithubUrl" /repo/packages --include="*.ts"

# Case-insensitive
grep -i "error.*classification" /repo/packages --include="*.ts"

# Count matches per file (find most relevant files)
grep -c "FatalError" /repo/packages --include="*.ts"

# Find all enum members
grep -A 50 "enum ToolErrorType" /repo/packages/core/src/tools/tool-error.ts
```

### grep Anti-Patterns

```bash
# ❌ Don't: search everything
grep "model" /repo

# ❌ Don't: search without --include
grep "function" /repo/packages

# ❌ Don't: use overly broad patterns
grep ".*" /repo/packages --include="*.ts"

# ✅ Do: scope to directories and file types
grep "config\.getModel()" /repo/packages/cli/src --include="*.ts"
grep -C 3 "ToolErrorType" /repo/packages/core/src/tools --include="*.ts"
```

### Workflow Integration

Use grep **after** trim discovery to fill gaps:

```
Phase 1: trim --intent "error handling" --budget 4000
  → Found error files, but missed specific enum values

Phase 2: grep "ToolErrorType\." /repo/packages/core/src/tools/ --include="*.ts"
  → Found all enum members and their usage

Phase 3: trim --intent "ToolErrorType NO_SPACE_LEFT isFatalToolError" --budget 4000 --deps
  → Got the full error handling chain
```

**Key principle:** trim finds *structure*, grep finds *content*. Use both.

## When trim Returns Sparse Output

If trim returns mostly Skeleton units or very few Full/Compact units:

1. **Your intent is too vague** — add specific class/function names
2. **Your budget is too low** — increase to 6000-8000
3. **Too many irrelevant files** — add `--ignore` patterns
4. **Codebase is huge** — use `--session` to build context across calls

```bash
# Bad: vague intent, small budget → sparse results
trim /repo --intent "errors" --budget 2000

# Good: specific intent, adequate budget → rich results
trim /repo --intent "FatalError hierarchy, exit codes, isFatalToolError" --budget 4000 --why --stats
```

## Key Flags

| Flag | When to Use |
|------|-------------|
| `--why` | Debugging: understand why units were selected/excluded |
| `--stats` | Get compression ratio and token usage summary |
| `--deps` | Pull dependencies of important functions |
| `--session <id>` | Maintain memory across multiple calls |
| `--git-signals` | Boost recently modified files |
| `--ignore <pattern>` | Exclude directories/files (glob syntax) |
| `--no-cache` | Force fresh scan (skip .trim_cache) |

## Budget Guidelines

| Codebase Size | Recommended Budget | Notes |
|---------------|-------------------|-------|
| Small (<100 files) | 2000-4000 | Can fit most relevant code |
| Medium (100-1000 files) | 4000-8000 | Start broad, then narrow |
| Large (1000+ files) | 8000-16000 | May need multiple passes, use `--ignore` |
| Huge (10000+ files) | 16000-32000 | Use `--session` for memory, aggressive `--ignore` |

**Critical:** At 100k budget, trim returns too much. At 10k-20k, it may miss important files in large codebases. The sweet spot is 4k-8k for targeted searches.

## Session Memory

Use `--session <id>` to maintain context across calls:

```bash
# Turn 1: Discover structure
trim /repo --intent "auth middleware" --budget 4000 --session task-1

# Turn 2: Session remembers previous context
trim /repo --intent "jwt validation" --budget 4000 --session task-1

# Turn 3: Build on previous knowledge
trim /repo --intent "token refresh" --budget 4000 --session task-1
```

The session stores a "hot set" of previously-included units and boosts them on subsequent calls.

## Anti-Patterns

### ❌ Don't: Dump 100k tokens
```bash
trim /repo --budget 100000 --intent "explain everything"
```
Returns too much. Agent drowns in context.

### ❌ Don't: Same intent 3 times
```bash
trim /repo --intent "commands" --budget 4000
trim /repo --intent "commands" --budget 4000
trim /repo --intent "commands" --budget 4000
```
No iteration. No narrowing. Wasted calls.

### ❌ Don't: Skip inspection
```bash
trim /repo --intent "aboutCommand" --budget 4000
# Agent reads output but doesn't read the actual file
```
You miss the actual implementation details.

### ❌ Don't: Use trim for string searches
```bash
trim /repo --intent "error message text" --budget 4000
```
trim extracts structure, not string content. Use grep for that.

### ❌ Don't: grep everything first
```bash
grep -r "function" /repo --include="*.ts"
grep -r "class" /repo --include="*.ts"
```
Returns too much noise. Use trim for structure, grep for targeted content.

### ❌ Don't: grep without file scoping
```bash
grep "model" /repo
```
Searches all files including node_modules, test fixtures, lock files. Always scope with `--include` and a directory path.

### ✅ Do: Iterate and narrow
```bash
# 1. Discover
trim /repo --intent "command registration" --budget 4000 --why
# Output shows BuiltinCommandLoader.ts is relevant

# 2. Inspect (read the file or use trim_file)
grep -A 30 "allDefinitions" /repo/packages/cli/src/services/BuiltinCommandLoader.ts
# Found: loadCommands() imports all commands and adds to array

# 3. Narrow
trim /repo --intent "aboutCommand, getVersion" --budget 4000 --deps
# Output shows the specific command implementation
```

### ✅ Do: Combine trim + grep strategically
```bash
# Step 1: trim for structure
trim /repo --intent "error handling, FatalError" --budget 4000 --why
# Found: errors.ts has FatalError hierarchy

# Step 2: grep for specific content
grep "exitCode" /repo/packages/core/src/utils/errors.ts
# Found: exact exit codes (41, 42, 44, 52, 53, 54, 55, 130)

# Step 3: trim for dependencies
trim /repo --intent "FatalToolExecutionError, handleError" --budget 4000 --deps
# Found: how FatalToolExecutionError is used in CLI error handling
```

## Example: Finding and Fixing a Bug

**Task:** Fix the `tryParseGithubUrl` bug where non-GitHub SCP-styled SSH URLs cause errors.

### Step 1: Discover the function
```bash
trim /repo --intent "tryParseGithubUrl, git@github.com, SCP URL, extension install" --budget 4000 --why
```
Found: `github.ts` with `tryParseGithubUrl` function.

### Step 2: Read the function
```bash
grep -A 30 "export function tryParseGithubUrl" /repo/packages/cli/src/config/extensions/github.ts
```
Found: only handles `git@github.com:` prefix, passes other SCP URLs to `URL.parse()` which throws.

### Step 3: Find related code
```bash
grep "tryParseGithubUrl" /repo/packages/cli/src --include="*.ts"
```
Found: callers in `extension-manager.ts` and test file.

### Step 4: Understand the fix needed
The function needs to return `null` for non-GitHub SCP URLs (like `git@gitlab.com:...`) instead of letting them reach `URL.parse()`.

### Step 5: Verify the pattern
```bash
grep -B 5 -A 10 "startsWith.*git@" /repo/packages/cli/src/config/extensions/github.ts
```
Found: the existing `git@github.com:` check pattern to follow.

## MCP vs CLI

| Feature | CLI | MCP |
|---------|-----|-----|
| `trim` | ✓ | ✓ |
| `trim_plan` | ✗ | ✓ |
| `trim_file` | ✗ | ✓ |
| `list_languages` | ✗ | ✓ |
| `--why` | ✓ | ✗ |
| `--stats` | ✓ | ✗ |
| `--deps` | ✓ | ✓ (always on) |
| `--session` | ✓ | ✓ |
| `--ignore` | ✓ | ✗ |

**Use CLI** for quick searches with `--why` debugging and `--ignore` scoping.
**Use MCP** for structured workflows with `trim_plan` and `trim_file`.

## Quick Reference

```bash
# Broad discovery (with scoping)
trim /repo --intent "AUTH middleware" --budget 4000 --why --stats \
  --ignore "tools/**,evals/**,docs/**,*.test.ts"

# Narrow search
trim /repo --intent "jwt validation, token refresh" --budget 4000 --deps

# Session memory
trim /repo --intent "auth" --budget 4000 --session task-1

# Find specific patterns (supplementary)
grep "ToolErrorType\." /repo/packages/core/src --include="*.ts"
grep -C 3 "tryParseGithubUrl" /repo/packages/cli/src --include="*.ts"
grep "config\.getModel()" /repo/packages --include="*.ts"

# Inspect file (MCP only)
trim_file(path: "src/auth.ts")

# Plan with metadata (MCP only)
trim_plan(path: "/repo", task: "fix connection leak", budget: 6000)
```
