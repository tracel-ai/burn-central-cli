use std::collections::BTreeMap;
use std::path::PathBuf;
use tracel_client::response::{PresignedModelFileUploadUrlsResponse, PresignedUploadUrlResponse};

#[derive(Clone, Debug)]
pub struct PartUploadTask {
    pub rel_path: String,
    pub absolute_path: PathBuf,
    pub part: u32,
    pub url: String,
    pub offset: u64,
    pub size_bytes: u64,
}

fn validate_part_sequence(rel_path: &str, parts: &[PresignedUploadUrlResponse]) -> anyhow::Result<()> {
    for (index, part) in parts.iter().enumerate() {
        let expected = index as u32 + 1;
        if part.part != expected {
            anyhow::bail!(
                "Upload response for '{rel_path}' has non-contiguous or duplicate part numbers (expected part {expected}, got {}).",
                part.part
            );
        }
    }
    Ok(())
}

fn validate_part_total_size(
    rel_path: &str,
    parts: &[PresignedUploadUrlResponse],
    expected_size: u64,
) -> anyhow::Result<()> {
    let total: u64 = parts.iter().map(|part| part.size_bytes).sum();
    if total != expected_size {
        anyhow::bail!(
            "Upload response for '{rel_path}' parts sum to {total} bytes but local file is {expected_size} bytes."
        );
    }
    Ok(())
}

pub fn build_part_tasks(
    files: &BTreeMap<String, PathBuf>,
    file_sizes: &BTreeMap<String, u64>,
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
        let expected_size = *file_sizes.get(&upload_file.rel_path).ok_or_else(|| {
            anyhow::anyhow!(
                "Upload response referenced file '{}' with no known local size.",
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
        validate_part_sequence(&upload_file.rel_path, &parts)?;
        validate_part_total_size(&upload_file.rel_path, &parts, expected_size)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use tracel_client::response::MultipartUploadResponse;

    #[test]
    fn given_multi_part_file_when_build_part_tasks_then_computes_sequential_offsets() {
        let mut files = BTreeMap::new();
        files.insert("weights.bin".to_string(), PathBuf::from("/tmp/weights.bin"));
        let mut file_sizes = BTreeMap::new();
        file_sizes.insert("weights.bin".to_string(), 300u64);
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

        let tasks = build_part_tasks(&files, &file_sizes, &upload_files).unwrap();

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
        let file_sizes: BTreeMap<String, u64> = BTreeMap::new();

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

        let result = build_part_tasks(&files, &file_sizes, &upload_files);

        assert!(result.is_err());
    }

    #[test]
    fn given_parts_summing_to_less_than_local_size_when_build_part_tasks_then_returns_error() {
        let mut files = BTreeMap::new();
        files.insert("weights.bin".to_string(), PathBuf::from("/tmp/weights.bin"));
        let mut file_sizes = BTreeMap::new();
        file_sizes.insert("weights.bin".to_string(), 500u64);

        let upload_files = vec![PresignedModelFileUploadUrlsResponse {
            rel_path: "weights.bin".to_string(),
            urls: MultipartUploadResponse {
                id: "upload-1".to_string(),
                parts: vec![PresignedUploadUrlResponse {
                    part: 1,
                    url: "https://example.com/part1".to_string(),
                    size_bytes: 300,
                }],
            },
        }];

        let result = build_part_tasks(&files, &file_sizes, &upload_files);

        assert!(result.is_err());
    }

    #[test]
    fn given_duplicate_part_number_when_build_part_tasks_then_returns_error() {
        let mut files = BTreeMap::new();
        files.insert("weights.bin".to_string(), PathBuf::from("/tmp/weights.bin"));
        let mut file_sizes = BTreeMap::new();
        file_sizes.insert("weights.bin".to_string(), 400u64);

        let upload_files = vec![PresignedModelFileUploadUrlsResponse {
            rel_path: "weights.bin".to_string(),
            urls: MultipartUploadResponse {
                id: "upload-1".to_string(),
                parts: vec![
                    PresignedUploadUrlResponse {
                        part: 1,
                        url: "https://example.com/part1".to_string(),
                        size_bytes: 200,
                    },
                    PresignedUploadUrlResponse {
                        part: 1,
                        url: "https://example.com/part1b".to_string(),
                        size_bytes: 200,
                    },
                ],
            },
        }];

        let result = build_part_tasks(&files, &file_sizes, &upload_files);

        assert!(result.is_err());
    }

    #[test]
    fn given_gap_in_part_numbers_when_build_part_tasks_then_returns_error() {
        let mut files = BTreeMap::new();
        files.insert("weights.bin".to_string(), PathBuf::from("/tmp/weights.bin"));
        let mut file_sizes = BTreeMap::new();
        file_sizes.insert("weights.bin".to_string(), 300u64);

        let upload_files = vec![PresignedModelFileUploadUrlsResponse {
            rel_path: "weights.bin".to_string(),
            urls: MultipartUploadResponse {
                id: "upload-1".to_string(),
                parts: vec![
                    PresignedUploadUrlResponse {
                        part: 1,
                        url: "https://example.com/part1".to_string(),
                        size_bytes: 200,
                    },
                    PresignedUploadUrlResponse {
                        part: 3,
                        url: "https://example.com/part3".to_string(),
                        size_bytes: 100,
                    },
                ],
            },
        }];

        let result = build_part_tasks(&files, &file_sizes, &upload_files);

        assert!(result.is_err());
    }
}
