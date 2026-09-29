import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// The backend serves dist/ in production (ADR 0007); in dev, Vite proxies to it.
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      '/api': 'http://localhost:3000',
      '/hooks': 'http://localhost:3000',
    },
  },
})
