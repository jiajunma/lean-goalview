# lean-goalview

The **official Lean 4 infoview** in a floating native window, for editors that
can't host it themselves (Zed, and any plain-LSP editor).

Zed has no webview and its extensions can't open panels or touch the LSP
stream, so the official `@leanprover/infoview` — a web app — can't live inside
it. lean-goalview runs it *beside* the editor instead: an LSP proxy multiplexes
the infoview onto the same `lake serve`, and a tiny native window (embedded
WebKit, no bundled browser) renders it, always-on-top and following the cursor.

```
Zed ──LSP──> lean-goalview (proxy) ──> lake serve
                  │  HTTP + /ws bridge
                  ▼
        lean-goalview-window (native, embedded WebKit)
                  loads the official @leanprover/infoview
```

## What you get

- The **real** infoview: interactive goals, clickable subterms, ProofWidgets.
- A floating window, auto-opened when you open a Lean file, following the
  cursor. ⌘T toggles always-on-top; ⌘W / Esc hides it.
- Also a fallback plain goal view over hover (`K`) and a live `.goalview.md`.

## Install

```sh
git clone https://github.com/jiajunma/lean-goalview.git
cd lean-goalview && ./install.sh
```

Needs Rust (cargo), Node (npm), and a Lean toolchain (elan). Then point Zed's
Lean language server at the proxy in `settings.json`:

```json
{
  "lsp": { "lean4-lsp": { "binary": { "path": "/absolute/.local/bin/lean-goalview" } } }
}
```

Open a `.lean` file in Zed — the infoview window opens on its own.

## How it works

The proxy is transparent between the editor and `lake serve`. It also:

1. **Serves the built infoview** (`webui/`, a host page implementing the
   official `EditorApi` + a WebSocket transport) as static files.
2. **Bridges `/ws`**: infoview RPC requests are multiplexed onto `lake serve`
   (id-tagged `iv:N`), their responses routed back; the `initialize` result
   becomes `serverRestarted`; server notifications are forwarded; cursor moves
   are pushed. The infoview constructs the Lean RPC calls itself, so the proxy
   only relays `(uri, method, params)` faithfully.

## Configuration

| Env var | Default | Meaning |
|---|---|---|
| `LEAN_GOALVIEW_FILE` | `<workspace>/.goalview.md` | Output path |
| `LEAN_GOALVIEW_LAKE` | `lake` on PATH, else `~/.elan/bin/lake` | Lake binary |

## License

MIT
