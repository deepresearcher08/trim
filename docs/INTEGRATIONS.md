# Agent Framework Integrations

`trim` can be used directly as a sub-process, library bridge, or MCP server within popular LLM agent frameworks.

---

## 1. LangChain (Python)

Create a custom `TrimContextCompressor` to minimize retrieved repository documents before passing them to the LLM prompt:

```python
import subprocess
from typing import Sequence
from langchain_core.documents import Document
from langchain.retrievers.document_compressors.base import BaseDocumentCompressor

class TrimContextCompressor(BaseDocumentCompressor):
    budget: int = 4000
    repo_path: str = "."

    def compress_documents(
        self,
        documents: Sequence[Document],
        query: str,
    ) -> Sequence[Document]:
        cmd = [
            "trim",
            self.repo_path,
            "--intent", query,
            "--budget", str(self.budget),
            "--deps"
        ]
        result = subprocess.run(cmd, capture_output=True, text=True, check=True)
        return [Document(page_content=result.stdout, metadata={"source": "trim_minimized"})]
```

---

## 2. LlamaIndex (Python)

Create a custom `TrimNodePostprocessor` to condense retrieved nodes into a structured AST payload:

```python
import subprocess
from typing import List, Optional
from llama_index.core.schema import NodeWithScore, TextNode
from llama_index.core.postprocessor.types import BaseNodePostprocessor

class TrimNodePostprocessor(BaseNodePostprocessor):
    budget: int = 6000
    repo_path: str = "."

    def _postprocess_nodes(
        self,
        nodes: List[NodeWithScore],
        query_str: Optional[str] = None
    ) -> List[NodeWithScore]:
        intent = query_str or ""
        cmd = ["trim", self.repo_path, "--intent", intent, "--budget", str(self.budget)]
        res = subprocess.run(cmd, capture_output=True, text=True, check=True)
        return [NodeWithScore(node=TextNode(text=res.stdout), score=1.0)]
```

---

## 3. AutoGen Context Helper

Minimize entire code directories before tool execution in multi-agent workflows:

```python
import subprocess

def get_codebase_context(intent: str, budget: int = 8000, path: str = ".") -> str:
    """Invokes trim to produce an AST-minimized context payload for LLM agents."""
    result = subprocess.run(
        ["trim", path, "--intent", intent, "--budget", str(budget), "--scan-secrets"],
        capture_output=True,
        text=True,
        check=True
    )
    return result.stdout
```

---

## 4. MCP Clients (Claude Code, Cursor, Windsurf, Cline)

Configure `trim-mcp` in your IDE/assistant MCP configuration:

### Cursor (`.cursor/mcp.json`) & Claude Desktop (`claude_desktop_config.json`)
```json
{
  "mcpServers": {
    "trim": {
      "command": "trim-mcp"
    }
  }
}
```

### Claude Code CLI
```bash
claude mcp add trim trim-mcp
```
