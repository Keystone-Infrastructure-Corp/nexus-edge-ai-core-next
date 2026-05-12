import { defineConfig } from "vite";

// Built UI is shipped inside the engine container at /usr/share/nexus/ui
// and served by tower_http::services::ServeDir. Hashed asset names are fine
// because every request lands at the same SPA index.
export default defineConfig({
  base: "/",
  build: {
    target: "es2022",
    outDir: "dist",
    sourcemap: true,
    cssCodeSplit: false,
    rollupOptions: {
      output: {
        manualChunks: undefined,
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
});
