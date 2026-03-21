use crate::manifest::schema::{RenderManifestV2, StaticSliceArtifactManifest};
use crate::runtime::static_slice::build_static_slice_manifest;
use std::collections::HashMap;

pub fn build_bundle_static_slice_manifest(
    manifest: &RenderManifestV2,
    module_sources: &HashMap<String, String>,
) -> StaticSliceArtifactManifest {
    build_static_slice_manifest(manifest, module_sources)
}
