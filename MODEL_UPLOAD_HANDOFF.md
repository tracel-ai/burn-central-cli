# Handoff: `tracel model upload` — CLI side

_Written 2026-07-09 for whichever Claude picks this up next._

## Status

Nothing has been written to `crates/tracel-cli/src/` yet for this feature.
`commands/model.rs` does not exist. This file exists so you don't have to
re-derive the context below from scratch.

Branch: `feat/upload_model_command` (already checked out, off `main`).
Only existing change on the branch: `Cargo.toml`/`Cargo.lock` pin
`tracel-client = "0.9.0"` (published on crates.io, verified to build —
this is the correct pin, not a local path hack; leave it as-is).

## What this task is

Add `tracel model upload <model_name> <directory> [--namespace <ns>] [--project <proj>]`:
walk a local directory, checksum every file, ask the backend for presigned
multipart upload URLs, PUT the bytes, then finalize. Scope for this repo is
**CLI only** — backend (`backend/backend`) and client (`client/tracel-client`)
sides are already done and released (`tracel-client` 0.9.0 has the needed
`Client` methods, confirmed by reading the client source directly).

## Design docs (read these first, but see "drift" section below)

- `../docs/superpowers/specs/2026-07-08-model-upload-design.md`
- `../docs/superpowers/plans/2026-07-08-model-upload-plan.md`

These live outside all three repos (`Tracel/docs/` or (`../docs/`)), intentionally not
committed into any of them. The plan is written as a TDD task list across
Parts A (backend), B (client), C (CLI) — **only Part C, Task 5 onward is
relevant here**, since A and B are already merged/published.

There is also a stale reference branch `origin/feat/model-upload` in this
repo (Jonathan Richard's proof-of-concept, 39 commits behind `main`, predates
the `burn-central-cli` → `tracel-cli` rename, touches unrelated files). We
already decided **not** to rebase/merge it — treat it as read-only reference
material at most, not something to build on. The design doc already extracted
what was worth keeping from it (streamed SHA-256, real multipart handling,
confirm-before-create, deterministic `BTreeMap` ordering) and what to leave
behind (`--include` globs, `crossbeam-channel`).

## Drift between the plan doc and the actual client API

The plan doc was written before the client-side task was implemented, and the
real implementation used different names than the plan guessed. **Trust the
code below, not the plan's Rust snippets, for exact names.** Verified by
reading `client/tracel-client/src/model/{mod,request,response}.rs` directly:

```rust
// tracel_client::model::request
pub struct CreateModelRequest { pub name: String, pub description: Option<String> }
pub struct ModelFileSpecRequest { pub rel_path: String, pub size_bytes: u64, pub checksum: String }
pub struct RequestModelVersionUploadRequest { pub files: Vec<ModelFileSpecRequest> }
// NOT `CreateModelVersionUploadRequest` — the plan's guessed name is wrong.

// tracel_client::model::response
pub struct PresignedModelFileUploadUrlsResponse { pub rel_path: String, pub urls: tracel_client::artifact::response::MultipartUploadResponse }
pub struct RequestModelVersionUploadResponse { pub version: u32, pub files: Vec<PresignedModelFileUploadUrlsResponse> }
// NOT `CreateModelVersionUploadResponse`.

// tracel_client::artifact::response (reused as-is, per the design doc)
pub struct MultipartUploadResponse { pub id: String, pub parts: Vec<PresignedUploadUrlResponse> }
pub struct PresignedUploadUrlResponse { pub part: u32, pub url: String, pub size_bytes: u64 }

impl Client {
    pub fn create_model(&self, namespace: &str, project_name: &str, req: CreateModelRequest) -> Result<ModelResponse, ClientError>;
    pub fn get_model(&self, namespace: &str, project_name: &str, model_name: &str) -> Result<ModelResponse, ClientError>;
    pub fn request_model_version_upload(&self, namespace: &str, project_name: &str, model_name: &str, req: RequestModelVersionUploadRequest) -> Result<RequestModelVersionUploadResponse, ClientError>;
    // NOT `create_model_version_upload` — the plan's guessed name is wrong.
    pub fn complete_model_version_upload(&self, namespace: &str, project_name: &str, model_name: &str, version: u32) -> Result<(), ClientError>;
    pub fn upload_bytes_to_url(&self, url: &str, bytes: Vec<u8>) -> Result<(), ClientError>; // generic PUT, reused for each part
}

impl ClientError {
    pub fn is_not_found(&self) -> bool; // exists, use for the "model doesn't exist yet" branch
}
```

All of the above are also re-exported flat under `tracel_client::request::*` /
`tracel_client::response::*` (see `lib.rs`'s `pub mod tracel { pub mod
request {...} pub mod response {...} }`), so either import path works — the
plan doc's snippets use the flat path in some places, module path in others.

Everything else in the plan (worker count = 8, fail-fast on first failure,
`Arc<Mutex<Vec<Task>>>` queue instead of crossbeam, no `--include` flag, no
`--message` flag for the version, deterministic `BTreeMap<rel_path,
PathBuf>` ordering, confirm-before-create with optional description prompt)
still holds and matches how the client/backend were actually built.

## Repo structure primer (tracel-cli)

- `src/bin/tracel.rs` — `fn main()`, calls `cli::cli_main()`.
- `src/cli.rs` — clap `CliArgs`/`Commands` enum + `handle_command` dispatch.
  **Add a `Model(commands::model::ModelArgs)` variant here** (after `Me`),
  plus a match arm `Commands::Model(a) => commands::model::handle_command(a, context)`.
- `src/commands/mod.rs` — declares `pub mod <name>;` per command file
  (alphabetical). **Add `pub mod model;` after `pub mod me;`.**
- `src/commands/model.rs` — **new file, doesn't exist yet.** This is the
  actual work.
- `src/commands/package.rs` — closest existing precedent: local
  checksum → request presigned URL(s) → PUT bytes → complete. Single-file/
  single-PUT though, not multipart; model upload needs per-part multipart
  handling package.rs doesn't have.
- `src/commands/login.rs` — has `get_client_and_login_if_needed(&mut context)
  -> anyhow::Result<Client>`, the standard way every command gets an
  authenticated client.
- `src/helpers/project.rs` (re-exported via `helpers::mod.rs`) —
  `require_linked_project(&context) -> anyhow::Result<ProjectContext>`,
  gives you `.get_project()` → `{ owner, name }` for the linked
  namespace/project when `--namespace`/`--project` aren't both given.
- `src/context.rs` — `CliContext`: `.terminal()`, `.environment()`,
  `.create_client()`.
- `src/tools/terminal.rs` — `Terminal`: `command_title`, `print`,
  `print_success`, `print_err`, `print_warning`, `confirm(msg) -> bool`,
  `spinner() -> cliclack::ProgressBar` (has `.start(msg)`, `.set_message(msg)`,
  `.stop(msg)`, `.error(msg)`), `finalize(msg)`, `cancel_finalize(msg)`. All
  user-facing output goes through here, never raw `println!`.

Dependencies already in `crates/tracel-cli/Cargo.toml` (no new ones needed):
`sha2`, `walkdir`, `cliclack`, `anyhow`, `clap`.

## Next step

Write `src/commands/model.rs` following plan doc Part C (Tasks 5-11) with the
corrected API names above, then wire it into `commands/mod.rs` and `cli.rs`.
The user wants to understand the repo structure alongside the implementation
(not just get a finished file dropped in) — walk through it, don't just paste
code silently.
