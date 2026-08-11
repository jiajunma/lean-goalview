# lean-goalview

A Lean 4 **goal view** for editors that cannot host the official infoview —
built for [Zed](https://zed.dev), works with any plain-LSP editor.

Zed has no webview (its GPUI renderer ships no browser engine), so the
official `@leanprover/infoview` — a React app — cannot run inside it, and
Zed extensions cannot touch the LSP stream to feed one elsewhere. This tool
takes the [lean.nvim](https://github.com/Julian/lean.nvim) approach instead:
the same goal-state data the infoview consumes, rendered as live text.

```
editor ↔ lean-goalview ↔ lake serve
              │
              └→ <project>/.goalview.md   ← keep open in a split;
                                            updates as the cursor moves
```

## How it works

`lean-goalview` is a transparent LSP proxy around `lake serve`:

1. All traffic passes through untouched — the editor sees a normal Lean
   language server.
2. The editor's own `textDocument/documentHighlight` requests (Zed sends one
   whenever the cursor rests on a symbol) reveal the cursor position.
3. On each position change (debounced 120 ms) the proxy asks the server for
   `$/lean/plainGoal` and `$/lean/plainTermGoal` — using string request ids
   (`gv:N`) that cannot collide with the editor's numeric ids — and renders
   tactic state, expected type, and diagnostics into `.goalview.md`,
   written atomically and only on change.

The rendering shows exactly what the infoview's goal panel shows (case
names, hypotheses, `⊢` goals, error messages). What it deliberately does
not attempt: hover cards, expandable sub-terms, and ProofWidgets — those are
JavaScript executed by the browser-based infoview and cannot exist in a text
file. For proofs that need them, use VS Code or lean.nvim.

## Install

```bash
cargo install --path .            # or: cargo build --release && copy the binary
```

### Zed setup

With the [lean4 extension](https://zed.dev/extensions/lean4) installed, add
to `settings.json`:

```json
{
  "lsp": {
    "lean4-lsp": {
      "binary": {
        "path": "/absolute/path/to/lean-goalview",
        "arguments": []
      }
    }
  }
}
```

Open a `.lean` file, then open `.goalview.md` (project root) in a split —
raw or as markdown preview. Add `.goalview.md` to your `.gitignore`.

## Configuration

| Env var | Default | Meaning |
|---|---|---|
| `LEAN_GOALVIEW_FILE` | `<workspace>/.goalview.md` | Output path |
| `LEAN_GOALVIEW_LAKE` | `lake` on PATH, else `~/.elan/bin/lake` | Lake binary |

## License

MIT
