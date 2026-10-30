import { defineConfig } from "vite";
import wasm from "vite-plugin-wasm";
import path from "path";

export default defineConfig({
  // Relative URLs work at both / and GitHub Pages' /JigsawGenerator/ path,
  // including the worker and the WASM binary loaded by that worker.
  base: "./",
  plugins: [
    wasm(),
    {
      name: "wasm-mime-type",
      configureServer(server) {
        server.middlewares.use((_req, res, next) => {
          if (_req.url?.endsWith(".wasm")) {
            res.setHeader("Content-Type", "application/wasm");
          }
          next();
        });
      },
    },
  ],
  resolve: {
    alias: {
      "puzzle-wasm": path.resolve(__dirname, "../crates/puzzle-wasm/pkg"),
    },
  },
  server: {
    fs: {
      allow: [".", "../crates/puzzle-wasm/pkg"],
    },
  },
  build: {
    target: "esnext",
  },
});
