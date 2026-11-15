//! Stable resource identities map to resolved mod directories, never directory names.
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};
#[derive(Clone, Default)]
pub struct ResourceRoots(pub BTreeMap<String, PathBuf>);
impl ResourceRoots {
    pub fn discover(root: &Path) -> Result<Self, crate::LoadError> {
        crate::composition::discover(root).map(|mods| {
            Self(
                mods.into_iter()
                    .map(|(path, manifest)| (manifest.id, path))
                    .collect(),
            )
        })
    }
    pub fn from_registry(registry: &crate::ModRegistry) -> Self {
        Self(
            registry
                .mods
                .iter()
                .map(|m| (m.manifest.id.clone(), m.directory.clone()))
                .collect(),
        )
    }
    pub fn resolve(&self, path: &Path) -> Result<PathBuf, String> {
        let mut components = path.components();
        let Some(Component::Normal(id)) = components.next() else {
            return Err("asset path requires stable mod ID".into());
        };
        let id = id.to_str().ok_or("mod ID must be UTF-8")?;
        let root = self
            .0
            .get(id)
            .ok_or_else(|| format!("missing asset owner {id}"))?
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let relative = components.as_path();
        if relative.is_absolute() {
            return Err("absolute asset path forbidden".into());
        }
        let physical = root
            .join(relative)
            .canonicalize()
            .map_err(|e| format!("mod {id} resource {}: {e}", relative.display()))?;
        if !physical.starts_with(&root) {
            return Err(format!(
                "mod {id} resource escapes its directory: {}",
                relative.display()
            ));
        }
        Ok(physical)
    }
}
