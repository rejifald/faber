import { resolve } from 'node:path';
import tailwindcss from '@tailwindcss/vite';
import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';

// The stories' build: the entries' plugins, without their root and output paths.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  // The theme imports desktop/src/tokens.css, two levels above this directory.
  server: { fs: { allow: [resolve(import.meta.dirname, '..', '..')] } },
  build: { assetsInlineLimit: 0 },
});
