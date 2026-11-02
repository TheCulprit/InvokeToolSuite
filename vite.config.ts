import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({ plugins: [react()], clearScreen: false, server: { host: '0.0.0.0', port: 1420, strictPort: true }, envPrefix: ['VITE_', 'TAURI_'], build: { target: process.env.TAURI_ENV_PLATFORM === 'windows' ? 'chrome105' : 'safari13' } })
