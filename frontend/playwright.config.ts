import { defineConfig } from '@playwright/test';
import { existsSync, readFileSync } from 'node:fs';

const fixture = JSON.parse(readFileSync('../test-results/e2e.json', 'utf8'));
const chrome = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
export default defineConfig({
  testDir: './tests',
  timeout: 30_000,
  workers: 1,
  reporter: 'list',
  outputDir: '../test-results/browser',
  use: {
    baseURL: fixture.url,
    headless: true,
    viewport: { width: 1440, height: 1080 },
    launchOptions: existsSync(chrome) ? { executablePath: chrome } : {},
    trace: 'retain-on-failure',
  },
});
