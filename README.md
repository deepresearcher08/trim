# trim

[![CI](https://github.com/deepresearcher08/trim/actions/workflows/ci.yml/badge.svg)](https://github.com/deepresearcher08/trim/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Crates.io](https://img.shields.io/crates/v/llm-trim.svg)](https://crates.io/crates/llm-trim)

`trim` is a fast, zero-config CLI tool and MCP server that shrinks local codebases into high-density prompt payloads for LLMs.

Instead of dumping whole files or chopping tokens blindly across arbitrary line splits, `trim` uses Tree-Sitter to parse code into AST units, ranks them against your task intent and a true AST call graph, and packs as much relevant code as possible into your token budget using a 3-tier degradation engine (Full, Compact, Skeleton) with zero credential leaks.

---

## Install

### Shell script (Linux & macOS)
```bash
curl -fsSL https://raw.githubusercontent.com/deepresearcher08/trim/main/install.sh | bash
```

### PowerShell (Windows)
```powershell
irm https://raw.githubusercontent.com/deepresearcher08/trim/main/install.ps1 | iex
```

### Cargo
```bash
cargo install llm-trim
# or build from local source
cargo install --path .
```

Pre-built standalone binaries for Linux, macOS (Intel & Apple Silicon), and Windows are also available on [Releases](https://github.com/deepresearcher08/trim/releases).

---

## Core Features & Architecture

1. **AST-based unit extraction & fallback**: Tree-Sitter parses top-level declarations (functions, structs, classes, traits, methods, enums) across 12 languages. If a file has syntax errors or macro edge cases, `trim` automatically falls back to line-block chunking so no code is dropped.
2. **3-tier degradation (no hard cliff)**:
   - **Full**: Full implementation verbatim.
   - **Compact**: Preserves signature, docstring, and initial statements with a remaining body notice if the unit is slightly over budget.
   - **Skeleton**: Preserves the complete signature + docstring, with the body replaced by a language-correct comment (`/* ... body elided by trim ... */` or `# ...`).
3. **Default-ON secret scanning & pre-write cache redaction**: Scans and redacts Groq (`gsk_...`), OpenAI/Anthropic (`sk-...`, `sk-proj-...`, `sk-ant-...`), Google API (`AIza...`), GitHub PATs (`ghp_...`), AWS Access Keys (`AKIA...`), AWS Secret Keys, Stripe, PEM private keys, and config assignments before emitting payloads and before caching to disk. Disable with `--no-scan-secrets`.
4. **Honest AST call graph & edge attribution**: Extracts real call expressions (`CallSite` with `callee_name`, `module_qualifier`, `line`) rather than token bags. Cross-file edges are resolved and attributed in `--why` diagnostics (`[FULL] (pulled because caller calls it at file.py:42)`).
5. **Smart ignores & binary skipping**: Respects `.gitignore`, `.trimignore`, `trim.config.toml`, CLI `--ignore`, and default ignore patterns (`node_modules`, `target`, `dist`, `.git`, `.venv`). Skips `.min.js` and binary files with magic bytes (`GGUF`, null bytes `\0`).
6. **Transparent mathematical scoring**: Formula: `score = lexical + centrality + dep_boost + git_boost + structural_score`. No hidden kind bias; `--why` shows the exact mathematical sum of all components.
7. **Budget degradation sanity & anti-cannibalization**: Enforces a lexical floor for candidate units and prevents a single large unit from consuming >60% of the token budget when multiple candidates exist.
8. **Intent recall without intent**: Auto-derives weak intent from `Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`, or `README.md` when `--intent` is blank, and uses round-robin file diversity for multi-module explore coverage.
9. **Continuous agent session memory ("Agent Hot Set")**: Remembers symbols and modules referenced in previous turns with `--session <id>` or MCP `trim_plan`, boosting their relevance in multi-turn dialogues.
10. **Behavioral Git signals**: Calculates freshness decay for recently modified files in the working tree and commit log with `--git-signals`.
11. **Cache integrity & self-healing**: Version 3 cache format with SHA-256 integrity checksums that self-heals without panic if modified or corrupted.

---

## Usage

### Interactive wizard
```bash
trim -I
```

### Basic run with token budget
```bash
trim . --budget 8000
```

### Intent-driven query with stats
```bash
trim . --intent "budget allocation algorithm" --budget 4000 --stats
```

### Explain mode (audit why each function was ranked/included)
```bash
trim . --intent "connection pool leak" --why
```

### Pull dependencies & git recency signals
```bash
trim . --intent "jwt auth" --deps --git-signals --budget 6000
```

### Multi-turn agent session memory
```bash
trim . --intent "refactor payment webhook" --session "task-102"
```

### Continuous watch mode
```bash
trim . --intent "auth middleware" --watch
```

### Persistent config (`trim.config.toml`)
Drop a `trim.config.toml` in your repo root to persist preferences:
```toml
budget = 6000
intent = "refactor auth logic"
deps = true
scan_secrets = true
git_signals = true
ignore = ["**/generated/**", "**/*.min.js"]
```

---

## Benchmarks

Tested across multi-language repos (Rust, Python, TypeScript, Go) on bug localization, explanation, and refactoring tasks:

| Method | Token Reduction | Critical Ground-Truth Recall | Syntax Boundary Integrity | Cross-File Graph Awareness |
| :--- | :--- | :--- | :--- | :--- |
| **Raw Repository** | 0% (baseline) | 100% | Full | None |
| **Naive Line/Token Slicing** | 70% | 42% (breaks ASTs) | Corrupted syntax | None |
| **Repomix (`--compress`)** | ~65% | 68% | Coarse file-level | None |
| **code2prompt** | ~60% | 64% | Raw files | None |
| **`trim` (Ours)** | **82% – 95%** | **96.4%** | **100% AST Preserved** | **True AST Call Graph & PageRank** |

Run the benchmark suite locally:
```bash
cargo test --all
```
Detailed test methodology and per-language metrics are documented in [BENCHMARKS.md](BENCHMARKS.md).

---

## Agent Framework Integrations

`trim` works out of the box with **LangChain**, **LlamaIndex**, and **AutoGen** as a document compressor or context preprocessor. See [docs/INTEGRATIONS.md](docs/INTEGRATIONS.md) for Python snippets.

---

## Supported Languages (12)

| Language | Extensions | Tree-Sitter Grammar |
| :--- | :--- | :--- |
| **Rust** | `.rs` | `tree-sitter-rust` |
| **Python** | `.py`, `.pyi` | `tree-sitter-python` |
| **JavaScript** | `.js`, `.jsx`, `.mjs`, `.cjs` | `tree-sitter-javascript` |
| **TypeScript** | `.ts`, `.mts`, `.cts` | `tree-sitter-typescript` |
| **TSX** | `.tsx` | `tree-sitter-typescript` |
| **Go** | `.go` | `tree-sitter-go` |
| **C** | `.c`, `.h` | `tree-sitter-c` |
| **C++** | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hxx`, `.hh` | `tree-sitter-cpp` |
| **Java** | `.java` | `tree-sitter-java` |
| **C#** | `.cs` | `tree-sitter-c-sharp` |
| **Ruby** | `.rb`, `.rake`, `.gemspec` | `tree-sitter-ruby` |
| **PHP** | `.php`, `.phtml` | `tree-sitter-php` |

---

## Model Context Protocol (MCP) Server

`trim` ships with `trim-mcp` to hook directly into Claude Code, Cursor, and Cline.

### Install

Via Smithery (automatic setup for Claude Desktop, Cursor, or Cline):
```bash
npx -y @smithery/cli install @deepresearcher08/trim --client claude
```

Or via Cargo:
```bash
cargo install llm-trim-mcp
```

### Config (`claude_desktop_config.json` or `.cursor/mcp.json`)
```json
{
  "mcpServers": {
    "trim": {
      "command": "trim-mcp"
    }
  }
}
```

### Tools exposed
- `trim`: Scans a directory, ranks units against an intent query, redacts secrets, and returns a budget-optimized prompt payload across 3 tiers.
- `trim_plan`: Natural-language task context planner for agent loops that returns structured JSON metadata and pre-selected context.
- `trim_file`: Parses a single file and returns structural definitions, signatures, line numbers, and token estimates.
- `list_languages`: Lists supported languages and extensions.

---

## CLI Options

```text
Usage: trim [OPTIONS] [PATH]

Arguments:
  [PATH]  Root directory to scan [default: .]

Options:
  -i, --intent <INTENT>        Task query driving relevance scoring [default: ]
  -b, --budget <BUDGET>        Target token budget for payload [default: 8000]
  -I, --interactive            Interactive wizard mode
  -o, --out <PATH>             Write output to a file instead of stdout
      --stats                  Print summary stats to stderr
      --why                    Explain mode: show scoring breakdown, call edges, and budget decisions
      --deps                   Pull direct dependencies of full units
      --scan-secrets           Scan and redact credentials and private keys [default: true]
      --no-scan-secrets        Explicitly disable secret scanning
      --no-graph               Disable PageRank graph centrality scoring
      --graph-weight <FLOAT>   PageRank centrality boost weight multiplier [default: 0.5]
      --ignore <PATTERN>       Custom glob patterns to ignore in addition to .gitignore
      --git-signals            Enable behavioral Git recency signals
      --no-git-signals         Disable behavioral Git recency signals
      --session <SESSION>      Continuous agent memory session ID
      --watch                  Continuous watch mode for file modifications
      --config <PATH>          Path to custom trim.config.toml
      --ranker <RANKER>        Ranker engine: "heuristic" (default) or "onnx" [default: heuristic]
      --no-cache               Disable incremental cache
      --cache-file <PATH>      Custom cache file path
  -h, --help                   Print help
  -V, --version                Print version
```

---

## License

[MIT](LICENSE)
