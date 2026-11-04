import { resolve } from 'node:path';
import type { StorybookConfig } from '@storybook/react-vite';

// The story browser (ADR-0043): one story per state of every kit part, read by the owner and
// counted by `npm run catalog:check`. It builds with its own Vite config, not ../vite.config.ts,
// whose `root` and `outDir` belong to the two entries.
const config: StorybookConfig = {
  framework: {
    name: '@storybook/react-vite',
    options: { builder: { viteConfigPath: resolve(import.meta.dirname, 'vite.config.ts') } },
  },
  stories: ['../src/**/*.stories.tsx'],
  // Storybook reports usage and crashes to its makers by default; faber sends nothing anywhere.
  core: { disableTelemetry: true, enableCrashReports: false, disableWhatsNewNotifications: true },
  typescript: { reactDocgen: false },
};

export default config;
