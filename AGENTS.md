# DeepTutor — Agent-Native Architecture

## Overview

DeepTutor is an **agent-native** intelligent learning companion organized
around two plugin layers: single-shot **Tools** invoked by the model and
multi-stage **Capabilities** that own a turn. The desktop product is a
standalone **Tauri 2 + Rust Axum** client. It does not import, launch, bundle,
or depend on Python or Node.js at runtime.

The durable turn application service persists and replays turns before the
shared turn engine routes a normalized context to the selected capability.
All desktop HTTP, WebSocket, authentication, file, settings, update and
plugin services are implemented in Rust. Node.js is used only by developers
to compile the Vite frontend; its output is static and does not enter the
runtime as a Node server.

See [Desktop Tauri](docs-for-dev/DESKTOP_TAURI.md) for the portable layout,
WASM plugin ABI, build commands and security boundaries.

## Architecture

```text
Entry point: Tauri WebView
                    ↓
          Rust Axum loopback service
                    ↓
          TurnApplicationService
       persists, coordinates, replays
                    ↓
          TurnEngine / ChatOrchestrator
       routes UnifiedContext → Capability
                    ↓
       ToolRegistry + CapabilityRegistry
```

Each execution uses a per-turn stream bus. The orchestrator emits events,
the turn runtime persists them, and adapters replay them to consumers.
Runtime settings live under the installation directory; project-root `.env`
files are intentionally ignored.

## Portable Storage

`src-tauri/src/portable.rs` resolves every runtime path relative to the
installation directory:

| Path | Purpose |
| --- | --- |
| `data/` | SQLite, settings, knowledge, uploads, outputs, plugins |
| `cache/` | WebView2, APPDATA/LOCALAPPDATA and XDG caches |
| `temp/` | temporary downloads, extraction and subprocess work |
| `home/` | portable `HOME` and `USERPROFILE` |
| `logs/` | `desktop.log` |

Release mode uses the executable directory as the root. Development mode
uses the repository root. `DEEPTUTOR_PORTABLE_ROOT` can override both. The
application fails fast if the root is not writable instead of falling back
to the C-drive user profile.

## Level 1 — Tools

Single-function tools the LLM picks on demand. Eight user-toggleable tools
surface in `/settings/tools`:

| Tool | Description |
| --- | --- |
| `brainstorm` | Breadth-first idea exploration with rationale |
| `web_search` | Web search with citations |
| `paper_search` | arXiv preprint search |
| `zotero_search` | Search a configured Zotero library |
| `reason` | Dedicated deep-reasoning model call |
| `geogebra_analysis` | Analyze math images into GeoGebra commands |
| `imagegen` | Generate images |
| `videogen` | Generate videos |

Other built-ins are context-gated or capability-owned, including `rag`,
memory and notebook tools, `read_skill`, deferred MCP/CLI tools, `exec`,
`ask_user`, mastery navigation, `cron` and workspace tools.

`exec` is disabled by default. It runs only user-installed system runtimes in
an installation-directory workspace with a scrubbed environment, output and
time limits. It is process isolation, not a complete OS filesystem sandbox.

## Level 2 — Capabilities

| Capability | Stages |
| --- | --- |
| `chat` | exploring → responding |
| `ask_questions` | responding through `ask_user` |
| `audio_overview` | source-grounded audio overview generation |
| `deep_solve` | responding with solve planning tools |
| `deep_question` | ideation → generation |
| `deep_research` | rephrasing → decomposing → researching → reporting |
| `visualize` | analyzing → generating → reviewing |
| `math_animator` | concept analysis → design → SVG/CSS generation → review |
| `mastery_path` | guided mastery chat loop |
| `immersive_reading` | document- or timestamp-grounded reading loop |
| `video_lesson` | code generation → render check → self-contained HTML lesson |

All capabilities emit the same response and cost envelope. Status copy and
prompts are localized through embedded prompt assets.

## Plugin Model

Portable plugins declare `deeptutor.plugin/v1` and can expose:

- `static:<relative>` read-only files
- `json:<relative>` declarative JSON responses
- `wasm:<relative>.wasm` restricted WebAssembly HTTP handlers

The WASM runtime allows no host imports and exposes no filesystem, network,
process, environment or system capability. Requests, responses, memory, fuel
and wall-clock time are bounded. Python modules and wheel plugins are not
accepted.

## Key Files

| Path | Purpose |
| --- | --- |
| `src-tauri/src/lib.rs` | Tauri lifecycle and desktop commands |
| `src-tauri/src/portable.rs` | Installation-directory path and environment policy |
| `src-tauri/src/storage.rs` | SQLite persistence |
| `src-tauri/src/server/mod.rs` | Rust Axum service and router composition |
| `src-tauri/src/server/engine.rs` | Turn engine, capabilities and streaming results |
| `src-tauri/src/server/tool_policy.rs` | Tool composition and capability gates |
| `src-tauri/src/server/tools.rs` | Built-in tool definitions and dispatch |
| `src-tauri/src/server/plugin_wasm.rs` | Restricted WASM plugin runtime |
| `src-tauri/src/server/plugins.rs` | Plugin lifecycle, pages and routes |
| `src-tauri/src/server/cron.rs` | Persistent scheduling |
| `src-tauri/src/server/exec_tool.rs` | Optional external runtime execution |
| `src-tauri/src/update.rs` | Portable download, verification, rollback |
| `web/vite.desktop.config.ts` | Static desktop frontend build |
| `scripts/build_portable.ps1` | Standalone release assembly and ZIP audit |

## Build And Test

```powershell
cd src-tauri
cargo fmt --all
cargo check --locked
cargo test --locked
cd ..
npm --prefix web ci --legacy-peer-deps
npm --prefix web run build:desktop
.\scripts\audit_openapi_routes.ps1
.\scripts\build_portable.ps1
```

The release must contain the Rust executable and `resources/web` only, plus
optional WebView2 fixed runtime and licenses. It must not contain Python,
Node.js, `node_modules`, `.py/.pyc/.pyd`, Next.js, Remotion or user data.

### 编译交付约定

- 每次重新编译并交付 EXE，必须同步更新便携 ZIP 和对应的 `.sha256`。
- 统一使用 `.\scripts\build_portable.ps1`；前端未改动时可加
  `-SkipFrontend`，交付时不得使用 `-SkipZip`，也不得仅复制 Cargo 输出。
- 完成后运行 `verify_portable.ps1`，核对 ZIP 与发行目录中的 EXE 哈希一致；
  更新现有便携目录时必须保留用户的 data、cache、temp、logs 和 home。
