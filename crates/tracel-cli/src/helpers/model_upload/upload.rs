use anyhow::Context;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tracel_client::ClientError;

use super::parts::PartUploadTask;

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

/// Splits `tasks` into up to `worker_count` contiguous chunks, one per worker thread.
fn chunk_tasks(tasks: Vec<PartUploadTask>, worker_count: usize) -> Vec<Vec<PartUploadTask>> {
    if tasks.is_empty() {
        return Vec::new();
    }

    let chunk_size = tasks.len().div_ceil(worker_count);
    let mut iter = tasks.into_iter();
    let mut chunks = Vec::new();

    loop {
        let chunk: Vec<_> = iter.by_ref().take(chunk_size).collect();
        if chunk.is_empty() {
            break;
        }
        chunks.push(chunk);
    }

    chunks
}

/// Uploads a worker's chunk of parts in order, stopping early if `cancelled` is set by
/// another worker's failure. Bumps `completed` after each successful part so the caller
/// can poll progress without every worker touching the progress bar directly.
fn upload_chunk(
    chunk: Vec<PartUploadTask>,
    uploader: &impl PartUploader,
    cancelled: &AtomicBool,
    completed: &AtomicU64,
) -> anyhow::Result<()> {
    for task in chunk {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }

        let result =
            read_part_bytes(&task.absolute_path, task.offset, task.size_bytes).and_then(|bytes| {
                uploader
                    .upload(&task.url, bytes)
                    .map_err(|e| anyhow::anyhow!(e))
            });

        match result {
            Ok(()) => {
                completed.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                cancelled.store(true, Ordering::Relaxed);
                anyhow::bail!("{} (part {}): {e}", task.rel_path, task.part);
            }
        }
    }

    Ok(())
}

pub fn upload_parts(
    uploader: &impl PartUploader,
    tasks: Vec<PartUploadTask>,
) -> anyhow::Result<()> {
    let total_parts = tasks.len();
    let worker_count = UPLOAD_WORKER_COUNT.min(total_parts.max(1));
    let chunks = chunk_tasks(tasks, worker_count);

    let cancelled = AtomicBool::new(false);
    let completed = AtomicU64::new(0);

    let multi = cliclack::multi_progress(format!(
        "Uploading {total_parts} part(s) with {worker_count} worker(s)"
    ));
    let bar = multi.add(cliclack::ProgressBar::new(total_parts as u64).with_download_template());
    bar.start(format!("Uploading (0/{total_parts})"));

    let failure = std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                let cancelled = &cancelled;
                let completed = &completed;
                scope.spawn(move || upload_chunk(chunk, uploader, cancelled, completed))
            })
            .collect();

        let mut reported = 0u64;
        while handles.iter().any(|handle| !handle.is_finished()) {
            let done = completed.load(Ordering::Relaxed);
            if done > reported {
                bar.inc(done - reported);
                bar.set_message(format!("Uploading ({done}/{total_parts})"));
                reported = done;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        // Each worker thread's Result is joined explicitly here (rather than relying on
        // thread::scope's implicit join-on-exit), so a worker panic is turned into a clean
        // anyhow::Error instead of unwinding past the error-reporting below.
        handles
            .into_iter()
            .filter_map(|handle| match handle.join() {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(e),
                Err(_) => Some(anyhow::anyhow!("An upload worker thread panicked.")),
            })
            .next()
    });

    let done = completed.load(Ordering::Relaxed);
    bar.set_message(format!("Uploading ({done}/{total_parts})"));

    match failure {
        Some(e) => {
            bar.error(format!("Upload failed: {e}"));
            multi.error("Model upload failed");
            Err(e.context("Failed to upload model file part"))
        }
        None => {
            bar.stop(format!("Uploaded {total_parts}/{total_parts}"));
            multi.stop();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;

    fn make_temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tracel-cli-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("nested")).unwrap();
        dir
    }

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

        let result = upload_parts(&uploader, tasks);

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

        let result = upload_parts(&uploader, tasks);

        assert!(result.is_err());

        fs::remove_dir_all(&dir).unwrap();
    }
}
