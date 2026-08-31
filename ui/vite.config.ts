/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "node:path";

// Built UI is shipped inside the engine container at
// /usr/share/nexus/ui and served by tower_http::services::ServeDir.
// Hashed asset names are fine because every request lands at the
// same SPA index. The /api proxy lets `npm run dev` hit the engine
// on its default port without touching CORS.
export default defineConfig({
  base: "/",
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  build: {
    target: "es2022",
    outDir: "dist",
    sourcemap: true,
    cssCodeSplit: false,
    // Bumped from Vite's 500 kB default. Even after the manual-chunks
    // split below, the CodeMirror chunk lands around ~520 kB gzipped
    // because the language mode + theme + view are large by design;
    // splitting it further would just defer the same bytes across
    // more requests on the first /rules visit.
    chunkSizeWarningLimit: 900,
    rollupOptions: {
      output: {
        // Keep the main `index-*.js` lean by parking the heaviest
        // vendor surfaces in their own long-cache chunks. The router
        // and query client are touched by every page so they go
        // together; CodeMirror is only pulled by /rules so it gets
        // its own chunk that browsers can lazy-prefetch.
        manualChunks: (id) => {
          if (!id.includes("node_modules")) return undefined;
          if (id.includes("@codemirror") || id.includes("@uiw/react-codemirror") || id.includes("@lezer")) {
            return "codemirror";
          }
          if (id.includes("@tanstack")) {
            return "tanstack";
          }
          if (
            id.includes("/react/") ||
            id.includes("/react-dom/") ||
            id.includes("/scheduler/")
          ) {
            return "react";
          }
          if (
            id.includes("react-hook-form") ||
            id.includes("@hookform/") ||
            id.includes("/zod/")
          ) {
            return "forms";
          }
          if (id.includes("lucide-react")) {
            return "icons";
          }
          return undefined;
        },
      },
    },
  },
  server: {
    port: 5173,
    proxy: {
      // Local dev: vite serves the SPA, the engine serves /api on :8089.
      "/api": {
        target: "http://localhost:8089",
        changeOrigin: true,
      },
    },
  },
  // Vitest owns `src/`; Playwright owns `e2e/` and runs under `npm run e2e`.
  // Without this bound, `vitest run` globs the Playwright specs and fails on
  // all of them, which buries any real unit failure.
  test: {
    include: ["src/**/*.test.{ts,tsx}"],
  },
});
