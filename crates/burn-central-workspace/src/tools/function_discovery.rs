//! Build-time function discovery using cargo rustc macro expansion
//!
//! Uses `cargo rustc -- -Zunpretty=expanded` to extract `BCFN1|mod_path|fn|builder|routine|proc_type|END` markers from the expanded source code.

use crate::execution::cancellable::{CancellableProcess, CancellableResult, CancellationToken};
use quote::ToTokens;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::{Command, Stdio};

const MAGIC: &str = "BCFN1|";
const END: &str = "|END";
const SEP: char = '|';

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionMetadata {
    pub mod_path: String,
    pub fn_name: String,
    pub builder_fn_name: String,
    pub routine_name: String,
    pub proc_type: String,
    pub token_stream: Vec<u8>,
}

impl FunctionMetadata {
    pub fn get_function_code(&self) -> String {
        if self.token_stream.is_empty() {
            // If no token stream is available, create a placeholder function
            format!(
                "fn {}() {{\n    // Function implementation not available\n}}",
                self.fn_name
            )
        } else {
            // Try to decode as UTF-8 string first (new format with original source)
            if let Ok(source_code) = std::str::from_utf8(&self.token_stream) {
                // Check if it looks like Rust source code (not JSON)
                if !source_code.trim_start().starts_with('{') {
                    return source_code.to_string();
                }
            }

            // Fall back to JSON AST deserialization (old format)
            match syn_serde::json::from_slice::<syn::ItemFn>(&self.token_stream) {
                Ok(itemfn) => match syn::parse2(itemfn.into_token_stream()) {
                    Ok(syn_tree) => prettyplease::unparse(&syn_tree),
                    Err(_) => format!(
                        "fn {}() {{\n    // Failed to parse token stream\n}}",
                        self.fn_name
                    ),
                },
                Err(_) => format!(
                    "fn {}() {{\n    // Failed to deserialize token stream\n}}",
                    self.fn_name
                ),
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct FunctionDiscovery {
    project_root: PathBuf,
    target_dir: Option<PathBuf>,
    manifest_path: Option<PathBuf>,
}

impl FunctionDiscovery {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
            target_dir: None,
            manifest_path: None,
        }
    }

    pub fn with_target_dir(mut self, target_dir: impl Into<PathBuf>) -> Self {
        self.target_dir = Some(target_dir.into());
        self
    }

    pub fn with_manifest_path(mut self, manifest_path: impl Into<PathBuf>) -> Self {
        self.manifest_path = Some(manifest_path.into());
        self
    }

    /// Expand and extract with cancellation support
    pub fn discover_functions(
        &self,
        cancellation_token: &CancellationToken,
    ) -> Result<Vec<FunctionMetadata>, String> {
        let expanded = self.expand_with_cargo(cancellation_token)?;
        if cancellation_token.is_cancelled() {
            return Err("Function discovery was cancelled".to_string());
        }
        let functions = parse_expanded_output(&expanded);
        Ok(functions)
    }

    fn expand_with_cargo(&self, cancellation_token: &CancellationToken) -> Result<String, String> {
        let mut cmd = Command::new("cargo");
        cmd.current_dir(&self.project_root)
            .arg("rustc")
            .arg("--lib")
            .arg("--profile=check");

        if let Some(mp) = &self.manifest_path {
            cmd.arg("--manifest-path").arg(mp);
        }
        if let Some(td) = &self.target_dir {
            cmd.arg("--target-dir").arg(td);
        }

        cmd.arg("--");
        cmd.arg("-Zunpretty=expanded");
        cmd.env("RUSTC_BOOTSTRAP", "1");
        cmd.env("RUST_LOG", "error");

        let child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to spawn cargo rustc: {e}"))?;

        let cancellable = CancellableProcess::new(child, cancellation_token.clone());
        let result = cancellable.wait_with_output();

        match result {
            CancellableResult::Completed(output) => {
                if !output.status.success() {
                    return Err(format!("cargo rustc failed (status {})", output.status));
                }
                let expanded = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
                Ok(expanded)
            }
            CancellableResult::Cancelled => Err("Function discovery was cancelled".to_string()),
        }
    }
}

pub fn parse_expanded_output(expanded: &str) -> Vec<FunctionMetadata> {
    let bytes = expanded.as_bytes();
    let mut i = 0usize;
    let mut out = Vec::new();

    while let Some(m) = find(bytes, MAGIC.as_bytes(), i) {
        let start_payload = m + MAGIC.len();
        if let Some(end) = find(bytes, END.as_bytes(), start_payload) {
            if let Ok(slice) = std::str::from_utf8(&bytes[m..end + END.len()]) {
                if let Some(meta) = parse_bcfn_marker(slice) {
                    out.push(meta);
                }
            }
            i = end + END.len();
        } else {
            // no closing sentinel; stop scanning
            break;
        }
    }

    for meta in &mut out {
        let result = extract_ast_token_stream(expanded, &meta.fn_name);
        if let Some(token_stream) = result {
            meta.token_stream = token_stream;
        }
    }

    out
}

/// Expected `BCFN1|mod_path|fn_name|builder|routine|proc_type|END`.
fn parse_bcfn_marker(marker: &str) -> Option<FunctionMetadata> {
    if !marker.starts_with(MAGIC) || !marker.ends_with(END) {
        return None;
    }
    let body = &marker[MAGIC.len()..marker.len() - END.len()];
    let mut it = body.split(SEP);

    let mod_path = it.next()?.to_string();
    let fn_name = it.next()?.to_string();
    let builder_fn_name = it.next()?.to_string();
    let routine_name = it.next()?.to_string();
    let proc_type = it.next()?.to_string();

    // There must be exactly 5 parts.
    if it.next().is_some() {
        return None;
    }

    Some(FunctionMetadata {
        mod_path,
        fn_name,
        builder_fn_name,
        routine_name,
        proc_type,
        token_stream: Vec::new(),
    })
}

/// Naive byte-substring search (no regex).
fn find(hay: &[u8], needle: &[u8], mut from: usize) -> Option<usize> {
    while from + needle.len() <= hay.len() {
        if &hay[from..from + needle.len()] == needle {
            return Some(from);
        }
        from += 1;
    }
    None
}

/// Unescape a Rust byte string literal (without the surrounding b"...")
/// Handles common escape sequences: \", \\, \n, \r, \t
fn unescape_byte_string(escaped: &str) -> Vec<u8> {
    let mut result = Vec::new();
    let mut chars = escaped.chars();

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            // Handle escape sequences
            if let Some(next) = chars.next() {
                match next {
                    '"' => result.push(b'"'),
                    '\\' => result.push(b'\\'),
                    'n' => result.push(b'\n'),
                    'r' => result.push(b'\r'),
                    't' => result.push(b'\t'),
                    // For any other escape, just include it as-is
                    _ => {
                        result.push(b'\\');
                        result.extend(next.to_string().as_bytes());
                    }
                }
            } else {
                // Trailing backslash
                result.push(b'\\');
            }
        } else {
            // Regular character - convert to bytes
            result.extend(ch.to_string().as_bytes());
        }
    }

    result
}

/// Extract the JSON AST from a _BURN_FUNCTION_AST_* constant
/// Pattern: const _BURN_FUNCTION_AST_NAME: &[u8] = b"{...json...}";
fn extract_ast_token_stream(expanded: &str, fn_name: &str) -> Option<Vec<u8>> {
    // Derive the AST constant name from the function name
    let ast_const_name = format!("_BURN_FUNCTION_AST_{}", fn_name.to_uppercase());

    // Search for the constant declaration
    let const_pattern = format!("const {}: &[u8]", ast_const_name);
    let const_pos = expanded.find(&const_pattern)?;

    // Find the `b"` after the constant declaration (allowing for whitespace/newlines between = and b")
    let search_start = const_pos + const_pattern.len();
    let b_quote_pattern = "b\"";
    let b_quote_pos = expanded[search_start..].find(b_quote_pattern)?;
    let content_start = search_start + b_quote_pos + b_quote_pattern.len();

    // Find the closing `";`
    let chars: Vec<char> = expanded[content_start..].chars().collect();
    let mut pos = 0;

    while pos < chars.len() {
        if chars[pos] == '\\' && pos + 1 < chars.len() {
            // Skip the escaped character
            pos += 2;
        } else if chars[pos] == '"' {
            // Found the closing quote
            // Check if it's followed by `;`
            if pos + 1 < chars.len() && chars[pos + 1] == ';' {
                let escaped_content: String = chars[..pos].iter().collect();
                return Some(unescape_byte_string(&escaped_content));
            } else {
                pos += 1;
            }
        } else {
            pos += 1;
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_markers() {
        let expanded = r#"
            /* noise */ const X:&str="hello";
            const BURN_CENTRAL_FUNCTION_TRAIN:&str="BCFN1|my::module|train_fn|__train_fn_builder|train|training|END";
            const BURN_CENTRAL_FUNCTION_EVAL:&str=
                "BCFN1|my::module|eval_fn|__eval_fn_builder|evaluate|training|END";
        "#;

        let v = parse_expanded_output(expanded);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].mod_path, "my::module");
        assert_eq!(v[0].fn_name, "train_fn");
        assert_eq!(v[1].fn_name, "eval_fn");
        assert_eq!(v[1].routine_name, "evaluate");
    }

    #[test]
    fn rejects_bad_marker() {
        let bad = "BCFN1|a|b|c|d|END";
        assert!(parse_bcfn_marker(bad).is_none());
    }

    #[test]
    fn accepts_complex_mod_path() {
        let ok = "BCFN1|a::b::c::d|f|__builder|r|training|END";
        let m = parse_bcfn_marker(ok).unwrap();
        assert_eq!(m.mod_path, "a::b::c::d");
    }

    #[test]
    fn unescapes_byte_string() {
        let escaped = r#"hello \"world\" with \\backslash\\ and \n newline"#;
        let result = unescape_byte_string(escaped);
        let expected = b"hello \"world\" with \\backslash\\ and \n newline";
        assert_eq!(result, expected);
    }

    #[test]
    fn extracts_ast_token_stream() {
        let expanded = r#"
            const _: () = {
                const BURN_CENTRAL_FUNCTION_TEST: &str = "BCFN1|my::module|test|__test_builder|test|training|END";
                const _BURN_FUNCTION_AST_TEST: &[u8] = b"{\"vis\":\"pub\",\"ident\":\"test\"}";
            };
        "#;

        let token_stream = extract_ast_token_stream(expanded, "test").unwrap();
        let expected = b"{\"vis\":\"pub\",\"ident\":\"test\"}";
        assert_eq!(token_stream, expected);
    }

    #[test]
    fn parses_markers_with_ast() {
        let expanded = r#"
            const _: () = {
                const BURN_CENTRAL_FUNCTION_TRAIN:&str="BCFN1|my::module|train_fn|__train_fn_builder|train|training|END";
                const _BURN_FUNCTION_AST_TRAIN_FN: &[u8] = b"{\"vis\":\"pub\",\"ident\":\"train_fn\"}";
            };
            const _: () = {
                const BURN_CENTRAL_FUNCTION_EVAL:&str="BCFN1|my::module|eval_fn|__eval_fn_builder|evaluate|training|END";
                const _BURN_FUNCTION_AST_EVAL_FN: &[u8] = b"{\"vis\":\"pub\",\"ident\":\"eval_fn\"}";
            };
        "#;

        let v = parse_expanded_output(expanded);
        assert_eq!(v.len(), 2);

        // Verify metadata
        assert_eq!(v[0].mod_path, "my::module");
        assert_eq!(v[0].fn_name, "train_fn");

        // Verify token streams are populated
        assert!(!v[0].token_stream.is_empty());
        assert!(!v[1].token_stream.is_empty());

        // Verify token stream content
        let expected_train = b"{\"vis\":\"pub\",\"ident\":\"train_fn\"}";
        let expected_eval = b"{\"vis\":\"pub\",\"ident\":\"eval_fn\"}";
        assert_eq!(v[0].token_stream, expected_train);
        assert_eq!(v[1].token_stream, expected_eval);
    }

    #[test]
    fn handles_missing_ast_gracefully() {
        let expanded = r#"
            const BURN_CENTRAL_FUNCTION_TRAIN:&str="BCFN1|my::module|train_fn|__train_fn_builder|train|training|END";
        "#;

        let v = parse_expanded_output(expanded);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].fn_name, "train_fn");
        // Token stream should be empty when AST constant is missing
        assert!(v[0].token_stream.is_empty());
    }

    #[test]
    fn extracts_ast_with_newlines() {
        // This is the actual format from the macro expansion
        let expanded = r#"
            #[allow(dead_code)]
            const BURN_CENTRAL_FUNCTION_TRAINING: &str =
                "BCFN1|mnist_heat::training|training|__training_builder|mnist|training|END";
            #[allow(dead_code)]
            const _BURN_FUNCTION_AST_TRAINING: &[u8] =
                b"{\"vis\":\"pub\",\"ident\":\"training\"}";
        "#;

        let token_stream = extract_ast_token_stream(expanded, "training").unwrap();
        let expected = b"{\"vis\":\"pub\",\"ident\":\"training\"}";
        assert_eq!(token_stream, expected);
    }

    #[test]
    fn extracts_real_world_ast() {
        // This is the actual full format from the mnist project
        let expanded = r#"
            #[allow(dead_code)]
            const BURN_CENTRAL_FUNCTION_TRAINING: &str =
                "BCFN1|mnist_heat::training|training|__training_builder|mnist|training|END";
            #[allow(dead_code)]
            const _BURN_FUNCTION_AST_TRAINING: &[u8] =
                b"{\"vis\":\"pub\",\"ident\":\"training\",\"generics\":{\"params\":[{\"type\":{\"ident\":\"B\",\"colon_token\":true,\"bounds\":[{\"trait\":{\"path\":{\"segments\":[{\"ident\":\"AutodiffBackend\"}]}}}]}}]},\"inputs\":[{\"typed\":{\"pat\":{\"ident\":{\"ident\":\"client\"}},\"ty\":{\"reference\":{\"elem\":{\"path\":{\"segments\":[{\"ident\":\"ExperimentRun\"}]}}}}}},{\"typed\":{\"pat\":{\"ident\":{\"ident\":\"config\"}},\"ty\":{\"path\":{\"segments\":[{\"ident\":\"Args\",\"arguments\":{\"angle_bracketed\":{\"args\":[{\"type\":{\"path\":{\"segments\":[{\"ident\":\"ExperimentConfig\"}]}}}]}}}]}}}}],\"output\":{\"path\":{\"segments\":[{\"ident\":\"Result\"}]}}}";
        "#;

        let token_stream = extract_ast_token_stream(expanded, "training").unwrap();

        // Verify it starts with the expected JSON structure
        let json_str = std::str::from_utf8(&token_stream).unwrap();
        assert!(json_str.starts_with("{\"vis\":\"pub\",\"ident\":\"training\""));
        assert!(json_str.contains("\"ident\":\"AutodiffBackend\""));
        assert!(json_str.contains("\"ident\":\"client\""));
        assert!(json_str.contains("\"ident\":\"config\""));

        // Verify it's valid JSON by attempting to parse it
        let _: serde_json::Value =
            serde_json::from_slice(&token_stream).expect("Token stream should be valid JSON");
    }

    #[test]
    fn get_function_code_returns_source_with_comments() {
        let meta = FunctionMetadata {
            mod_path: "my::module".to_string(),
            fn_name: "test".to_string(),
            builder_fn_name: "__test_builder".to_string(),
            routine_name: "test".to_string(),
            proc_type: "training".to_string(),
            token_stream: "pub fn test() {\n    // Important comment\n    let value = 42;\n}"
                .as_bytes()
                .to_vec(),
        };

        let code = meta.get_function_code();
        assert!(code.contains("// Important comment"));
        assert!(code.contains("let value = 42;"));
    }
}
