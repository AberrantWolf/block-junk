//! Validate and fingerprint the referenced resource graph on both sides.
use bevy::asset::io::{AssetReader, AssetReaderError, PathStream, Reader, VecReader};
use block_junk_scripting::resources::ResourceRoots;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

pub fn validate(
    roots: &ResourceRoots,
    definitions: &BTreeMap<String, serde_json::Value>,
) -> Result<[u8; 32], String> {
    fn paths(value: &serde_json::Value, at: &str, out: &mut BTreeMap<String, Vec<String>>) {
        match value {
            serde_json::Value::String(path) if path.starts_with("mods://") => {
                out.entry(path.split('#').next().unwrap().to_owned())
                    .or_default()
                    .push(at.into());
            }
            serde_json::Value::Array(values) => {
                for (i, value) in values.iter().enumerate() {
                    paths(value, &format!("{at}/{i}"), out);
                }
            }
            serde_json::Value::Object(fields) => {
                for (key, value) in fields {
                    paths(value, &format!("{at}/{key}"), out);
                }
            }
            _ => {}
        }
    }
    let mut references = BTreeMap::new();
    for (id, definition) in definitions {
        paths(definition, id, &mut references);
    }
    let mut queue: BTreeSet<String> = references
        .keys()
        .map(|p| p.split('#').next().unwrap().to_owned())
        .collect();
    let mut visited = BTreeSet::new();
    let mut digests = BTreeMap::new();
    let mut animation_counts = BTreeMap::new();
    let mut graph_bytes = 0u64;
    while let Some(uri) = queue.pop_first() {
        if !visited.insert(uri.clone()) {
            continue;
        }
        if visited.len() > 16384 {
            return Err("referenced asset graph exceeds 16384 files".into());
        }
        let relative = uri
            .strip_prefix("mods://")
            .ok_or("assets must use mods:// identities")?;
        let physical = roots
            .resolve(Path::new(relative))
            .map_err(|e| format!("{uri}: {e}; referenced by {:?}", references.get(&uri)))?;
        let metadata = std::fs::metadata(&physical).map_err(|e| e.to_string())?;
        if !metadata.is_file() || metadata.len() > 128 * 1024 * 1024 {
            return Err(format!("{uri}: resource must be a file of at most 128 MiB"));
        }
        graph_bytes = graph_bytes
            .checked_add(metadata.len())
            .ok_or("asset graph size overflow")?;
        if graph_bytes > 1024 * 1024 * 1024 {
            return Err("referenced asset graph exceeds 1 GiB".into());
        }
        if !relative.ends_with(".meta") {
            let metadata_path = format!("{relative}.meta");
            let mut physical_meta = physical.as_os_str().to_owned();
            physical_meta.push(".meta");
            if Path::new(&physical_meta).exists() {
                roots.resolve(Path::new(&metadata_path))?;
                queue.insert(format!("mods://{metadata_path}"));
            }
        }
        let bytes = std::fs::read(&physical).map_err(|e| format!("{uri}: {e}"))?;
        digests.insert(uri.clone(), blake3::hash(&bytes));
        let extension = physical.extension().and_then(|s| s.to_str()).unwrap_or("");
        if matches!(extension, "gltf" | "glb") {
            let gltf =
                gltf::Gltf::from_slice(&bytes).map_err(|e| format!("{uri}: invalid glTF: {e}"))?;
            validate_gltf_data(&gltf, roots, relative).map_err(|e| format!("{uri}: {e}"))?;
            animation_counts.insert(uri.clone(), gltf.animations().count());
            // glTF's validated document gives canonical JSON, including external
            // image and buffer URIs. Embedded data is already hashed above.
            let json = serde_json::to_value(gltf.document.as_json()).map_err(|e| e.to_string())?;
            for category in ["buffers", "images"] {
                for entry in json[category].as_array().into_iter().flatten() {
                    let Some(dependency) = entry["uri"].as_str() else {
                        continue;
                    };
                    if dependency.starts_with("data:") {
                        continue;
                    }
                    if dependency.contains(':')
                        || dependency.starts_with('/')
                        || dependency.contains('\\')
                    {
                        return Err(format!(
                            "{uri}: external resource URI {dependency:?} must be relative"
                        ));
                    }
                    let dependency = decode_uri(dependency)?;
                    let joined = Path::new(relative)
                        .parent()
                        .ok_or("asset has no owner")?
                        .join(dependency);
                    // Check containment before canonicalizing the virtual name.
                    let resolved = roots.resolve(&joined)?;
                    let owner = relative.split('/').next().unwrap();
                    let root = roots.0[owner].canonicalize().map_err(|e| e.to_string())?;
                    let local = resolved.strip_prefix(root).map_err(|e| e.to_string())?;
                    let next = format!("mods://{owner}/{}", local.to_string_lossy());
                    references
                        .entry(next.clone())
                        .or_default()
                        .push(uri.clone());
                    queue.insert(next);
                }
            }
        }
    }
    for (id, definition) in definitions
        .iter()
        .filter(|(id, _)| id.starts_with("animations/"))
    {
        let asset = definition["asset"]
            .as_str()
            .ok_or_else(|| format!("{id}/asset missing"))?;
        let count = animation_counts
            .get(asset)
            .ok_or_else(|| format!("{id}/asset must reference a validated glTF resource"))?;
        let clip = definition["clip_index"]
            .as_u64()
            .ok_or_else(|| format!("{id}/clip_index missing"))?;
        if clip >= *count as u64 {
            return Err(format!(
                "{id}/clip_index {clip} outside {asset} animation count {count}"
            ));
        }
    }
    let mut hash = blake3::Hasher::new();
    for (uri, digest) in digests {
        hash.update(&(uri.len() as u64).to_le_bytes());
        hash.update(uri.as_bytes());
        hash.update(digest.as_bytes());
    }
    Ok(*hash.finalize().as_bytes())
}

fn resource_bytes(roots: &ResourceRoots, relative: &str, uri: &str) -> Result<Vec<u8>, String> {
    if uri.starts_with("data:") {
        use base64::Engine;
        let (header, data) = uri.split_once(',').ok_or("invalid embedded resource")?;
        if !header.ends_with(";base64") {
            return Err("embedded glTF resource must use base64".into());
        }
        if data.len() > 128 * 1024 * 1024 {
            return Err("embedded resource exceeds limit".into());
        }
        return base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| e.to_string());
    }
    let uri = decode_uri(uri)?;
    if uri.contains(':') || uri.starts_with('/') || uri.contains('\\') {
        return Err("external glTF resource must be relative".into());
    }
    let physical = roots.resolve(
        &Path::new(relative)
            .parent()
            .ok_or("asset has no owner")?
            .join(uri),
    )?;
    if std::fs::metadata(&physical)
        .map_err(|e| e.to_string())?
        .len()
        > 128 * 1024 * 1024
    {
        return Err("glTF resource exceeds 128 MiB".into());
    }
    std::fs::read(physical).map_err(|e| e.to_string())
}

fn validate_image(bytes: &[u8], mime: Option<&str>) -> Result<(), String> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    if let Some(mime) = mime {
        let declared = match mime {
            "image/png" => image::ImageFormat::Png,
            "image/jpeg" => image::ImageFormat::Jpeg,
            _ => return Err(format!("unsupported glTF image MIME type {mime}")),
        };
        if reader.format() != Some(declared) {
            return Err("invalid glTF image: MIME type does not match content".into());
        }
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    reader
        .decode()
        .map(|_| ())
        .map_err(|e| format!("invalid glTF image: {e}"))
}

fn validate_gltf_data(
    gltf: &gltf::Gltf,
    roots: &ResourceRoots,
    relative: &str,
) -> Result<(), String> {
    use gltf::{buffer, image};
    let mut buffers = Vec::new();
    let mut total = 0usize;
    for buffer in gltf.buffers() {
        let bytes = match buffer.source() {
            buffer::Source::Uri(uri) => resource_bytes(roots, relative, uri)?,
            buffer::Source::Bin => gltf.blob.clone().ok_or("missing GLB binary chunk")?,
        };
        total = total
            .checked_add(bytes.len())
            .ok_or("glTF buffer size overflow")?;
        if total > 256 * 1024 * 1024 {
            return Err("glTF decoded buffers exceed 256 MiB".into());
        }
        if bytes.len() < buffer.length() {
            return Err(format!(
                "buffer {} shorter than declared byteLength",
                buffer.index()
            ));
        }
        buffers.push(bytes);
    }
    for view in gltf.views() {
        if view
            .offset()
            .checked_add(view.length())
            .is_none_or(|end| end > view.buffer().length())
        {
            return Err(format!("buffer view {} exceeds its buffer", view.index()));
        }
    }
    let fits = |offset: usize, count: usize, stride: usize, size: usize, length: usize| {
        count > 0
            && stride >= size
            && (count - 1)
                .checked_mul(stride)
                .and_then(|n| n.checked_add(size))
                .and_then(|n| n.checked_add(offset))
                .is_some_and(|end| end <= length)
    };
    for accessor in gltf.accessors() {
        use gltf::accessor::Dimensions;
        let columns = match accessor.dimensions() {
            Dimensions::Mat2 => 2usize,
            Dimensions::Mat3 => 3,
            Dimensions::Mat4 => 4,
            _ => 0,
        };
        let size = if columns == 0 {
            accessor.size()
        } else {
            // glTF aligns matrix columns to four bytes, including byte/short
            // components; multiplicity alone underestimates those ranges.
            (columns * accessor.data_type().size()).div_ceil(4) * 4 * columns
        };
        if let Some(view) = accessor.view()
            && !fits(
                accessor.offset(),
                accessor.count(),
                view.stride().unwrap_or(size),
                size,
                view.length(),
            )
        {
            return Err(format!(
                "accessor {} exceeds its buffer view",
                accessor.index()
            ));
        }
        if let Some(sparse) = accessor.sparse() {
            let indices = sparse.indices();
            let index_size = match indices.index_type() {
                gltf::accessor::sparse::IndexType::U8 => 1,
                gltf::accessor::sparse::IndexType::U16 => 2,
                gltf::accessor::sparse::IndexType::U32 => 4,
            };
            let values = sparse.values();
            if sparse.count() > accessor.count()
                || !fits(
                    indices.offset(),
                    sparse.count(),
                    index_size,
                    index_size,
                    indices.view().length(),
                )
                || !fits(
                    values.offset(),
                    sparse.count(),
                    size,
                    size,
                    values.view().length(),
                )
            {
                return Err(format!(
                    "accessor {} has invalid sparse ranges",
                    accessor.index()
                ));
            }
            let view = indices.view();
            let start = view.offset() + indices.offset();
            let bytes = &buffers[view.buffer().index()][start..start + sparse.count() * index_size];
            let mut previous = None;
            for bytes in bytes.chunks_exact(index_size) {
                let mut word = [0u8; 4];
                word[..index_size].copy_from_slice(bytes);
                let index = u32::from_le_bytes(word) as usize;
                if index >= accessor.count() || previous.is_some_and(|old| old >= index) {
                    return Err(format!(
                        "accessor {} sparse indices are unsorted or out of bounds",
                        accessor.index()
                    ));
                }
                previous = Some(index);
            }
        }
    }
    for image in gltf.images() {
        match image.source() {
            image::Source::Uri { uri, mime_type } => {
                let embedded_mime = uri
                    .strip_prefix("data:")
                    .and_then(|data| data.split_once(';'))
                    .map(|(mime, _)| mime);
                validate_image(
                    &resource_bytes(roots, relative, uri)?,
                    mime_type.or(embedded_mime),
                )?
            }
            image::Source::View { view, mime_type } => validate_image(
                &buffers[view.buffer().index()][view.offset()..view.offset() + view.length()],
                Some(mime_type),
            )?,
        }
    }
    Ok(())
}
fn decode_uri(uri: &str) -> Result<String, String> {
    let mut bytes = Vec::new();
    let input = uri.as_bytes();
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' {
            let pair = input
                .get(i + 1..i + 3)
                .ok_or("invalid percent escape in asset URI")?;
            let hex = std::str::from_utf8(pair).map_err(|e| e.to_string())?;
            bytes.push(u8::from_str_radix(hex, 16).map_err(|e| e.to_string())?);
            i += 3;
        } else {
            bytes.push(input[i]);
            i += 1;
        }
    }
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

pub struct ModAssetReader {
    pub roots: ResourceRoots,
}
impl ModAssetReader {
    fn resolve(&self, path: &Path) -> Result<PathBuf, AssetReaderError> {
        self.roots
            .resolve(path)
            .map_err(|_| AssetReaderError::NotFound(path.into()))
    }
}
impl AssetReader for ModAssetReader {
    async fn read<'a>(&'a self, path: &'a Path) -> Result<impl Reader + 'a, AssetReaderError> {
        let physical = self.resolve(path)?;
        let file = bevy::asset::io::file::FileAssetReader::new("/");
        let mut reader = file.read(&physical).await?;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        Ok(VecReader::new(bytes))
    }
    async fn read_meta<'a>(&'a self, path: &'a Path) -> Result<impl Reader + 'a, AssetReaderError> {
        let mut metadata = path.as_os_str().to_owned();
        metadata.push(".meta");
        let physical = self.resolve(Path::new(&metadata))?;
        let file = bevy::asset::io::file::FileAssetReader::new("/");
        let mut reader = file.read(&physical).await?;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        Ok(VecReader::new(bytes))
    }
    async fn is_directory<'a>(&'a self, path: &'a Path) -> Result<bool, AssetReaderError> {
        Ok(self.resolve(path)?.is_dir())
    }
    async fn read_directory<'a>(
        &'a self,
        path: &'a Path,
    ) -> Result<Box<PathStream>, AssetReaderError> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(self.resolve(path)?)? {
            entries.push(path.join(entry?.file_name()));
        }
        entries.sort();
        Ok(Box::new(bevy::tasks::futures_lite::stream::iter(entries)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (
        tempfile::TempDir,
        ResourceRoots,
        BTreeMap<String, serde_json::Value>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("a-directory-unrelated-to-id");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(
            root.join("mesh.gltf"),
            r#"{"asset":{"version":"2.0"},"buffers":[{"uri":"payload.bin","byteLength":4}]}"#,
        )
        .unwrap();
        std::fs::write(root.join("payload.bin"), [0u8; 4]).unwrap();
        let roots = ResourceRoots(BTreeMap::from([("stable".into(), root)]));
        let definitions = BTreeMap::from([(
            "blocks/stable:fixture".into(),
            serde_json::json!({"mesh":"mods://stable/mesh.gltf"}),
        )]);
        (dir, roots, definitions)
    }
    #[test]
    fn stable_ids_resolve_renamed_directories_and_hash_transitive_buffers() {
        let (_dir, roots, definitions) = fixture();
        let first = validate(&roots, &definitions).unwrap();
        std::fs::write(roots.0["stable"].join("payload.bin"), [1u8; 4]).unwrap();
        assert_ne!(first, validate(&roots, &definitions).unwrap());
        let reader = ModAssetReader {
            roots: roots.clone(),
        };
        let bytes = bevy::tasks::block_on(async {
            let mut file = reader.read(Path::new("stable/payload.bin")).await.unwrap();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).await.unwrap();
            bytes
        });
        assert_eq!(bytes, vec![1; 4]);
        std::fs::remove_file(roots.0["stable"].join("payload.bin")).unwrap();
        assert!(
            validate(&roots, &definitions)
                .unwrap_err()
                .contains("payload.bin")
        );
    }
    #[test]
    fn truncated_buffers_and_invalid_image_payloads_fail_before_rendering() {
        let (_dir, roots, definitions) = fixture();
        std::fs::write(roots.0["stable"].join("payload.bin"), [0u8; 3]).unwrap();
        assert!(
            validate(&roots, &definitions)
                .unwrap_err()
                .contains("byteLength")
        );
        std::fs::write(roots.0["stable"].join("payload.bin"), [0u8; 4]).unwrap();
        std::fs::write(roots.0["stable"].join("mesh.gltf"),
            r#"{"asset":{"version":"2.0"},"buffers":[{"uri":"payload.bin","byteLength":4}],"bufferViews":[{"buffer":0,"byteOffset":2,"byteLength":4}]}"#).unwrap();
        assert!(
            validate(&roots, &definitions)
                .unwrap_err()
                .contains("buffer view")
        );
        std::fs::write(
            roots.0["stable"].join("mesh.gltf"),
            r#"{"asset":{"version":"2.0"},"images":[{"uri":"data:image/png;base64,AAAAAA=="}]}"#,
        )
        .unwrap();
        assert!(
            validate(&roots, &definitions)
                .unwrap_err()
                .contains("invalid glTF image")
        );
    }

    #[test]
    fn malformed_assets_and_external_escape_fail_headless_validation() {
        let (_dir, roots, definitions) = fixture();
        std::fs::write(roots.0["stable"].join("mesh.gltf"), "not gltf").unwrap();
        assert!(
            validate(&roots, &definitions)
                .unwrap_err()
                .contains("invalid glTF")
        );
        std::fs::write(
            roots.0["stable"].join("mesh.gltf"),
            r#"{"asset":{"version":"2.0"},"buffers":[{"uri":"../outside.bin","byteLength":4}]}"#,
        )
        .unwrap();
        std::fs::write(
            roots.0["stable"].parent().unwrap().join("outside.bin"),
            [0u8; 4],
        )
        .unwrap();
        assert!(
            validate(&roots, &definitions)
                .unwrap_err()
                .contains("escapes")
        );
    }
}
