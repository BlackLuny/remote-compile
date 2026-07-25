import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  build: {
    // The output is embedded into the rc-server binary (§14.1), so keep the
    // asset layout stable: assets/* is served immutable, everything else no-cache.
    outDir: "dist",
    emptyOutDir: true,
    chunkSizeWarningLimit: 1200,
    rollupOptions: {
      output: {
        // ECharts dwarfs everything else; splitting it lets the shell and the
        // non-charting pages paint without waiting for it.
        manualChunks(id: string) {
          if (id.includes("node_modules/echarts") || id.includes("node_modules/zrender")) {
            return "echarts";
          }
          if (id.includes("node_modules/react") || id.includes("node_modules/scheduler")) {
            return "react";
          }
          return undefined;
        },
      },
    },
  },
  server: {
    port: 5173,
    proxy: {
      "/api": { target: "http://127.0.0.1:7700", changeOrigin: true },
      "/metrics": { target: "http://127.0.0.1:7700", changeOrigin: true },
    },
  },
});
