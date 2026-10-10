//! Hashes the files in `src/web/static/` so their URLs change whenever their content does,
//! which lets browsers cache them for a long time without ever serving stale ones.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};

fn main() {
    println!("cargo:rerun-if-changed=src/web/static");
    let mut files: Vec<_> = fs::read_dir("src/web/static")
        .expect("static files directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();

    let mut hasher = DefaultHasher::new();
    for path in files {
        path.file_name().hash(&mut hasher);
        fs::read(&path).expect("readable asset").hash(&mut hasher);
    }
    println!("cargo:rustc-env=ASSET_VERSION={:016x}", hasher.finish());
}
