use std::{collections::HashMap, fmt::Display};

use cargo_metadata::Package;

use crate::tools::function_discovery::FunctionMetadata;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FunctionId {
    pub package_name: String,
    pub function_name: String,
}

impl FunctionId {
    pub fn new(package_name: &str, function_name: &str) -> Self {
        Self {
            package_name: package_name.to_string(),
            function_name: function_name.to_string(),
        }
    }
}

impl Display for FunctionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}::{}", self.package_name, self.function_name)
    }
}

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

    /// Attempt to find the package that contain a function with the given name. If multiple packages contain the function, all are returned with their corresponding metadata.
    pub fn find_packages_for_function_name(
        &self,
        function_name: &str,
    ) -> Vec<(FunctionMetadata, Package)> {
        let mut results = Vec::new();
        for (package, functions) in &self.functions {
            for function in functions {
                if function.routine_name == function_name {
                    results.push((function.clone(), package.clone()));
                }
            }
        }
        results
    }

    /// Checks if a function with the given name is registered by any package.
    pub fn has_function(&self, function_name: &str) -> bool {
        for functions in self.functions.values() {
            for function in functions {
                if function.routine_name == function_name {
                    return true;
                }
            }
        }
        false
    }

    /// Get the list of functions registered by the given package name.
    pub fn get_package_functions_by_name(
        &self,
        package_name: &str,
    ) -> Option<&Vec<FunctionMetadata>> {
        for (package, functions) in &self.functions {
            if package.name.as_str() == package_name {
                return Some(functions);
            }
        }
        None
    }

    pub fn get_package_functions(&self, package: &Package) -> Option<&Vec<FunctionMetadata>> {
        self.functions.get(package)
    }

    /// Check if the registry is empty, meaning no functions have been registered by any package.
    pub fn is_empty(&self) -> bool {
        self.functions.is_empty()
    }

    /// Get the total number of functions registered across all packages.
    pub fn num_functions(&self) -> usize {
        self.functions.values().map(|v| v.len()).sum()
    }

    /// Get a list of all FunctionIds registered in the registry.
    pub fn get_function_ids(&self) -> Vec<FunctionId> {
        self.functions
            .iter()
            .flat_map(|(package, funcs)| {
                funcs.iter().map(move |f| FunctionId {
                    package_name: package.name.to_string(),
                    function_name: f.routine_name.clone(),
                })
            })
            .collect()
    }

    /// Get function metadata by its FunctionId.
    pub fn get_function_by_id(&self, id: &FunctionId) -> Option<FunctionMetadata> {
        for (package, functions) in &self.functions {
            if package.name.as_str() == id.package_name {
                for function in functions {
                    if function.routine_name == id.function_name {
                        return Some(function.clone());
                    }
                }
            }
        }
        None
    }

    /// Get function metadata along with its package by FunctionId.
    pub fn get_package_function_pair_by_id(
        &self,
        id: &FunctionId,
    ) -> Option<(FunctionMetadata, Package)> {
        for (package, functions) in &self.functions {
            if package.name.as_str() == id.package_name {
                for function in functions {
                    if function.routine_name == id.function_name {
                        return Some((function.clone(), package.clone()));
                    }
                }
            }
        }
        None
    }

    /// Get a list of all registered functions across all packages.
    pub fn get_functions(&self) -> Vec<FunctionMetadata> {
        self.functions
            .values()
            .flat_map(|funcs| funcs.iter().cloned())
            .collect()
    }
}
