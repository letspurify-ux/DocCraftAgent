import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
export default defineConfig({
  plugins: [react()],
  server: {
    port: Number(process.env.DOCCRAFT_FRONTEND_PORT ?? 6001),
    strictPort: true,
    proxy: {
      "/api": `http://127.0.0.1:${process.env.DOCCRAFT_PORT ?? 8765}`,
      "/health": `http://127.0.0.1:${process.env.DOCCRAFT_PORT ?? 8765}`,
    },
  },
  build: { chunkSizeWarningLimit: 1500 },
});
