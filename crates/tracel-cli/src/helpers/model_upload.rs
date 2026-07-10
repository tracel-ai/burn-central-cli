//! Helpers for `tracel model upload`: directory walking, checksums, model
//! existence check, and multi-threaded fail-fast multipart part upload.

use anyhow::Context;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, mpsc};
use tracel_client::Client;
use tracel_client::ClientError;
use tracel_client::request::CreateModelRequest;
use tracel_client::response::PresignedModelFileUploadUrlsResponse;

use crate::context::CliContext;

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
    use std::io::Read;

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

pub fn ensure_model_exists(
    context: &CliContext,
    client: &Client,
    namespace: &str,
    project: &str,
    model_name: &str,
) -> anyhow::Result<()> {
    match client.get_model(namespace, project, model_name) {
        Ok(_) => return Ok(()),
        Err(e) if e.is_not_found() => {}
        Err(e) => anyhow::bail!("Failed to check model '{model_name}': {e}"),
    }

    let create = context.terminal().confirm(&format!(
        "Model '{model_name}' does not exist in {namespace}/{project}. Create it now?"
    ))?;

    if !create {
        anyhow::bail!("Model upload cancelled: model '{model_name}' does not exist.");
    }

    let description = cliclack::input("Enter model description (optional)")
        .required(false)
        .interact::<String>()
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    client.create_model(
        namespace,
        project,
        CreateModelRequest {
            name: model_name.to_string(),
            description,
        },
    )?;

    context.terminal().print_success(&format!(
        "Created model '{model_name}' in {namespace}/{project}."
    ));

    Ok(())
}

#[derive(Clone, Debug)]
pub struct PartUploadTask {
    pub rel_path: String,
    pub absolute_path: PathBuf,
    pub part: u32,
    pub url: String,
    pub offset: u64,
    pub size_bytes: u64,
}

pub fn build_part_tasks(
    files: &BTreeMap<String, PathBuf>,
    upload_files: &[PresignedModelFileUploadUrlsResponse],
) -> anyhow::Result<Vec<PartUploadTask>> {
    let mut tasks = Vec::new();

    for upload_file in upload_files {
        let absolute_path = files.get(&upload_file.rel_path).ok_or_else(|| {
            anyhow::anyhow!(
                "Upload response referenced unknown file path '{}'.",
                upload_file.rel_path
            )
        })?;

        if upload_file.urls.parts.is_empty() {
            anyhow::bail!(
                "Upload response for '{}' does not contain any part.",
                upload_file.rel_path
            );
        }
        let mut parts = upload_file.urls.parts.clone();
        parts.sort_by_key(|part| part.part);

        let mut offset = 0u64;
        for part in parts {
            tasks.push(PartUploadTask {
                rel_path: upload_file.rel_path.clone(),
                absolute_path: absolute_path.clone(),
                part: part.part,
                url: part.url,
                offset,
                size_bytes: part.size_bytes,
            });
            offset += part.size_bytes;
        }
    }

    Ok(tasks)
}

const UPLOAD_WORKER_COUNT: usize = 8;

pub trait PartUploader: Send + Sync {
    fn upload(&self, url: &str, bytes: Vec<u8>) -> Result<(), ClientError>;
}

impl PartUploader for tracel_client::Client {
    fn upload(&self, url: &str, bytes: Vec<u8>) -> Result<(), ClientError> {
        self.upload_bytes_to_url(url, bytes)
    }
}

fn read_part_bytes(path: &Path, offset: u64, size: u64) -> anyhow::Result<Vec<u8>> {
    let mut file =
        File::open(path).with_context(|| format!("Failed to open file '{}'.", path.display()))?;
    file.seek(SeekFrom::Start(offset))
        .with_context(|| format!("Failed to seek in file '{}'.", path.display()))?;

    let mut buffer = vec![0u8; size as usize];
    file.read_exact(&mut buffer)
        .with_context(|| format!("Failed to read part from file '{}'.", path.display()))?;

    Ok(buffer)
}

enum UploadEvent {
    PartUploaded,
    PartFailed(String),
}

pub fn upload_parts(
    context: &CliContext,
    uploader: &(impl PartUploader + Sync),
    tasks: Vec<PartUploadTask>,
) -> anyhow::Result<()> {
    let total_parts = tasks.len();
    let queue = Mutex::new(tasks);
    let cancelled = AtomicBool::new(false);
    let (event_tx, event_rx) = mpsc::channel::<UploadEvent>();

    let multi = cliclack::multi_progress(format!(
        "Uploading {total_parts} part(s) with {UPLOAD_WORKER_COUNT} worker(s)"
    ));
    let bar = multi.add(cliclack::ProgressBar::new(total_parts as u64).with_download_template());
    bar.start(format!("Uploading (0/{total_parts})"));

    std::thread::scope(|scope| {
        for _ in 0..UPLOAD_WORKER_COUNT.min(total_parts.max(1)) {
            let queue = &queue;
            let cancelled = &cancelled;
            let event_tx = event_tx.clone();
            let uploader = &uploader;

            scope.spawn(move || {
                loop {
                    if cancelled.load(Ordering::SeqCst) {
                        break;
                    }

                    let task = {
                        let mut queue = queue.lock().unwrap();
                        queue.pop()
                    };

                    let Some(task) = task else { break };

                    let result = read_part_bytes(&task.absolute_path, task.offset, task.size_bytes)
                        .and_then(|bytes| {
                            uploader
                                .upload(&task.url, bytes)
                                .map_err(|e| anyhow::anyhow!(e))
                        });

                    match result {
                        Ok(()) => {
                            let _ = event_tx.send(UploadEvent::PartUploaded);
                        }
                        Err(e) => {
                            cancelled.store(true, Ordering::SeqCst);
                            let _ = event_tx.send(UploadEvent::PartFailed(format!(
                                "{} (part {}): {e}",
                                task.rel_path, task.part
                            )));
                        }
                    }
                }
            });
        }

        drop(event_tx);

        let mut completed = 0u64;
        let mut failure: Option<String> = None;

        while let Ok(event) = event_rx.recv() {
            match event {
                UploadEvent::PartUploaded => {
                    completed += 1;
                    bar.inc(1);
                    bar.set_message(format!("Uploading ({completed}/{total_parts})"));
                }
                UploadEvent::PartFailed(msg) => {
                    if failure.is_none() {
                        failure = Some(msg);
                    }
                }
            }
        }

        match failure {
            Some(msg) => {
                bar.error(format!("Upload failed: {msg}"));
                multi.error("Model upload failed");
                anyhow::bail!("Failed to upload model file part: {msg}")
            }
            None => {
                bar.stop(format!("Uploaded {total_parts}/{total_parts}"));
                multi.stop();
                Ok(())
            }
        }
    })?;

    let _ = context;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tracel_client::response::{
        MultipartUploadResponse, PresignedModelFileUploadUrlsResponse, PresignedUploadUrlResponse,
    };

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

    #[test]
    fn given_multi_part_file_when_build_part_tasks_then_computes_sequential_offsets() {
        let mut files = BTreeMap::new();
        files.insert("weights.bin".to_string(), PathBuf::from("/tmp/weights.bin"));
        let upload_files = vec![PresignedModelFileUploadUrlsResponse {
            rel_path: "weights.bin".to_string(),
            urls: MultipartUploadResponse {
                id: "upload-1".to_string(),
                parts: vec![
                    PresignedUploadUrlResponse {
                        part: 2,
                        url: "https://example.com/part2".to_string(),
                        size_bytes: 100,
                    },
                    PresignedUploadUrlResponse {
                        part: 1,
                        url: "https://example.com/part1".to_string(),
                        size_bytes: 200,
                    },
                ],
            },
        }];

        let tasks = build_part_tasks(&files, &upload_files).unwrap();

        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].part, 1);
        assert_eq!(tasks[0].offset, 0);
        assert_eq!(tasks[0].size_bytes, 200);
        assert_eq!(tasks[1].part, 2);
        assert_eq!(tasks[1].offset, 200);
        assert_eq!(tasks[1].size_bytes, 100);
    }

    #[test]
    fn given_unknown_rel_path_when_build_part_tasks_then_returns_error() {
        let files: BTreeMap<String, PathBuf> = BTreeMap::new();

        let upload_files = vec![PresignedModelFileUploadUrlsResponse {
            rel_path: "missing.bin".to_string(),
            urls: MultipartUploadResponse {
                id: "upload-1".to_string(),
                parts: vec![PresignedUploadUrlResponse {
                    part: 1,
                    url: "https://example.com/part1".to_string(),
                    size_bytes: 10,
                }],
            },
        }];

        let result = build_part_tasks(&files, &upload_files);

        assert!(result.is_err());
    }

    use std::sync::Mutex as StdMutex;

    struct FakeUploader {
        calls: StdMutex<Vec<String>>,
        fail_url: Option<String>,
    }

    impl FakeUploader {
        fn new(fail_url: Option<&str>) -> Self {
            Self {
                calls: StdMutex::new(Vec::new()),
                fail_url: fail_url.map(str::to_string),
            }
        }
    }

    impl PartUploader for FakeUploader {
        fn upload(&self, url: &str, _bytes: Vec<u8>) -> Result<(), tracel_client::ClientError> {
            self.calls.lock().unwrap().push(url.to_string());
            if self.fail_url.as_deref() == Some(url) {
                return Err(tracel_client::ClientError::NotFound);
            }
            Ok(())
        }
    }

    fn make_task(
        dir: &Path,
        rel_path: &str,
        part: u32,
        url: &str,
        content: &[u8],
    ) -> PartUploadTask {
        let path = dir.join(rel_path);
        fs::write(&path, content).unwrap();
        PartUploadTask {
            rel_path: rel_path.to_string(),
            absolute_path: path,
            part,
            url: url.to_string(),
            offset: 0,
            size_bytes: content.len() as u64,
        }
    }

    #[test]
    fn given_all_parts_succeed_when_upload_parts_then_returns_ok_and_uploads_each_once() {
        let dir = make_temp_dir("upload-parts-ok");
        let tasks = vec![
            make_task(&dir, "a.bin", 1, "https://example.com/a", b"a-data"),
            make_task(&dir, "b.bin", 1, "https://example.com/b", b"b-data"),
        ];
        let uploader = FakeUploader::new(None);
        let context = CliContext::new(
            crate::tools::terminal::Terminal::default(),
            crate::app_config::Environment::Production,
        );

        let result = upload_parts(&context, &uploader, tasks);

        assert!(result.is_ok());
        assert_eq!(uploader.calls.lock().unwrap().len(), 2);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn given_one_part_fails_when_upload_parts_then_returns_error() {
        let dir = make_temp_dir("upload-parts-fail");
        let tasks = vec![
            make_task(&dir, "a.bin", 1, "https://example.com/a", b"a-data"),
            make_task(&dir, "b.bin", 1, "https://example.com/b", b"b-data"),
        ];
        let uploader = FakeUploader::new(Some("https://example.com/b"));
        let context = CliContext::new(
            crate::tools::terminal::Terminal::default(),
            crate::app_config::Environment::Production,
        );

        let result = upload_parts(&context, &uploader, tasks);

        assert!(result.is_err());

        fs::remove_dir_all(&dir).unwrap();
    }
}
