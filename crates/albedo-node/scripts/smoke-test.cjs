const path = require('node:path');

async function main() {
  const bridge = require(path.resolve(__dirname, '..', 'index.js'));

  if (typeof bridge.analyzeProject !== 'function') {
    throw new Error('Expected analyzeProject export');
  }
  if (typeof bridge.optimizeManifest !== 'function') {
    throw new Error('Expected optimizeManifest export');
  }
  if (typeof bridge.getCacheStats !== 'function') {
    throw new Error('Expected getCacheStats export');
  }

  const componentRoot = path.resolve(
    __dirname,
    '..',
    '..',
    '..',
    'test-app',
    'src',
    'components',
  );

  const manifest = await bridge.analyzeProject(componentRoot, {
    persistCache: false,
  });
  if (!manifest || !Array.isArray(manifest.components) || manifest.components.length === 0) {
    throw new Error('analyzeProject returned an invalid manifest payload');
  }

  const optimized = await bridge.optimizeManifest(manifest, {
    inferSharedVendorChunks: true,
    sharedDependencyMinComponents: 2,
  });
  if (!optimized || !Array.isArray(optimized.vendor_chunks)) {
    throw new Error('optimizeManifest returned an invalid payload');
  }

  const stats = bridge.getCacheStats();
  if (typeof stats.cacheEnabled !== 'boolean') {
    throw new Error('getCacheStats returned invalid cacheEnabled field');
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
