#[path = "src/architecture.rs"]
mod architecture;

use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let source_root = Path::new("src");
    println!("cargo:rerun-if-changed={}", source_root.display());
    let mut files = Vec::new();
    collect_rust_files(source_root, &mut files);
    let mut failures = Vec::new();
    for path in files {
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        for violation in architecture::violations(&path, &source) {
            failures.push(format!("{}: {violation}", path.display()));
        }
    }
    assert!(
        failures.is_empty(),
        "spatial architecture violations:\n{}",
        failures.join("\n")
    );
}

fn collect_rust_files(directory: &Path, output: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
    {
        let path = entry.expect("source directory entry").path();
        if path.is_dir() {
            collect_rust_files(&path, output);
        } else if path.extension().is_some_and(|extension| extension == "rs")
            && !path.ends_with("architecture.rs")
        {
            output.push(path);
        }
    }
}
