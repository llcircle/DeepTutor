# Tauri 纯 Rust 便携桌面版

[开发手册](README.md) · [架构](ARCHITECTURE.md) · [数据存储](DATA_STORAGE.md)

DeepTutor 桌面版以 **Tauri 2 + Rust Axum** 构建为独立客户端。桌面运行时
只包含 Rust 可执行文件、`WebView2Loader.dll`、Vite 生成的静态前端和可选
WebView2 固定运行时，
不依赖或启动 Python、Node.js、`node_modules`、Next.js、Remotion 或 Python
wheel 插件。

## 运行架构

```text
Tauri WebView
    │ 同源 HTTP / WebSocket
    ▼
Rust Axum 本地服务（127.0.0.1 随机端口）
    ├─ TurnApplicationService：回合持久化、恢复与事件重放
    ├─ TurnEngine / ChatOrchestrator：11 类内置能力与统一流式事件
    ├─ Tool / Capability / Module registries
    ├─ 知识库、阅读、记忆、Notebook、Partners、日历等应用服务
    ├─ SQLite、文件上传、文档解析、更新与定时任务
    ├─ 无宿主能力的 WASM 插件运行时
    └─ 可选外部 MCP/CLI、文档解析器与 `exec`
```

桌面窗口只访问随机 loopback 端口上的单一同源服务。Rust 后端直接实现
HTTP、WebSocket、流式事件、认证、多用户作用域和文件接口，不再通过
Python/Node 网关转发。

## 功能覆盖

桌面客户端保留 v1.7.0 的产品能力入口，并在 Rust 中实现：

- `chat`、`ask_questions`、`audio_overview`、`deep_solve`、`deep_question`
- `deep_research`、`visualize`、`math_animator`、`mastery_path`
- `immersive_reading`、`video_lesson`
- 用户工具、上下文工具、MCP、CLI Apps、Skills 与插件管理
- 知识库、解析、阅读、记忆、Notebook、Co-Writer、Books、题库、日历
- Partners、多用户认证、语音、任务板、设置、会话、用量与学习分析
- 定时任务、便携更新、回滚和启动状态恢复

数学动画输出为浏览器原生 SVG/CSS/SMIL；视频课件输出为自包含 HTML，
不再依赖 Manim、Python、Remotion 或 Node 构建服务。

## 便携目录

所有应用状态相对可执行文件所在目录解析：

```text
<安装目录>/
├── DeepTutor.exe
├── WebView2Loader.dll    # Tauri WebView2 启动必需的原生加载器
├── resources/web/          # Vite 静态资源
├── resources/webview2-runtime/  # 可选固定运行时
├── data/
│   ├── deeptutor.sqlite3   # 会话、设置、任务、插件等持久数据
│   ├── settings/
│   ├── knowledge/
│   ├── uploads/
│   ├── outputs/
│   ├── plugins/
│   └── user/update/        # 更新缓存、下载、备份与工作状态
├── cache/
│   ├── appdata/roaming/    # APPDATA
│   ├── appdata/local/      # LOCALAPPDATA
│   ├── xdg/                # XDG config/cache/data/state
│   └── webview2/           # WebView2 用户数据和浏览器缓存
├── home/                   # HOME/USERPROFILE
├── temp/                   # TMP/TEMP/TMPDIR
└── logs/desktop.log
```

`src-tauri/src/portable.rs` 在初始化日志、数据库和 WebView 前设置：

- `DEEPTUTOR_HOME`、`DEEPTUTOR_CACHE_ROOT`、`DEEPTUTOR_DATABASE`
- `HOME`、`USERPROFILE`、`APPDATA`、`LOCALAPPDATA`
- `XDG_CONFIG_HOME`、`XDG_CACHE_HOME`、`XDG_DATA_HOME`、`XDG_STATE_HOME`
- `TMP`、`TEMP`、`TMPDIR`
- `WEBVIEW2_USER_DATA_FOLDER`、`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`

发行模式以可执行文件目录为便携根；开发模式以仓库根目录为便携根。
可用 `DEEPTUTOR_PORTABLE_ROOT` 显式指定根目录。根目录不可写时程序会
停止并显示具体路径，不静默回退到 C 盘用户目录。

SQLite WAL、日志、上传、解析缓存、生成文件、插件包、更新下载与
WebView2 状态均位于上述目录。测试代码可以使用系统临时目录创建一次性
隔离夹具，但不会进入正式运行链路。

## 插件运行时

便携插件安装包必须声明 `deeptutor.plugin/v1` 清单。HTTP 路由支持：

- `static:<relative>`：只读静态文件
- `json:<relative>`：声明式 JSON 响应
- `wasm:<relative>.wasm`：受限 WebAssembly 处理函数

WASM ABI：

```text
(memory (export "memory") ...)
(func (export "alloc") (param len i32) (result i32) ...)
(func (export "deeptutor_route")
  (param req_ptr i32 req_len i32 out_ptr i32 out_capacity i32)
  (result i32))
```

HTTP 路由请求与响应是 JSON。请求正文使用 `body_base64`；响应可返回
`body` 或 `body_base64`，并仅允许白名单响应头。

WASM 模块不允许任何 host import，不提供文件、网络、进程、环境变量或
系统调用能力。每次请求独立实例化，并限制：

| 项目 | 限制 |
| --- | ---: |
| 模块大小 | 16 MiB |
| 请求大小 | 2 MiB |
| 响应大小 | 4 MiB |
| 线性内存 | 32 MiB |
| 执行燃料 | 10,000,000 |
| 墙钟时间 | 5 秒 |

插件入口不再接受 Python 模块或 wheel。插件页面使用严格 CSP，资源路径
经过扩展名、目录穿越和符号链接校验。

### WASM Tool 契约

Tool 请求：

```json
{
  "version": 1,
  "kind": "tool",
  "plugin_id": "demo.plugin",
  "plugin_version": "1.0.0",
  "extension_id": "echo_tool",
  "tool_name": "demo_plugin_echo_tool",
  "arguments": {"message": "你好"},
  "context": {"turn_id": "turn-id", "session_id": "session-id", "language": "zh"}
}
```

Tool 响应：

```json
{
  "content": "工具结果",
  "metadata": {},
  "success": true,
  "terminate_turn": false
}
```

`content` 必须是字符串，`metadata` 必须是 JSON 对象。已批准并启用的
Tool 会进入模型 deferred definitions（延迟工具定义），由 Rust 工具分发器
执行。

### WASM Capability 契约

`capability` 和 `loop_capability` 共用回合级执行协议；请求中的 `kind`
分别取 `capability` 或 `loop_capability`。

```json
{
  "version": 1,
  "kind": "capability",
  "plugin_id": "demo.plugin",
  "plugin_version": "1.0.0",
  "extension_id": "demo_capability",
  "capability": "demo_capability",
  "context": {
    "session_id": "session-id",
    "turn_id": "turn-id",
    "language": "zh",
    "user_message": "请分析",
    "config": {},
    "knowledge_bases": [],
    "attachments": []
  }
}
```

响应：

```json
{
  "response": "最终正文",
  "events": [
    {"type": "progress", "content": "处理中", "metadata": {}}
  ],
  "result": {},
  "attachments": []
}
```

事件类型仅允许 `progress`、`content`、`thinking`、`stage_start`、
`stage_end`、`artifact`、`workspace_item`、`source`。`result` 必须是 JSON
对象，`attachments` 最多 128 项；缺少 `result.response` 时由宿主补入最终正文。

### Reading Extension 契约

阅读扩展的 `parameters` 必须声明：

```json
{
  "id": "plugin_explain",
  "protocol_version": "1",
  "actions": [
    {
      "id": "explain",
      "label": "解释",
      "trigger": "toolbar",
      "requires": ["selection"]
    }
  ],
  "result_types": ["card"]
}
```

动作请求使用 `kind: "reading_extension"`，上下文包含 `material_id`、
`locator`、`locale`、经过材料原文校验的 `selection` 和截断后的
`visible_text`。动作响应为：

```json
{
  "type": "card",
  "title": "标题",
  "message": "正文",
  "payload": {}
}
```

结果类型支持 `card`、`quiz`、`feedback`、`browser_speech`。`quiz` 必须
返回 1 到 20 道带 `prompt` 和 2 到 8 个字符串选项的题目；`browser_speech`
必须返回非空且不超过 60,000 字符的 `payload.text`。

### 扩展 readiness

| 扩展类型 | Rust 桌面状态 |
| --- | --- |
| `tool`、`capability`、`loop_capability`、`reading_extension` | WASM 注册、校验和执行 |
| `http_route` | `static:`、`json:`、`wasm:` 路由执行 |
| `frontend_page` | 插件页面、资源和认证策略 |
| `visualizer` | 插件内 `visualizer.json` 进入可视化器目录，资源走沙箱 iframe |
| `persistence_schema` | 声明式 schema/operations 契约元数据，不执行任意代码 |
| `app_connector` | 声明式 operations 契约元数据，不执行任意代码 |

`persistence_schema` 与 `app_connector` 在 v1.7.0 协议中就是契约描述，
不是 Python worker 入口；纯 Rust 版保留其清单校验和插件目录展示。

## Exec 边界

`exec` 工具默认关闭，只在用户显式启用 `sandbox_allow_subprocess` 或
`exec_enabled` 后可见。它支持用户系统中已经安装的 Python、PowerShell/sh
和 C/C++ 工具链，不下载或回退到随包运行时。

执行工作目录位于 `data/exec_workspaces/<turn_id>`，并限制源码、输出、
artifact 数量、文件哈希和超时。子进程环境被清空，仅保留系统运行变量和
便携 HOME/TEMP/APPDATA/XDG 路径，避免模型 API Key 泄漏。

当前实现是进程隔离和危险命令拦截，**不是完整 OS 文件系统沙箱**。外部
程序理论上仍可通过系统 API 访问用户明确可读写的位置。高风险场景应使用
系统级沙箱、容器或专用虚拟机。

## 文档解析

纯 Rust 内置文本、HTML、Markdown、CSV、XLSX、PDF、DOCX、EPUB、图片和
常见办公格式解析。MarkItDown、PyMuPDF4LLM、LiteParse 等旧 Python 解析器
不会安装或调用；MinerU、Docling、Tika 可使用远程服务或用户配置的外部
CLI。状态码 `external_parser_required` 表示需要用户配置外部解析器。

## 开发验证

```powershell
cd D:\VS_Projects\DeepTutor\src-tauri
cargo fmt --all
cargo check --locked
cargo test --locked

cd ..
npm --prefix web ci --legacy-peer-deps
npm --prefix web run build:desktop

.\scripts\audit_openapi_routes.ps1
.\scripts\build_portable.ps1
.\scripts\verify_portable.ps1 -StageRoot artifacts\portable\DeepTutor -ZipPath artifacts\DeepTutor-1.7.0-portable-X64.zip -SmokeTest
```

开发运行：

```powershell
npm --prefix web run dev:desktop
cargo run --manifest-path src-tauri/Cargo.toml
```

Rust 测试目标不链接 GUI，避免 Windows 测试资源清单问题。构建脚本把
`DeepTutor.exe`、与 EXE 架构一致的 `WebView2Loader.dll`、`resources/web`、
许可证、便携说明和 manifest 放入发行目录；用户数据不会进入 ZIP。

`verify_portable.ps1` 解析 PE 头和导入表，要求 `DeepTutor.exe` 显式导入的
`WebView2Loader.dll` 与 EXE、manifest 架构一致，并确认其余原生依赖由系统或
发行目录提供。它还会比较目录与 ZIP 文件清单、校验 SHA-256、扫描禁入项。
`-SmokeTest` 在隔离副本中启动 EXE，等待 Rust 服务就绪，并确认 SQLite、
WebView2、日志、临时目录和 HOME 都创建在该副本内部。

## 发行审计

构建后必须确认 `artifacts/portable/DeepTutor` 与 ZIP 不包含：

- `python.exe`、`python3`、`.py`、`.pyc`、`.pyd`
- `node.exe`、`node_modules`、Next.js、Remotion
- `src-tauri/resources/runtime/python`
- `src-tauri/resources/runtime/node`
- 用户的 `data/`、`cache/`、`temp/`、`logs/`、`home/`

仓库中遗留的被忽略运行时目录不进入 Git、Tauri 资源或便携 ZIP；正式打包
仍应按上述清单做独立审计。

## 已知边界

- 可选 WebView2 固定运行时会显著增大发行包；默认使用系统 WebView2。
- 外部模型/API、MCP、CLI、MinerU、Docling、Tika、LibreOffice 等能力取决于
  用户配置和本机安装，客户端不会伪造 ready 状态。
- Windows 正式发行建议使用 MSVC + Windows SDK 构建，并在干净虚拟机验证
  便携目录、更新回滚和 WebView2 缓存位置。
