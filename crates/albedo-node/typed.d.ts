import type {
  AnalyzeProjectOptions,
  CacheMetrics,
  OptimizeManifestOptions,
} from './index'

export type { AnalyzeProjectOptions, OptimizeManifestOptions, CacheMetrics }

export type Tier = 'A' | 'B' | 'C'

export type HydrationMode =
  | 'None'
  | 'OnVisible'
  | 'OnIdle'
  | 'OnInteraction'
  | 'Immediate'

export interface ComponentManifestEntry {
  id: number
  name: string
  module_path: string
  tier: Tier
  weight_bytes: number
  priority: number
  dependencies: number[]
  can_defer: boolean
  hydration_mode: HydrationMode
}

export interface VendorChunk {
  chunk_name: string
  packages: string[]
}

export interface RenderManifestV2 {
  schema_version: string
  generated_at: string
  components: ComponentManifestEntry[]
  parallel_batches: number[][]
  critical_path: number[]
  vendor_chunks: VendorChunk[]
}

export declare function analyzeProject(
  projectPath: string,
  options?: AnalyzeProjectOptions | undefined | null,
): Promise<RenderManifestV2>

export declare function optimizeManifest(
  manifest: RenderManifestV2,
  options?: OptimizeManifestOptions | undefined | null,
): Promise<RenderManifestV2>

export declare function getCacheStats(): CacheMetrics
