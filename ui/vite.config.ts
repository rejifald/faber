import { resolve } from 'node:path';
import tailwindcss from '@tailwindcss/vite';
import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';

// Two entries, one build each (ADR-0042 § 1), picked by `--mode`:
//   dashboard  served by the daemon, embedded by build.rs from dist/dashboard
//   popover    the Tauri menu-bar popover, dist/popover
// Each entry's index.html is the Vite root of its build. `base: './'` keeps
// every asset URL relative, so the bundle works under whatever path serves it.
const ENTRIES = ['dashboard', 'popover'];

export default defineConfig(({ mode }) => {
  const entry = ENTRIES.includes(mode) ? mode : 'dashboard';
  return {
    root: resolve(import.meta.dirname, entry),
    base: './',
    publicDir: resolve(import.meta.dirname, 'public'),
    plugins: [react(), tailwindcss()],
    // The theme imports desktop/src/tokens.css, one level above this package.
    server: { fs: { allow: [resolve(import.meta.dirname, '..')] } },
    build: {
      outDir: resolve(import.meta.dirname, 'dist', entry),
      emptyOutDir: true,
      // Fonts stay files: inlining them would put every face in the CSS.
      assetsInlineLimit: 0,
    },
  };
});
