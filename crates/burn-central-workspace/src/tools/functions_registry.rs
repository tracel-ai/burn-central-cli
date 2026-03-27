use std::{collections::HashMap, fmt::Display};

use cargo_metadata::Package;

use crate::{execution::ProcedureType, tools::function_discovery::FunctionMetadata};

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
        results.sort_by(|a, b| a.1.name.cmp(&b.1.name));
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

    /// Return a new registry containing only functions matching the requested procedure type.
    pub fn filter_by_type(&self, procedure_type: ProcedureType) -> Self {
        let expected_proc_type = procedure_type.to_string();
        let functions = self
            .functions
            .iter()
            .filter_map(|(package, functions)| {
                let matching_functions = functions
                    .iter()
                    .filter(|function| function.proc_type == expected_proc_type)
                    .cloned()
                    .collect::<Vec<_>>();

                if matching_functions.is_empty() {
                    None
                } else {
                    Some((package.clone(), matching_functions))
                }
            })
            .collect();

        Self { functions }
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
        let mut functions = self
            .functions
            .iter()
            .flat_map(|(package, funcs)| {
                funcs.iter().map(move |f| FunctionId {
                    package_name: package.name.to_string(),
                    function_name: f.routine_name.clone(),
                })
            })
            .collect::<Vec<_>>();

        functions.sort_by(|a, b| a.package_name.cmp(&b.package_name));
        functions
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_package() -> Package {
        cargo_metadata::MetadataCommand::new()
            .manifest_path(format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR")))
            .no_deps()
            .exec()
            .unwrap()
            .packages
            .into_iter()
            .find(|package| package.name.as_str() == env!("CARGO_PKG_NAME"))
            .unwrap()
    }

    fn test_function(routine_name: &str, proc_type: &str) -> FunctionMetadata {
        FunctionMetadata {
            mod_path: "crate::module".to_string(),
            fn_name: routine_name.to_string(),
            builder_fn_name: format!("__{}_builder", routine_name),
            routine_name: routine_name.to_string(),
            proc_type: proc_type.to_string(),
            token_stream: Vec::new(),
        }
    }

    #[test]
    fn filter_by_procedure_type_keeps_only_matching_functions() {
        let package = test_package();
        let mut registry = FunctionRegistry::new();
        registry
            .get_or_create_package_entry(package.clone())
            .extend([
                test_function("train_model", "training"),
                test_function("run_inference", "inference"),
            ]);

        let training_registry = registry.filter_by_type(ProcedureType::Training);

        assert_eq!(training_registry.num_functions(), 1);
        assert!(training_registry.has_function("train_model"));
        assert!(!training_registry.has_function("run_inference"));

        let package_functions = training_registry.get_package_functions(&package).unwrap();
        assert_eq!(package_functions.len(), 1);
        assert_eq!(package_functions[0].routine_name, "train_model");
        assert_eq!(package_functions[0].proc_type, "training");
    }
}
