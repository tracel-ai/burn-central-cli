use std::collections::HashMap;

use cargo_metadata::Package;

use crate::tools::function_discovery::FunctionMetadata;

/// Registry mapping packages to the functions they register. Allows querying which package(s) provide a given function.
#[derive(Debug, Clone, Default)]
pub struct FunctionRegistry {
    functions: HashMap<Package, Vec<FunctionMetadata>>,
}

impl FunctionRegistry {
    pub fn new() -> Self {
        Self {
            functions: HashMap::new(),
        }
    }

    /// Get the list of functions registered by the given package, creating an entry if it does not exist.
    pub fn get_or_create_package_entry(&mut self, package: Package) -> &mut Vec<FunctionMetadata> {
        self.functions.entry(package).or_default()
    }

    /// Attempt to find the package that contain a function with the given name. If multiple packages contain the function, all are returned.
    pub fn find_package_from_function(&self, function_name: &str) -> Vec<Package> {
        self.functions
            .iter()
            .filter_map(|(package, functions)| {
                if functions.iter().any(|f| f.routine_name == function_name) {
                    Some(package)
                } else {
                    None
                }
            })
            .cloned()
            .collect()
    }

    /// Check if the registry is empty, meaning no functions have been registered by any package.
    pub fn is_empty(&self) -> bool {
        self.functions.is_empty()
    }

    /// Get the total number of functions registered across all packages.
    pub fn num_functions(&self) -> usize {
        self.functions.values().map(|v| v.len()).sum()
    }

    pub fn get_function_references(&self) -> &[FunctionMetadata] {
        // backward compatibility: return functions from the first package found
        self.functions
            .values()
            .next()
            .expect("Should have at least one package")
    }
}
