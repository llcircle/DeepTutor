# DeepTutor 工作日志

## 2026-10-29

- 从最新稳定标签 `v1.7.0` 创建分支 `codex/tauri-rust-port`。
- 确认仓库工作区干净；当前 Python 能力层约 28 万行，包含 25 个官方模块、CLI、WebSocket API、Python SDK 与 Next.js 前端。
- 确定迁移策略：保留完整能力引擎，以 Tauri 2 + Rust 作为桌面后端、进程监督器和 HTTP/WebSocket 统一网关，避免重写造成功能缺失。
- 开始实现便携式安装目录数据布局、缓存重定向、运行时打包与桌面启动链路。

## 2026-10-31

- 根据用户澄清修正技术路线：桌面端改为不绑定 Python 的纯 Rust/Tauri 独立客户端，Node.js 仅在构建期生成 Vite 静态资源。
- 已在 `codex/tauri-rust-port` 分支完成 Rust Axum API、WebSocket、回合引擎、认证、存储、工具、能力、文件、更新、定时任务与桌面生命周期迁移。
- 新增无宿主能力 WASM 插件运行时，支持 `wasm:` 路由并拒绝 Python wheel 插件；请求、响应、内存、燃料与执行时间均有限制。
- 所有正式运行状态与 WebView2 缓存指向安装目录；便携构建禁止包含 Python、Node、Next.js、Remotion、旧运行时和用户数据。
- 代码当前处于开发验证阶段；完整测试、OpenAPI 审计和便携包构建结果将在本轮完成后记录。

## 2026-10-31 01:27:57 +08:00
- 分支：codex/tauri-rust-port（基于 v1.7.0 / f9a68757）。
- Rust 验证：cargo check --locked 通过；cargo test --locked 通过（118 passed, 0 failed）。
- 新增回归：插件 WASM Tool、Capability、loop_capability、Reading Extension、Visualizer manifest、便携路径与篡改包保护。
- 前端验证：npm --prefix web run build:desktop 通过。
- OpenAPI 审计：695 个契约操作，710 个 Rust 操作，缺失契约 0，额外 Rust 操作 30，脚本退出码 0。
- 便携构建：artifacts/DeepTutor-1.7.0-portable-X64.zip；SHA-256 300FB03E224946B5963D47CDA611BFE688C1F0CDA965A735BAC90234C5D28DFA。
- ZIP 审计：927 个文件；Python/Node 运行时、node_modules、.py/.pyc/.pyd、data/cache/temp/logs/home 均未进入发行包。
- 代码可用状态：可用于纯 Rust Tauri 便携桌面构建；外部 MCP、CLI、解析器和模型服务仍按用户配置启用。

## 2026-10-31 01:45:13 +08:00

- 最终 Rust 验证：`cargo check --locked` 与 `cargo test --locked` 均通过；测试结果 118 passed、0 failed。
- 最终前端验证：`build:desktop`、`typecheck` 通过；`test:unit` 为 197 files / 998 tests 通过；`test:node` 为 1306 / 1306 通过；`lint` 为 0 errors、159 warnings。
- OpenAPI 契约审计通过：695 个契约操作、710 个 Rust 操作、缺失契约 0、额外 Rust 操作 30，脚本退出码 0。
- 最新前端构建完成后重新执行便携打包，生成 `artifacts/DeepTutor-1.7.0-portable-X64.zip`。
- 最终目录与 ZIP 各包含 927 个文件；Python/Node 运行时、`node_modules`、`.py/.pyc/.pyd`、`runtime/python`、`runtime/node`、用户 `data/cache/temp/logs/home` 禁入项均为 0，审计通过。
- 最终 ZIP SHA-256：`F23726558DDFF1A03A052714CB8627F9CD308B9302A31C9B03E8104D850C4577`。上一条日志中的哈希对应旧前端构建产物，已由本条最终值替代。
- 代码可用状态：纯 Rust/Tauri 便携桌面构建可用于验收；外部 MCP、CLI、文档解析器和模型服务仍需按用户环境配置后启用。

## 2026-10-31 10:07:18 +08:00
- 任务：修复 DeepTutor.exe 缺少 WebView2Loader.dll，并对独立 Tauri/Rust 便携版进行充分校验。
- 分支：codex/tauri-rust-port；基准提交：4c965ac7d7698fa697cf786b0d0a441e08c82e5b。
- 修复：build_portable.ps1 随包复制 WebView2Loader.dll，manifest 记录 native_dependencies，升级说明同步替换 EXE、DLL 与 resources/web。
- 修复：删除 Rust HTTP 路由中的重复内存路由；新增 GET /api/modules 的 25 项纯 Rust 模块清单；根路由改为 /chat；修正日志过滤器。
- 新增：scripts/verify_portable.ps1，覆盖必需文件、PE 架构、EXE/DLL 导入、禁入项、目录/ZIP 一致性、SHA-256、隔离启动、安装目录数据与 /api/modules。
- 测试：cargo check 通过；cargo test 122/122 通过；前端 Node 1307/1307 通过；TypeScript typecheck 通过；PowerShell 语法通过。
- 运行验收：最终 EXE 隔离启动通过；WebView/Rust 服务就绪；SQLite、WebView2 缓存、日志、temp、home 均位于安装目录；/api/modules 返回 25 个且全部 installed/enabled/ready。
- 可视验收：初始地址为 /chat；聊天欢迎页、输入框和能力入口正常；日历、任务看板、笔记本（/co-writer）均可正常渲染，不再出现 404 或仅显示“重试”。
- 发布物：D:\VS_Projects\DeepTutor\artifacts\DeepTutor-1.7.0-portable-X64.zip。
- SHA-256：815702d7c0a67bcd4bba939d64a893e692b238df368b824c516430f162a470f0。
- 路由审计：OpenAPI 695 项，Rust 711 项，缺失 0 项，额外 31 项兼容/扩展路由。
- 代码可用：是。
