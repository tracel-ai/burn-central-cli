use semver::Version;
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

const CRATE_NAME: &str = env!("CARGO_CRATE_NAME");
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const CHECK_INTERVAL: Duration = Duration::from_hours(24);

#[derive(Deserialize)]
struct CratesIoResponse {
    #[serde(rename = "crate")]
    crate_info: CrateInfo,
}

#[derive(Deserialize)]
struct CrateInfo {
    max_version: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct VersionCache {
    last_check: SystemTime,
    latest_version: String,
}

pub struct VersionChecker {
    cache_path: PathBuf,
}

impl VersionChecker {
    pub fn new() -> Option<Self> {
        let proj_dirs = directories::ProjectDirs::from("com", "tracel", "burncentral")?;
        let cache_dir = proj_dirs.cache_dir();
        fs::create_dir_all(cache_dir).ok()?;

        Some(Self {
            cache_path: cache_dir.join("version_check.json"),
        })
    }

    fn load_cache(&self) -> Option<VersionCache> {
        let contents = fs::read_to_string(&self.cache_path).ok()?;
        serde_json::from_str(&contents).ok()
    }

    fn save_cache(&self, cache: &VersionCache) {
        if let Ok(json) = serde_json::to_string(cache) {
            let _ = fs::write(&self.cache_path, json);
        }
    }

    fn should_check(&self) -> bool {
        match self.load_cache() {
            Some(cache) => {
                if let Ok(elapsed) = SystemTime::now().duration_since(cache.last_check) {
                    elapsed > CHECK_INTERVAL
                } else {
                    true
                }
            }
            None => true,
        }
    }

    fn fetch_latest_version(&self) -> Option<String> {
        let url = format!("https://crates.io/api/v1/crates/{}", CRATE_NAME);

        let response = ureq::get(&url)
            .config()
            .timeout_global(Some(std::time::Duration::from_secs(5)))
            .build()
            .call()
            .ok()?;

        let crates_response: CratesIoResponse = response.into_body().read_json().ok()?;
        Some(crates_response.crate_info.max_version)
    }

    pub fn check_for_updates(&self) -> Option<String> {
        if !self.should_check() {
            if let Some(cache) = self.load_cache() {
                if Self::is_newer_version(&cache.latest_version, CURRENT_VERSION) {
                    return Some(cache.latest_version);
                }
            }
            return None;
        }

        if let Some(latest) = self.fetch_latest_version() {
            let cache = VersionCache {
                last_check: SystemTime::now(),
                latest_version: latest.clone(),
            };
            self.save_cache(&cache);

            if Self::is_newer_version(&latest, CURRENT_VERSION) {
                return Some(latest);
            }
        }

        None
    }

    fn is_newer_version(latest: &str, current: &str) -> bool {
        let Ok(latest_version) = Version::parse(latest) else {
            return false;
        };
        let Ok(current_version) = Version::parse(current) else {
            return false;
        };

        latest_version.cmp_precedence(&current_version) == std::cmp::Ordering::Greater
    }

    pub fn print_update_notification(
        latest_version: &str,
        terminal: &crate::tools::terminal::Terminal,
    ) {
        use colored::Colorize;

        println!();
        terminal.print_warning(&format!(
            "A new version of {CRATE_NAME} is available: {} → {}",
            CURRENT_VERSION.dimmed(),
            latest_version.bright_green()
        ));
        terminal.finalize_info(&format!(
            "Update with: {}",
            format!("cargo install {CRATE_NAME}").bright_cyan()
        ));
    }
}
