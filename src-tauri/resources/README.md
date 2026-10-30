# Tauri 打包资源

桌面发行构建只需要：

- `web/`：`npm run build:desktop` 生成的 Vite 静态资源。
- `webview2-runtime/`：可选 WebView2 Fixed Version Runtime。

桌面客户端不读取 Python、Node.js、`node_modules`、Next.js 或 Remotion
运行时。`resources/runtime/` 中若存在旧构建残留，也会被 `.gitignore`、
Tauri 资源声明和便携打包脚本排除。

正式构建使用：

```powershell
npm --prefix web run build:desktop
.\scripts\build_portable.ps1
```
