use std::path::PathBuf;

/// All the ways a texture doc can fail to load, validate, or evaluate.
/// `Invalid.at` is a human-readable doc path like
/// `texture "vanilla:grass_top" / layer 2 / step 3 (fbm) / param "octaves"`
/// — precise enough that an author can fix the file without reading Rust.
#[derive(Debug, thiserror::Error)]
pub enum TexError {
    #[error("{at}: {msg}")]
    Invalid { at: String, msg: String },
    #[error("lua error in {source_name}: {source}")]
    Lua {
        source_name: String,
        #[source]
        source: mlua::Error,
    },
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl TexError {
    pub fn invalid(at: impl Into<String>, msg: impl Into<String>) -> Self {
        Self::Invalid {
            at: at.into(),
            msg: msg.into(),
        }
    }
}
