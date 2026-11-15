import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

export default defineConfig({
  plugins: [svelte()],
  server: {
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:49321',
        configure(proxy) {
          proxy.on('proxyReq', (req) => req.setHeader('origin', 'http://127.0.0.1:49321'));
        },
      },
    },
  },
});
