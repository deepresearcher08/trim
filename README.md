# trim

`trim` is a zero-configuration command-line utility designed to construct high-density context payloads from source code repositories for Large Language Model (LLM) prompts.

By combining Abstract Syntax Tree (AST) structural parsing, intent-driven semantic relevance scoring, and a two-pass token budget allocation algorithm, `trim` reduces repository context size while maintaining structural integrity.

---

## Architectural & Context Preparation Model

The design of `trim` centers around five formal principles:

### 1. AST-Guided Structural Extraction
Instead of relying on line-based or arbitrary token-window slicing, `trim` uses Tree-Sitter grammars to parse source files into Abstract Syntax Trees. It isolates top-level declarations (functions, structs, classes, interfaces, traits, methods, and constants), ensuring that code units correspond strictly to complete syntactic constructs.

### 2. Explicit Boundary Preservation
Partial line or token truncation often introduces invalid syntax or ambiguous code fragments, which can lead to model hallucinations. When `trim` elides a definition body to conserve tokens, it preserves the complete signature and replaces the body block with an explicit elision comment (for example, `/* ... body elided by trim ... */`). This provides the model with unambiguous structural boundaries.

### 3. Intent-Driven Relevance Scoring
When provided with an intent query (via `--intent`), `trim` evaluates candidate code units for semantic relevance using a dependency-free lexical ranker (BM25-lite algorithm over identifiers, signatures, and docstrings). For advanced neural re-ranking, an opt-in ONNX cross-encoder ranker is supported (`--features onnx`).

### 4. Greedy Two-Pass Budget Allocation
`trim` allocates a target token budget (`--budget`) using a two-pass selection process:
- **Pass 1 (Breadth First):** Admits structural skeletons for all relevant symbols across the target directory up to the budget limit, maximizing broad architectural visibility.
- **Pass 2 (Depth First):** Iterates through units in descending order of relevance score, upgrading skeletonized signatures to full implementations whenever remaining token budget permits.

### 5. Incremental AST Caching
On large repositories, re-parsing every source file on each invocation introduces avoidable overhead. `trim` maintains a `.trim_cache` file that records file metadata (modification time, size) and a SHA-256 content hash for each previously parsed source file. On subsequent runs, files whose metadata matches the cached state are skipped entirely, and their extracted code units are reused directly. Files with changed metadata but identical content (e.g., after a `touch` or branch switch without edits) fall through to a secondary content-hash check. Only genuinely modified or newly created files trigger a full Tree-Sitter parse. Entries for deleted files are purged automatically.

---

## Installation

### Standard Installation (Zero Dependencies)
```bash
cargo install --path .
```

### Installation via Git
```bash
cargo install --git https://github.com/deepresearcher08/trim.git
```

### Installation with ONNX Support
```bash
cargo install --path . --features onnx
```

---

## Usage Examples

### Standard Repository Scanning (Default 8,000 Token Budget)
```bash
trim . --budget 8000
```

### Intent-Driven Selection with Summary Statistics
```bash
trim /path/to/repository --intent "budget allocation algorithm" --budget 4000 --stats
```

### Directing Payload Output to a File
```bash
trim . --intent "tree sitter parsing" --budget 6000 --out context_payload.txt
```

### Disabling or Relocating the Parse Cache
```bash
trim . --no-cache
trim . --cache-file /tmp/my_cache
```

---

## Command-Line Options

```text
Usage: trim [OPTIONS] [PATH]

Arguments:
  [PATH]  Root directory to scan [default: .]

Options:
  -i, --intent <INTENT>        Task description or query driving relevance scoring [default: ]
  -b, --budget <BUDGET>        Target token budget for the generated payload [default: 8000]
  -o, --out <PATH>             Write payload output to a specified file
      --stats                  Print summary statistics (scanned files, included units, token counts) to stderr
      --ranker <RANKER>        Relevance ranker engine: "heuristic" (default) or "onnx" [default: heuristic]
      --model <PATH>           Path to model.onnx (required when using --ranker onnx)
      --tokenizer <PATH>       Path to tokenizer.json (required when using --ranker onnx)
      --max-length <LENGTH>    Maximum sequence length for cross-encoder processing [default: 256]
      --no-cache               Disable incremental caching; re-parse every file from scratch
      --cache-file <PATH>      Path to the cache file [default: .trim_cache in scanned root]
  -h, --help                   Print help information
  -V, --version                Print version information
```

---

## Example Payload Format

```rust
// === ./crates/llm-trim-core/src/budget.rs ===
pub struct BudgetPlan {
    pub budget_tokens: usize,
    pub used_tokens: usize,
    pub included: Vec<PlannedUnit>,
    pub excluded_unit_ids: Vec<usize>,
}

pub fn select_within_budget(
    units: &[CodeUnit],
    scores: &HashMap<usize, f32>,
    budget_tokens: usize,
) -> BudgetPlan {
    /* ... body elided by trim ... */
}
```

---

## Supported Languages

| Language | File Extensions | Tree-Sitter Grammar |
|----------|-----------------|---------------------|
| **Rust** | `.rs` | `tree-sitter-rust` |
| **Python** | `.py`, `.pyi` | `tree-sitter-python` |
| **JavaScript** | `.js`, `.jsx`, `.mjs`, `.cjs` | `tree-sitter-javascript` |
| **TypeScript** | `.ts`, `.mts`, `.cts` | `tree-sitter-typescript` |
| **TSX** | `.tsx` | `tree-sitter-typescript` |
| **Go** | `.go` | `tree-sitter-go` |

## Model Context Protocol (MCP) Server

`trim` includes an MCP server implementation (`trim-mcp`) that exposes codebase minimization tools directly to LLM coding assistants such as Claude Code, Cursor, and Cline over standard I/O.

### Installation

```bash
cargo install --path crates/llm-trim-mcp
```

### Client Configuration

Add `trim-mcp` to your client configuration file (e.g., `claude_desktop_config.json` or `.cursor/mcp.json`):

```json
{
  "mcpServers": {
    "trim": {
      "command": "trim-mcp"
    }
  }
}
```

### Available Tools

| Tool | Description | Parameters |
|------|-------------|------------|
| `trim` | Scans a repository directory, ranks code units by intent relevance, and returns a budget-optimized prompt payload. | `path` (string, required), `intent` (string, optional), `budget` (number, optional, default: 8000), `no_cache` (boolean, optional) |
| `trim_file` | Parses a single source file and returns its structural definitions, signatures, line numbers, and token estimates. | `path` (string, required) |
| `list_languages` | Returns the list of supported programming languages and file extensions. | None |

---

## License

This project is licensed under the terms of the [MIT License](LICENSE).
