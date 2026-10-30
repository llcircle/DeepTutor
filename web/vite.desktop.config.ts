import path from "node:path";
import { fileURLToPath } from "node:url";

import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

const webRoot = path.dirname(fileURLToPath(import.meta.url));
const desktopRoot = path.join(webRoot, "desktop");

export default defineConfig(({ mode }) => ({
  root: desktopRoot,
  base: "/",
  publicDir: path.join(webRoot, "public"),
  plugins: [react()],
  resolve: {
    alias: [
      {
        find: /^next\/navigation$/,
        replacement: path.join(desktopRoot, "shims", "navigation.ts"),
      },
      {
        find: /^next\/link$/,
        replacement: path.join(desktopRoot, "shims", "link.tsx"),
      },
      {
        find: /^next\/image$/,
        replacement: path.join(desktopRoot, "shims", "image.tsx"),
      },
      {
        find: /^next\/dynamic$/,
        replacement: path.join(desktopRoot, "shims", "dynamic.tsx"),
      },
      {
        find: /^next\/font\/google$/,
        replacement: path.join(desktopRoot, "shims", "font-google.ts"),
      },
      { find: "@", replacement: webRoot },
    ],
  },
  define: {
    "process.env": JSON.stringify({
      NODE_ENV: mode,
      NEXT_PUBLIC_API_BASE: "",
      NEXT_PUBLIC_AUTH_ENABLED: "false",
      NEXT_PUBLIC_APP_VERSION: "1.7.0",
    }),
    "process.env.NODE_ENV": JSON.stringify(mode),
    "process.env.NEXT_PUBLIC_API_BASE": JSON.stringify(""),
    "process.env.NEXT_PUBLIC_AUTH_ENABLED": JSON.stringify("false"),
    "process.env.NEXT_PUBLIC_APP_VERSION": JSON.stringify("1.7.0"),
  },
  build: {
    outDir: path.resolve(webRoot, "..", "src-tauri", "resources", "web"),
    emptyOutDir: true,
    sourcemap: false,
    target: "es2022",
    chunkSizeWarningLimit: 4000,
    rollupOptions: {
      output: {
        manualChunks: {
          react: ["react", "react-dom", "react-router-dom"],
          markdown: ["react-markdown", "remark-gfm", "remark-math"],
          editor: ["@blocknote/core", "@blocknote/react"],
          documents: ["pdfjs-dist", "epubjs", "docx-preview"],
          charts: ["chart.js", "cytoscape", "mermaid"],
        },
      },
    },
  },
  server: {
    host: "127.0.0.1",
    port: 1420,
  },
}));
