# DeepTutor 开发经验

## Tauri 纯 Rust 迁移

- 桌面客户端不能只替换外壳。Tauri 窗口、Rust Axum 本地服务、回合引擎、持久化、认证、文件、插件、更新和 WebSocket 都必须形成独立运行链路，才能真正解除 Python/Node 运行时绑定。
- v1.7.0 的 Python 代码可作为功能清单和接口参考，但不能成为桌面运行依赖。迁移验证应以 Rust 路由覆盖、能力提示、存储行为和前端交互为依据，而不是检查 Python 包是否可导入。
- 构建期 Node.js 与运行期 Node.js 必须明确区分。Vite 可以在开发机上生成静态资源，但发行目录不得包含 `node_modules`、Next.js standalone、Remotion 或 Node 可执行文件。

## 便携数据

- 便携性不能只设置 `DEEPTUTOR_HOME`。SQLite WAL、日志、上传、生成文件、插件、更新下载、WebView2、HOME、APPDATA/LOCALAPPDATA、XDG 和 TMP 都必须在初始化早期统一指向安装目录。
- WebView2 数据目录必须在窗口创建前设置；否则即使应用数据已便携，浏览器缓存仍可能落入 C 盘。
- 安装目录不可写时应失败并给出明确路径。静默回退到用户目录会破坏便携语义，并导致复制安装目录后丢失状态。
- 打包脚本应白名单复制，并对 Python、Node、旧 runtime、Next.js、Remotion 和用户数据做显式黑名单审计。

## 插件与外部程序

- 第三方 Python wheel 插件不适合作为纯 Rust 客户端扩展。初始迁移采用无 host import 的 WASM ABI，不暴露文件、网络、进程和环境变量能力。
- WASM 仍需限制模块、请求、响应、线性内存、燃料和墙钟时间，否则单个插件可拖垮桌面服务。
- `exec` 默认关闭，只调用用户系统已安装的运行时。清空环境、独立工作目录、输出限制和超时可以降低风险，但不能宣称是完整 OS 文件系统沙箱。
- 文档解析器应如实区分内置 Rust 格式、远程服务和外部 CLI。缺失外部解析器时返回明确 readiness 状态，不能伪造成功或启动 Python。

## 纯 Rust Tauri 迁移经验（2026-10-31）
- Windows 便携路径必须对已存在目标文件使用空后缀直接返回规范化路径，避免 join("") 产生尾部分隔符导致 is_file 误判。
- 插件运行时只接受 wasm: Tool/Capability/Reading Extension 入口；拒绝 Python entry_point，且每次执行都重新计算安装包 SHA-256。
- 插件 Tool、Capability、loop_capability 使用统一 JSON 契约；Reading Extension 需校验 selection/visible_text，并校验 card、quiz、feedback、browser_speech payload。
- visualizer 扩展通过插件内 visualizer.json 接入 Rust visualizer catalog，资源由沙箱 iframe 服务；persistence_schema/app_connector 保持 v1.7.0 的声明式契约语义。
- 便携构建审计要同时检查目录和 ZIP；缓存、日志、临时文件、WebView2 用户数据必须相对安装根目录解析。

## 测试契约与发布收口

- 旧测试若固定断言 Python、Next.js 或 Remotion 必须随桌面运行时启动，应改为验证纯 Rust/Tauri 构建链、静态 Vite 产物和便携路径约束，否则测试会阻止正确的架构迁移。
- 替换 CommonJS/Next.js 桌面 shim 时不要保留名为 `module` 的变量；它会与 ESLint 的 CommonJS 作用域规则冲突。使用明确的配置变量名，并用静态检查及时发现残留运行时假设。
- 便携 ZIP 必须在最终前端重建之后生成。任何后续前端构建都会使旧 ZIP 和 SHA-256 失效，发布记录应明确以最终哈希替代旧值。
- 发布审计应分别读取展开目录与 ZIP 条目，比较文件数并搜索禁入路径；只检查构建脚本参数不足以证明发行包实际没有夹带 Python、Node 或用户数据。
