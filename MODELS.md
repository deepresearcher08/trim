# ONNX Model Guide for trim

`trim` ships zero-config with a lightweight, dependency-free **heuristic lexical ranker** by default (`--ranker heuristic`).

For enhanced semantic matching, `trim` supports an opt-in **ONNX cross-encoder ranker** (`--ranker onnx`). Because model weights are not bundled (keeping the base installation small and license-clean), you can use any local cross-encoder model exported to ONNX.

---

## Recommended Model

We recommend **`cross-encoder/ms-marco-MiniLM-L-6-v2`** (~22M parameters, sub-10ms inference per candidate on CPU).

### Downloading Pre-Exported Weights

You can export the model using Hugging Face Optimum or Python:

```bash
pip install optimum[onnxruntime] transformers
optimum-cli export onnx --model cross-encoder/ms-marco-MiniLM-L-6-v2 ./onnx-model/
```

This generates `model.onnx` and `tokenizer.json` inside `./onnx-model/`.

---

## Running trim with ONNX

First, ensure `trim` was installed/built with the `onnx` feature:

```bash
cargo build --release --features onnx
```

Then invoke `trim` pointing to your exported model and tokenizer:

```bash
trim . \
  --intent "how does budget selection work" \
  --budget 4000 \
  --ranker onnx \
  --model ./onnx-model/model.onnx \
  --tokenizer ./onnx-model/tokenizer.json
```
