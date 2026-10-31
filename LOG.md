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

## 2026-10-31 11:26:22 +08:00
- 任务：修复桌面端点击“学习/资料”后的动态组件与学习索引契约问题，并重新验收最终便携 EXE。
- 修改：桌面 `next/dynamic` 兼容层支持命名组件直接返回；`/api/dashboard/learning-index` 改为 Rust 汇总 `{sources, failed}`，覆盖书籍、精通之路、阅读、观看和练习五类数据并逐类容错。
- 测试：Rust 124/124 通过；前端 1312/1312 通过；TypeScript、ESLint、Vite desktop build、OpenAPI 路由审计通过。
- 产物：`artifacts/DeepTutor-1.7.0-portable-X64.zip`，32,938,236 字节，SHA-256 `3ECDAC0D70B87A3E75B6512D72A1D4E970E7F297E853B031295E338E8576E09E`。
- 发布校验：X64、30 项原生导入、928 个目录/ZIP 文件一致、禁入项 0、WebView/Rust 启动和 25 个模块接口通过。
- 实机校验：“学习”“资料”“全部学习”均正常显示；学习索引五类字段和 `failed` 契约正确；SQLite、cache、temp、home、logs 均写入安装目录，用户目录无 DeepTutor 运行数据。
- 代码可用：是。
[2026-10-31 15:00:04]
- 修复桌面端点击“设置”后主内容空白的问题。
- 根因：React 渲染函数内部反复创建 lazy 组件，导致惰性内容无法稳定提交。
- 同步修正阅读文件夹与观看会话特殊路由的模块级惰性加载和布局组合。
- 验证：TypeScript typecheck、Node 1314/1314、ESLint、desktop Vite build、便携包构建、verify_portable 静态与 SmokeTest 均通过。
- EXE 实测：“常规”“工作区”“外观”设置分页正常，工作区数据路径位于安装目录。
- 代码状态：可用。

## 2026-10-31 19:16:44 +08:00
- 修复模型设置保存：POST /api/settings/apply 接受空请求体，并将服务端保存的 settings_draft.catalog 提升为正式 catalog；响应返回前端需要的 catalog 与 draft。
- 修复便携构建脚本：artifacts\portable\DeepTutor 只更新发行文件，保留 data/cache/temp/logs/home；干净 ZIP 从 artifacts\portable-package\DeepTutor 生成，避免清除用户数据库与模型草稿。
- 修复便携验证脚本：发行载荷审计排除安装目录运行时数据，ZIP 仍严格执行禁入项检查；EXE 冒烟测试只复制发行载荷到全新目录，验证首次启动自行创建安装目录状态。
- 验证结果：cargo fmt/check/test 通过（125 项）；前端 Node 测试通过（1314 项）；OpenAPI 审计无缺失契约操作；ZIP 890 个文件、SHA-256、PE X64 与 30 项导入依赖通过；EXE 启动、Rust API、25 个模块和安装目录数据写入通过。
- GUI 验证：设置页正常显示，语言模型页面恢复 tju/deepseek-v4.1-flash；点击应用后显示“已应用”，无 400 Bad Request；API 确认正式配置包含 1 个连接、1 个语言模型、1 个嵌入模型，草稿为空。
- 代码状态：可用。

## 2026-10-31 21:44:41 - 修复模型连接地址解析并完善页面个性化接口
- 分支：codex/tauri-rust-port。
- 根因：模型配置通过 connection_id 关联提供方时，模型配置保留了旧 base_url；服务诊断和聊天路径未先解析关联连接，导致空地址或旧地址回退到 https://api.openai.com/v1。
- 修复：catalog::service_selection、engine::resolve_llm_settings 和 provider_probe::execute 统一解析连接覆盖后的有效地址、密钥、协议与附加请求头。
- 连带修复：补齐 GET /api/settings/page-customization 聚合接口和静态分段 GET，保存接口统一返回包含默认值的完整配置文档，避免聊天页显示“未能加载页面设置”。
- 回归测试：cargo fmt、cargo check --locked、cargo test --locked 全部通过，129 个测试通过、0 个失败。
- 本地集成验证：模型配置故意保留 api.openai.com 旧地址，连接配置指向本地模拟服务；实际请求命中 /v1/chat/completions，诊断事件中的请求目标为连接地址，测试成功。
- UI 验证：学习、资料、常规设置、语言模型和提供方页面均正常；模型绑定 tju，提供方地址为用户配置的自定义地址，API Key 仅以掩码显示。
- 发行验证：便携 EXE 启动冒烟、25 个模块接口、安装目录数据、PE 架构、30 项 DLL 导入依赖、890 个 ZIP 文件一致性及发行载荷禁入项全部通过。
- 最终包：artifacts/DeepTutor-1.7.0-portable-X64.zip，SHA-256=66055BC1D6BAA3347C95462DFB272FB843BF10E3A7872C605665BFA4611535A3。
- 代码状态：可用于提交；未推送。
