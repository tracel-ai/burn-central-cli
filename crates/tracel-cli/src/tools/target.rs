//! Target (OS + architecture) helpers for binary upload.

use std::collections::HashSet;
use std::process::{Command, Stdio};

use anyhow::Context;
use colored::Colorize;
use tracel_client::request::{Arch, Os};

/// Every (os, arch) target we offer to build for, in canonical display order.
/// The host is surfaced separately and pulled to the front by `prompt_targets`.
/// macOS x86_64 (Intel) is intentionally omitted — we don't support it.
pub const ALL_TARGETS: [(Os, Arch); 5] = [
    (Os::Linux, Arch::X86_64),
    (Os::Linux, Arch::Arm64),
    (Os::Macos, Arch::Arm64),
    (Os::Windows, Arch::X86_64),
    (Os::Windows, Arch::Arm64),
];

/// Canonical Rust target triple for an (os, arch) pair. Must match the server's
/// `TargetTriplet::Display`, because the upload-URL map is keyed by this string.
pub fn target_triple(os: Os, arch: Arch) -> &'static str {
    match (os, arch) {
        (Os::Windows, Arch::X86_64) => "x86_64-pc-windows-msvc",
        (Os::Windows, Arch::Arm64) => "aarch64-pc-windows-msvc",
        (Os::Linux, Arch::X86_64) => "x86_64-unknown-linux-gnu",
        (Os::Linux, Arch::Arm64) => "aarch64-unknown-linux-gnu",
        (Os::Macos, Arch::X86_64) => "x86_64-apple-darwin",
        (Os::Macos, Arch::Arm64) => "aarch64-apple-darwin",
    }
}

/// Reverse of [`target_triple`]: map a canonical base triple back to its `(os, arch)`,
/// or `None` if it isn't one we support. Iterates `ALL_TARGETS` so the mapping stays
/// a single source of truth.
pub fn os_arch_from_triple(base: &str) -> Option<(Os, Arch)> {
    ALL_TARGETS
        .iter()
        .copied()
        .find(|&(os, arch)| target_triple(os, arch) == base)
}

/// Comma-separated list of every supported base triple, for error messages.
pub fn supported_targets_help() -> String {
    ALL_TARGETS
        .iter()
        .map(|&(os, arch)| target_triple(os, arch))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A parsed `--target` spec: a supported `(os, arch)` plus optional per-target build
/// knobs — a pinned glibc version and raw args appended to that target's cargo command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetSpec {
    pub os: Os,
    pub arch: Arch,
    /// Only ever `Some` for `*-linux-gnu` targets. Forces the build through
    /// `cargo zigbuild`, which pins glibc via a `.<version>` suffix on the triple.
    pub glibc: Option<String>,
    /// Extra arguments appended verbatim to this target's cargo invocation.
    pub raw_args: Vec<String>,
}

/// Validate a glibc version string: non-empty, digits and dots only, no leading,
/// trailing, or doubled dots (so it forms a valid `<triple>.<version>` suffix).
fn validate_glibc(v: &str) -> anyhow::Result<()> {
    let ok = !v.is_empty()
        && !v.starts_with('.')
        && !v.ends_with('.')
        && !v.contains("..")
        && v.chars().all(|c| c.is_ascii_digit() || c == '.');
    if !ok {
        anyhow::bail!("Invalid glibc version `{v}` (expected something like `2.28`).");
    }
    Ok(())
}

/// Parse a single `--target` value of the form `<triple>[.<glibc>] [-- <raw args>]`.
///
/// Everything after the first ` -- ` becomes raw args (whitespace-split); the rest is a
/// canonical base triple with an optional `.<glibc>` suffix (valid only for `*-linux-gnu`).
pub fn parse_target_spec(input: &str) -> anyhow::Result<TargetSpec> {
    let (triple_part, raw_args): (&str, Vec<String>) = match input.split_once(" -- ") {
        Some((left, right)) => (
            left.trim(),
            right.split_whitespace().map(str::to_string).collect(),
        ),
        None => (input.trim(), Vec::new()),
    };

    // Exact canonical triple, no glibc suffix.
    if let Some((os, arch)) = os_arch_from_triple(triple_part) {
        return Ok(TargetSpec {
            os,
            arch,
            glibc: None,
            raw_args,
        });
    }

    // `<...>-linux-gnu.<glibc>` — the only triple that carries a version suffix.
    if let Some((base, suffix)) = triple_part.split_once("-linux-gnu.") {
        let base_triple = format!("{base}-linux-gnu");
        if let Some((os, arch)) = os_arch_from_triple(&base_triple) {
            validate_glibc(suffix)?;
            return Ok(TargetSpec {
                os,
                arch,
                glibc: Some(suffix.to_string()),
                raw_args,
            });
        }
    }

    anyhow::bail!(
        "Unsupported target `{triple_part}`. Supported: {} (append `.<glibc>` to a *-linux-gnu triple to pin glibc).",
        supported_targets_help()
    )
}

/// Apply a default glibc version to every `*-linux-gnu` spec lacking an inline suffix.
/// Errors if a default is given but no selected target is a Linux target.
pub fn apply_glibc_default(
    specs: &mut [TargetSpec],
    glibc_default: Option<&str>,
) -> anyhow::Result<()> {
    let Some(g) = glibc_default else {
        return Ok(());
    };
    validate_glibc(g)?;
    let mut has_linux = false;
    for spec in specs.iter_mut() {
        if spec.os == Os::Linux {
            has_linux = true;
            // Inline `.<glibc>` suffixes take precedence over the broadcast default.
            if spec.glibc.is_none() {
                spec.glibc = Some(g.to_string());
            }
        }
    }
    if !has_linux {
        anyhow::bail!("`--glibc` was given but no selected target is a *-linux-gnu target.");
    }
    Ok(())
}

/// Parse and finalize the `--target` specs: parse each value, prepend the global raw args
/// to every target, apply the `--glibc` default, and reject two specs that collapse to the
/// same `(os, arch)` (their canonical upload key would collide).
pub fn resolve_target_specs(
    inputs: &[String],
    glibc_default: Option<&str>,
    global_args: &[String],
) -> anyhow::Result<Vec<TargetSpec>> {
    let mut specs = Vec::with_capacity(inputs.len());
    for raw in inputs {
        let mut spec = parse_target_spec(raw)?;
        // Global raw args apply to every target, before the per-target ones.
        if !global_args.is_empty() {
            let mut merged = global_args.to_vec();
            merged.append(&mut spec.raw_args);
            spec.raw_args = merged;
        }
        specs.push(spec);
    }

    apply_glibc_default(&mut specs, glibc_default)?;

    // Two builds for one (os, arch) would upload under the same canonical key.
    for i in 0..specs.len() {
        for j in (i + 1)..specs.len() {
            if (specs[i].os, specs[i].arch) == (specs[j].os, specs[j].arch) {
                anyhow::bail!(
                    "Duplicate target `{}` — each (os, arch) may only be built once per run.",
                    target_triple(specs[i].os, specs[i].arch)
                );
            }
        }
    }

    Ok(specs)
}

/// Human-friendly name for an (os, arch) pair, e.g. "Linux x86_64".
fn pretty_name(os: Os, arch: Arch) -> &'static str {
    match (os, arch) {
        (Os::Linux, Arch::X86_64) => "Linux x86_64",
        (Os::Linux, Arch::Arm64) => "Linux arm64",
        (Os::Macos, Arch::X86_64) => "macOS x86_64",
        (Os::Macos, Arch::Arm64) => "macOS arm64",
        (Os::Windows, Arch::X86_64) => "Windows x86_64",
        (Os::Windows, Arch::Arm64) => "Windows arm64",
    }
}

/// Detect the host OS/arch
pub fn host_target() -> anyhow::Result<(Os, Arch)> {
    let os = match std::env::consts::OS {
        "windows" => Os::Windows,
        "linux" => Os::Linux,
        "macos" => Os::Macos,
        other => anyhow::bail!("Unsupported host operating system for packaging: `{other}`"),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => Arch::X86_64,
        "aarch64" | "arm64" => Arch::Arm64,
        other => anyhow::bail!("Unsupported host architecture for packaging: `{other}`"),
    };
    if (os, arch) == (Os::Macos, Arch::X86_64) {
        anyhow::bail!("macOS on x86_64 (Intel) is not supported for packaging.");
    }
    Ok((os, arch))
}

/// Triples reported by `rustup target list --installed`.
///
/// Returns an empty set on any failure (rustup missing, non-zero exit, bad UTF-8),
/// which conservatively makes every *cross* target appear "not installed". The host
/// target never depends on this set — a host build needs no rustup target.
pub fn installed_targets() -> HashSet<String> {
    let output = Command::new("rustup")
        .arg("target")
        .arg("list")
        .arg("--installed")
        .stderr(Stdio::null())
        .output();

    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect(),
        _ => HashSet::new(),
    }
}

/// Install a target's prebuilt std with `rustup target add <triple>`, streaming
/// output to the user's terminal.
pub fn add_target(triple: &str) -> anyhow::Result<()> {
    let status = Command::new("rustup")
        .arg("target")
        .arg("add")
        .arg(triple)
        .status()
        .with_context(|| {
            format!("Failed to run `rustup target add {triple}` (is rustup installed?)")
        })?;
    if !status.success() {
        anyhow::bail!("`rustup target add {triple}` failed");
    }
    Ok(())
}

/// Prompt the user to select one or more targets to build for. The host is listed
/// first and labelled "(this machine)"; cross targets not installed via rustup are
/// dimmed and annotated with the `rustup target add` command to install them.
pub fn prompt_targets(
    host: (Os, Arch),
    installed: &HashSet<String>,
) -> anyhow::Result<Vec<(Os, Arch)>> {
    // Host first, then the remaining targets in canonical order.
    let mut ordered: Vec<(Os, Arch)> = vec![host];
    ordered.extend(ALL_TARGETS.iter().copied().filter(|t| *t != host));

    let items: Vec<((Os, Arch), String, String)> = ordered
        .iter()
        .map(|&(os, arch)| {
            let triple = target_triple(os, arch);
            let name = pretty_name(os, arch);

            if (os, arch) == host {
                (
                    (os, arch),
                    format!("{name}  ({triple})  (this machine)"),
                    "builds natively; no extra toolchain needed".to_string(),
                )
            } else if installed.contains(triple) {
                ((os, arch), format!("{name}  ({triple})"), String::new())
            } else {
                // Not installed: dim the whole label (stays greyed even under the
                // cursor) and keep a visible textual marker so the distinction
                // survives in terminals without color, plus an actionable hint.
                (
                    (os, arch),
                    format!("{name}  ({triple})  - not installed")
                        .dimmed()
                        .to_string(),
                    format!("run: rustup target add {triple}"),
                )
            }
        })
        .collect();

    cliclack::multiselect("Select the target(s) to build for (space to toggle, enter to confirm)")
        .items(&items)
        .initial_values(vec![host])
        .required(true)
        .interact()
        .map_err(anyhow::Error::from)
}

pub fn install_missing_target(missing: Vec<&str>, assume_yes: bool) -> anyhow::Result<()> {
    if !missing.is_empty() {
        let list = missing.join(", ");
        let install = assume_yes
            || cliclack::confirm(format!(
                "These targets are not installed: {list}. Run `rustup target add` for them now?"
            ))
            .initial_value(true)
            .interact()?;
        if install {
            for triple in &missing {
                add_target(triple)?;
            }
        } else {
            let cmds = missing
                .iter()
                .map(|triple| format!("rustup target add {triple}"))
                .collect::<Vec<_>>()
                .join("\n  ");
            anyhow::bail!(
                "Cannot build without the selected targets installed. Install them with:\n  {cmds}"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_args() -> Vec<String> {
        Vec::new()
    }

    #[test]
    fn parses_canonical_triple() {
        let spec = parse_target_spec("x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(spec.os, Os::Linux);
        assert_eq!(spec.arch, Arch::X86_64);
        assert_eq!(spec.glibc, None);
        assert!(spec.raw_args.is_empty());
    }

    #[test]
    fn parses_glibc_suffix() {
        let spec = parse_target_spec("aarch64-unknown-linux-gnu.2.17").unwrap();
        assert_eq!((spec.os, spec.arch), (Os::Linux, Arch::Arm64));
        assert_eq!(spec.glibc.as_deref(), Some("2.17"));
    }

    #[test]
    fn parses_raw_args_after_dashdash() {
        let spec = parse_target_spec("x86_64-unknown-linux-gnu.2.28 -- --config net.git=true -Z build-std")
            .unwrap();
        assert_eq!(spec.glibc.as_deref(), Some("2.28"));
        assert_eq!(spec.raw_args, vec!["--config", "net.git=true", "-Z", "build-std"]);
    }

    #[test]
    fn rejects_glibc_on_non_linux() {
        // Windows triples don't match the `-linux-gnu.` split, so the suffix is rejected.
        assert!(parse_target_spec("x86_64-pc-windows-msvc.2.28").is_err());
    }

    #[test]
    fn rejects_unknown_triple() {
        assert!(parse_target_spec("riscv64gc-unknown-linux-gnu").is_err());
        assert!(parse_target_spec("x86_64-apple-darwin").is_err()); // unsupported (Intel mac)
    }

    #[test]
    fn rejects_bad_glibc_version() {
        assert!(parse_target_spec("x86_64-unknown-linux-gnu.").is_err());
        assert!(parse_target_spec("x86_64-unknown-linux-gnu.2..1").is_err());
        assert!(parse_target_spec("x86_64-unknown-linux-gnu.abc").is_err());
    }

    #[test]
    fn glibc_default_applies_to_linux_only() {
        let mut specs = vec![
            TargetSpec { os: Os::Linux, arch: Arch::X86_64, glibc: None, raw_args: no_args() },
            TargetSpec { os: Os::Windows, arch: Arch::X86_64, glibc: None, raw_args: no_args() },
        ];
        apply_glibc_default(&mut specs, Some("2.28")).unwrap();
        assert_eq!(specs[0].glibc.as_deref(), Some("2.28"));
        assert_eq!(specs[1].glibc, None);
    }

    #[test]
    fn glibc_default_does_not_override_inline() {
        let mut specs = vec![TargetSpec {
            os: Os::Linux,
            arch: Arch::X86_64,
            glibc: Some("2.17".to_string()),
            raw_args: no_args(),
        }];
        apply_glibc_default(&mut specs, Some("2.28")).unwrap();
        assert_eq!(specs[0].glibc.as_deref(), Some("2.17"));
    }

    #[test]
    fn glibc_default_without_linux_target_errors() {
        let mut specs = vec![TargetSpec {
            os: Os::Windows,
            arch: Arch::X86_64,
            glibc: None,
            raw_args: no_args(),
        }];
        assert!(apply_glibc_default(&mut specs, Some("2.28")).is_err());
    }

    #[test]
    fn resolve_prepends_global_args() {
        let specs = resolve_target_specs(
            &["x86_64-unknown-linux-gnu -- --per-target".to_string()],
            None,
            &["--global".to_string()],
        )
        .unwrap();
        assert_eq!(specs[0].raw_args, vec!["--global", "--per-target"]);
    }

    #[test]
    fn resolve_rejects_duplicate_os_arch() {
        let err = resolve_target_specs(
            &[
                "x86_64-unknown-linux-gnu".to_string(),
                "x86_64-unknown-linux-gnu.2.28".to_string(),
            ],
            None,
            &no_args(),
        );
        assert!(err.is_err());
    }
}
