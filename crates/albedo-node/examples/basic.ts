import {
  analyzeProject,
  getCacheStats,
  optimizeManifest,
  type RenderManifestV2,
} from '../typed';

async function main(): Promise<void> {
  const manifest: RenderManifestV2 = await analyzeProject('../../test-app/src/components', {
    cacheDir: '.albedo-cache',
    persistCache: true,
  });

  const optimized = await optimizeManifest(manifest, {
    inferSharedVendorChunks: true,
    sharedDependencyMinComponents: 2,
  });

  const cache = getCacheStats();
  console.log('Manifest components:', optimized.components.length);
  console.log('Vendor chunks:', optimized.vendor_chunks.length);
  console.log('Cache metrics:', cache);
}

void main();
