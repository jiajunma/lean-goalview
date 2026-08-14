import { defineConfig } from 'vite'

// Build the host page + compiled EditorApi to a predictable layout the proxy
// serves. The infoview module itself is NOT bundled — it's loaded at runtime
// from /imports/* via loadRenderInfoview, so widgets can import it too.
export default defineConfig({
  build: {
    rollupOptions: {
      // Keep these external: resolved at runtime from the import map (/imports).
      external: [
        '@leanprover/infoview',
        'react',
        'react/jsx-runtime',
        'react-dom',
      ],
      output: {
        entryFileNames: 'main.js',
        assetFileNames: 'assets/[name][extname]',
      },
    },
  },
})
