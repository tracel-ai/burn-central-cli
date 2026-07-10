use anyhow::Context;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

pub fn collect_files(directory: &Path) -> anyhow::Result<BTreeMap<String, PathBuf>> {
    let base_dir = std::fs::canonicalize(directory)
        .with_context(|| format!("Failed to resolve directory '{}'.", directory.display()))?;

    if !base_dir.is_dir() {
        anyhow::bail!("'{}' is not a directory.", base_dir.display());
    }

    let mut files = BTreeMap::new();

    for entry in walkdir::WalkDir::new(&base_dir).follow_links(false) {
        let entry =
            entry.with_context(|| format!("Failed to walk directory '{}'.", base_dir.display()))?;

        if !entry.file_type().is_file() {
            continue;
        }

        let absolute = entry.path().to_path_buf();
        let rel_path = absolute
            .strip_prefix(&base_dir)
            .expect("walked entry should be under base_dir")
            .to_string_lossy()
            .replace('\\', "/");

        files.insert(rel_path, absolute);
    }

    if files.is_empty() {
        anyhow::bail!("No files found in '{}'.", base_dir.display());
    }

    Ok(files)
}

pub struct FileMeta {
    pub rel_path: String,
    pub size_bytes: u64,
    pub checksum: String,
}

fn file_sha256_and_size(path: &Path) -> anyhow::Result<(String, u64)> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("Failed to open file '{}'.", path.display()))?;

    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    let mut size = 0u64;

    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("Failed reading file '{}'.", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }

    Ok((format!("{:x}", hasher.finalize()), size))
}

pub fn build_file_specs(files: &BTreeMap<String, PathBuf>) -> anyhow::Result<Vec<FileMeta>> {
    let mut specs = Vec::with_capacity(files.len());

    for (rel_path, absolute_path) in files {
        let (checksum, size_bytes) = file_sha256_and_size(absolute_path)?;
        specs.push(FileMeta {
            rel_path: rel_path.clone(),
            size_bytes,
            checksum,
        });
    }

    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tracel-cli-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("nested")).unwrap();
        dir
    }

    #[test]
    fn given_nested_directory_when_collect_files_then_returns_all_files_with_forward_slash_rel_paths()
     {
        let dir = make_temp_dir("collect-files");
        fs::write(dir.join("a.txt"), b"a").unwrap();
        fs::write(dir.join("nested").join("b.txt"), b"b").unwrap();

        let files = collect_files(&dir).unwrap();

        assert_eq!(files.len(), 2);
        assert!(files.contains_key("a.txt"));
        assert!(files.contains_key("nested/b.txt"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn given_empty_directory_when_collect_files_then_returns_error() {
        let dir = make_temp_dir("collect-files-empty");

        let result = collect_files(&dir);

        assert!(result.is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn given_known_content_when_build_file_specs_then_computes_correct_sha256_and_size() {
        let dir = make_temp_dir("build-file-specs");
        fs::write(dir.join("hello.txt"), b"hello world").unwrap();

        let mut files = BTreeMap::new();
        files.insert("hello.txt".to_string(), dir.join("hello.txt"));

        let specs = build_file_specs(&files).unwrap();

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].rel_path, "hello.txt");
        assert_eq!(specs[0].size_bytes, 11);
        assert_eq!(
            specs[0].checksum,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );

        fs::remove_dir_all(&dir).unwrap();
    }
}
