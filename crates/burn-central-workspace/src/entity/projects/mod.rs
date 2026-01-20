use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::entity::projects::burn_dir::{BurnDir, project::BurnCentralProject};
use crate::event::Reporter;
use crate::execution::cancellable::CancellationToken;
use crate::tools::function_discovery::{
    DiscoveryConfig, DiscoveryError, DiscoveryEvent, FunctionDiscovery, PkgId,
};
use crate::tools::functions_registry::FunctionRegistry;

pub mod burn_dir;

#[derive(Debug)]
pub enum ErrorKind {
    ManifestNotFound,
    Parsing,
    BurnDirInitialization,
    BurnDirNotInitialized,
    Unexpected,
}

#[derive(thiserror::Error, Debug)]
pub struct ProjectContextError {
    message: String,
    kind: ErrorKind,
    #[source]
    source: Option<anyhow::Error>,
}

impl ProjectContextError {
    pub fn new(message: String, kind: ErrorKind, source: Option<anyhow::Error>) -> Self {
        Self {
            message,
            kind,
            source,
        }
    }

    pub fn kind(&self) -> &ErrorKind {
        &self.kind
    }

    pub fn is_burn_dir_not_initialized(&self) -> bool {
        matches!(self.kind, ErrorKind::BurnDirNotInitialized)
    }
}

impl std::fmt::Display for ProjectContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

pub struct ProjectContext {
    pub workspace_info: WorkspaceInfo,
    pub build_profile: String,
    pub burn_dir: BurnDir,
    pub project: BurnCentralProject,
    function_registry: Mutex<FunctionRegistry>,
}

pub struct WorkspaceInfo {
    pub workspace_name: String,
    pub workspace_root: PathBuf,
    pub metadata: cargo_metadata::Metadata,
}

impl WorkspaceInfo {
    pub fn load_from_path(manifest_path: &Path) -> Result<Self, ProjectContextError> {
        if !manifest_path.is_file() {
            return Err(ProjectContextError::new(
                format!(
                    "Cargo.toml not found at specified path '{}'",
                    manifest_path.display()
                ),
                ErrorKind::ManifestNotFound,
                None,
            ));
        }
        let metadata = cargo_metadata::MetadataCommand::new()
            .manifest_path(manifest_path)
            .no_deps()
            .exec()
            .map_err(|e| {
                ProjectContextError::new(
                    format!(
                        "Failed to load cargo metadata for manifest at '{}': {}",
                        manifest_path.display(),
                        e
                    ),
                    ErrorKind::Parsing,
                    Some(anyhow::anyhow!(e)),
                )
            })?;

        let workspace_root = metadata.workspace_root.clone().into_std_path_buf();

        // Determine workspace name from workspace Cargo.toml or directory name
        let workspace_toml_path = workspace_root.join("Cargo.toml");
        let workspace_name = if workspace_toml_path.exists() {
            let toml_str = std::fs::read_to_string(&workspace_toml_path).map_err(|e| {
                ProjectContextError::new(
                    format!(
                        "Failed to read workspace Cargo.toml at '{}': {}",
                        workspace_toml_path.display(),
                        e
                    ),
                    ErrorKind::Parsing,
                    Some(anyhow::anyhow!(e)),
                )
            })?;

            let workspace_document = toml::de::from_str::<toml::Value>(&toml_str).map_err(|e| {
                ProjectContextError::new(
                    format!(
                        "Failed to parse workspace Cargo.toml at '{}': {}",
                        workspace_toml_path.display(),
                        e
                    ),
                    ErrorKind::Parsing,
                    Some(anyhow::anyhow!(e)),
                )
            })?;

            // Try to get name from workspace.package.name or package.name
            workspace_document
                .get("workspace")
                .and_then(|ws| ws.get("package"))
                .and_then(|pkg| pkg.get("name"))
                .and_then(|name| name.as_str())
                .or_else(|| {
                    workspace_document
                        .get("package")
                        .and_then(|pkg| pkg.get("name"))
                        .and_then(|name| name.as_str())
                })
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    // Fallback to directory name
                    workspace_root
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("workspace")
                        .to_string()
                })
        } else {
            // Fallback to directory name if workspace Cargo.toml doesn't exist
            workspace_root
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("workspace")
                .to_string()
        };

        Ok(WorkspaceInfo {
            workspace_name,
            workspace_root,
            metadata,
        })
    }

    pub fn get_ws_root(&self) -> PathBuf {
        self.metadata.workspace_root.clone().into_std_path_buf()
    }

    pub fn get_manifest_path(&self) -> PathBuf {
        self.workspace_root.join(PathBuf::from("Cargo.toml"))
    }
}

impl ProjectContext {
    pub fn load_workspace_info(manifest_path: &Path) -> Result<WorkspaceInfo, ProjectContextError> {
        WorkspaceInfo::load_from_path(manifest_path)
    }

    pub fn load(manifest_path: &Path, burn_dir_name: &str) -> Result<Self, ProjectContextError> {
        let workspace_info = WorkspaceInfo::load_from_path(manifest_path)?;
        let burn_dir_root = workspace_info
            .workspace_root
            .join(PathBuf::from(burn_dir_name));
        let burn_dir = BurnDir::new(burn_dir_root);
        burn_dir.init().map_err(|e| {
            ProjectContextError::new(
                "Failed to initialize Burn directory".to_string(),
                ErrorKind::BurnDirInitialization,
                Some(e.into()),
            )
        })?;

        let project = burn_dir
            .load_project()
            .map_err(|e| {
                ProjectContextError::new(
                    "Failed to load project metadata from Burn directory".to_string(),
                    ErrorKind::BurnDirNotInitialized,
                    Some(e.into()),
                )
            })?
            .ok_or_else(|| {
                ProjectContextError::new(
                    "No Burn Central project linked to this repository".to_string(),
                    ErrorKind::BurnDirNotInitialized,
                    None,
                )
            })?;

        Ok(Self {
            workspace_info,
            build_profile: "release".to_string(),
            burn_dir,
            project,
            function_registry: Mutex::new(Default::default()),
        })
    }

    pub fn init(
        project: BurnCentralProject,
        manifest_path: &Path,
        burn_dir_name: &str,
    ) -> Result<Self, ProjectContextError> {
        let workspace_info = WorkspaceInfo::load_from_path(manifest_path)?;

        let burn_dir_root = workspace_info
            .workspace_root
            .join(PathBuf::from(burn_dir_name));
        let burn_dir = BurnDir::new(burn_dir_root);
        burn_dir.init().map_err(|e| {
            ProjectContextError::new(
                "Failed to initialize Burn directory".to_string(),
                ErrorKind::BurnDirInitialization,
                Some(e.into()),
            )
        })?;

        burn_dir.save_project(&project).map_err(|e| {
            ProjectContextError::new(
                "Failed to save project metadata to Burn directory".to_string(),
                ErrorKind::BurnDirInitialization,
                Some(e.into()),
            )
        })?;

        Ok(Self {
            workspace_info,
            build_profile: "release".to_string(),
            burn_dir,
            project: project.clone(),
            function_registry: Mutex::new(Default::default()),
        })
    }

    pub fn unlink(manifest_path: &Path, burn_dir_name: &str) -> Result<(), ProjectContextError> {
        let workspace_info = WorkspaceInfo::load_from_path(manifest_path)?;

        let burn_dir_root = workspace_info
            .workspace_root
            .join(PathBuf::from(burn_dir_name));
        let burn_dir = BurnDir::new(burn_dir_root);

        std::fs::remove_dir_all(burn_dir.root()).map_err(|e| {
            ProjectContextError::new(
                "Failed to remove Burn directory".to_string(),
                ErrorKind::Unexpected,
                Some(e.into()),
            )
        })?;

        Ok(())
    }

    pub fn get_project(&self) -> &BurnCentralProject {
        &self.project
    }

    pub fn get_workspace_name(&self) -> &str {
        &self.workspace_info.workspace_name
    }

    pub fn get_workspace_path(&self) -> &Path {
        &self.workspace_info.workspace_root
    }

    pub fn get_workspace_root(&self) -> &Path {
        &self.workspace_info.workspace_root
    }

    pub fn get_manifest_path(&self) -> PathBuf {
        self.workspace_info.get_manifest_path()
    }

    pub fn burn_dir(&self) -> &BurnDir {
        &self.burn_dir
    }

    pub fn cwd(&self) -> &Path {
        &self.workspace_info.workspace_root
    }

    pub fn load_functions(
        &self,
        reporter: Option<Arc<dyn Reporter<DiscoveryEvent>>>,
    ) -> Result<FunctionRegistry, DiscoveryError> {
        let token = CancellationToken::new();
        self.load_functions_cancellable(&token, reporter)
    }

    pub fn load_functions_cancellable(
        &self,
        cancel_token: &CancellationToken,
        reporter: Option<Arc<dyn Reporter<DiscoveryEvent>>>,
    ) -> Result<FunctionRegistry, DiscoveryError> {
        let mut functions = self.function_registry.lock().unwrap();
        if functions.is_empty() {
            // Discover all workspace packages
            let workspace_pkgids = self.workspace_info.metadata.workspace_members.clone();
            let workspace_packages: Vec<_> = self
                .workspace_info
                .metadata
                .packages
                .iter()
                .filter(|pkg| workspace_pkgids.contains(&pkg.id))
                .collect();

            let pkgids = workspace_packages
                .iter()
                .map(|pkg| PkgId {
                    name: pkg.name.to_string(),
                    version: Some(pkg.version.to_string()),
                })
                .collect::<Vec<_>>();

            let config = DiscoveryConfig {
                packages: pkgids,
                target_dir: Some(self.burn_dir.target_dir()),
            };

            let result = FunctionDiscovery::new(self.get_workspace_root()).discover_functions(
                &config,
                cancel_token,
                reporter,
            )?;

            let mut registry = FunctionRegistry::new();
            for (pkgid, funcs) in result.functions.into_iter() {
                let package = workspace_packages
                    .iter()
                    .find(|pkg| pkg.name.as_str() == pkgid.name)
                    .expect("Discovered package should be in workspace");
                registry
                    .get_or_create_package_entry((*package).clone())
                    .extend(funcs);
            }

            *functions = registry;
        }
        Ok(functions.clone())
    }

    pub fn get_workspace_packages(&self) -> Vec<&cargo_metadata::Package> {
        self.workspace_info
            .metadata
            .packages
            .iter()
            .filter(|pkg| {
                self.workspace_info
                    .metadata
                    .workspace_members
                    .contains(&pkg.id)
            })
            .collect()
    }

    pub fn find_package_by_name(&self, name: &str) -> Option<&cargo_metadata::Package> {
        self.get_workspace_packages()
            .into_iter()
            .find(|pkg| pkg.name.as_str() == name)
    }
}
