use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, RwLock};

use cterm_plugin_api::{
    decode_request_frame, decode_response_frame, proto, ActionScope, BundleDigest, CommandId,
    PluginBundle, PluginPackageError, WireError, MAX_FRAME_BYTES,
};
use thiserror::Error;
use wasi_common::pipe::{ReadPipe, WritePipe};
use wasmi::{
    Config, EnforcedLimits, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder,
};
use wasmi_wasi::{WasiCtx, WasiCtxBuilder};

use crate::framing::BoundedOutput;

pub const DEFAULT_FUEL: u64 = 10_000_000;
pub const DEFAULT_MEMORY_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_STDERR_BYTES: usize = 64 * 1024;
pub const DEFAULT_STACK_BYTES: usize = 512 * 1024;
pub const DEFAULT_TABLE_ELEMENTS: usize = 4096;

/// Resource policy applied independently to every fresh invocation store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvocationLimits {
    pub fuel: u64,
    pub memory_bytes: usize,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub stack_bytes: usize,
    pub table_elements: usize,
}

impl Default for InvocationLimits {
    fn default() -> Self {
        Self {
            fuel: DEFAULT_FUEL,
            memory_bytes: DEFAULT_MEMORY_BYTES,
            stdout_bytes: MAX_FRAME_BYTES,
            stderr_bytes: DEFAULT_STDERR_BYTES,
            stack_bytes: DEFAULT_STACK_BYTES,
            table_elements: DEFAULT_TABLE_ELEMENTS,
        }
    }
}

/// Validated guest output. The original frame is retained so the runner does
/// not silently normalize or expand untrusted output before returning it.
#[derive(Debug)]
pub struct InvocationOutput {
    response: proto::PluginResponse,
    response_frame: Vec<u8>,
    stderr: Vec<u8>,
}

impl InvocationOutput {
    pub fn response(&self) -> &proto::PluginResponse {
        &self.response
    }

    pub fn response_frame(&self) -> &[u8] {
        &self.response_frame
    }

    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }
}

/// Reload, re-hash, and execute one command from one package.
///
/// The package must be absolute so runner behavior never depends on its current
/// working directory. `expected_digest` is the digest approved by the caller;
/// the independently loaded bytes must match it before compilation starts.
pub fn invoke(
    package_root: &Path,
    expected_digest: BundleDigest,
    request_frame: &[u8],
    limits: InvocationLimits,
) -> Result<InvocationOutput, RunnerError> {
    validate_limits(limits)?;
    if !package_root.is_absolute() {
        return Err(RunnerError::PackagePathNotAbsolute);
    }

    let bundle = PluginBundle::load(package_root)?;
    if bundle.digest() != expected_digest {
        return Err(RunnerError::DigestMismatch {
            expected: expected_digest,
            actual: bundle.digest(),
        });
    }

    let request = decode_request_frame(request_frame)?;
    let command = CommandId::parse(&request.command_id)?;
    if bundle.manifest().command(&command).is_none() {
        return Err(RunnerError::CommandNotDeclared(request.command_id));
    }

    let output = execute(&bundle, request_frame, limits)?;
    validate_declared_actions(&bundle, output)
}

fn execute(
    bundle: &PluginBundle,
    request_frame: &[u8],
    limits: InvocationLimits,
) -> Result<InvocationOutput, RunnerError> {
    let mut config = Config::default();
    config
        .consume_fuel(true)
        .enforced_limits(EnforcedLimits::strict())
        .ignore_custom_sections(true)
        .wasm_memory64(false)
        .wasm_multi_memory(false)
        .wasm_tail_call(false)
        .set_max_recursion_depth(256)
        .set_max_stack_height(limits.stack_bytes)
        .set_max_cached_stacks(0);
    let engine = Engine::new(&config);
    let module = Module::new(&engine, bundle.module()).map_err(RunnerError::Compile)?;

    let stdout = Arc::new(RwLock::new(BoundedOutput::new(limits.stdout_bytes)));
    let stderr = Arc::new(RwLock::new(BoundedOutput::new(limits.stderr_bytes)));
    let mut wasi = WasiCtxBuilder::new();
    wasi.arg("plugin.wasm")
        .map_err(|error| RunnerError::WasiContext(error.to_string()))?
        .stdin(Box::new(ReadPipe::from(request_frame)))
        .stdout(Box::new(WritePipe::from_shared(stdout.clone())))
        .stderr(Box::new(WritePipe::from_shared(stderr.clone())));

    let store_limits = StoreLimitsBuilder::new()
        .instances(1)
        .memories(1)
        .memory_size(limits.memory_bytes)
        .tables(1)
        .table_elements(limits.table_elements)
        .trap_on_grow_failure(true)
        .build();
    let state = HostState {
        wasi: wasi.build(),
        limits: store_limits,
    };
    let mut store = Store::new(&engine, state);
    store.limiter(|state| &mut state.limits);
    store.set_fuel(limits.fuel).map_err(RunnerError::Fuel)?;

    let mut linker = Linker::new(&engine);
    wasmi_wasi::add_to_linker(&mut linker, |state: &mut HostState| &mut state.wasi)
        .map_err(|error| RunnerError::WasiLinker(error.to_string()))?;

    let execution = (|| {
        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(RunnerError::Instantiate)?;
        let start = instance
            .get_typed_func::<(), ()>(&store, "_start")
            .map_err(RunnerError::MissingStart)?;
        match start.call(&mut store, ()) {
            Ok(()) => Ok(()),
            Err(error) if error.i32_exit_status() == Some(0) => Ok(()),
            Err(error) => Err(RunnerError::GuestExecution(error)),
        }
    })();

    let stdout = snapshot_output(&stdout);
    let stderr = snapshot_output(&stderr);
    if stdout.overflowed {
        return Err(RunnerError::StdoutLimitExceeded {
            limit: limits.stdout_bytes,
        });
    }
    if stderr.overflowed {
        return Err(RunnerError::StderrLimitExceeded {
            limit: limits.stderr_bytes,
        });
    }
    execution?;

    let response = decode_response_frame(&stdout.bytes)?;
    Ok(InvocationOutput {
        response,
        response_frame: stdout.bytes,
        stderr: stderr.bytes,
    })
}

fn validate_declared_actions(
    bundle: &PluginBundle,
    output: InvocationOutput,
) -> Result<InvocationOutput, RunnerError> {
    let declared = bundle.manifest().invoke_actions();
    let returned = output
        .response()
        .actions
        .iter()
        .map(|action| ActionScope::parse(&action.id))
        .collect::<Result<BTreeSet<_>, _>>()?;
    if let Some(scope) = returned.difference(declared).next() {
        return Err(RunnerError::ActionNotDeclared(scope.clone()));
    }
    Ok(output)
}

fn validate_limits(limits: InvocationLimits) -> Result<(), RunnerError> {
    if !(1..=DEFAULT_FUEL).contains(&limits.fuel) {
        return Err(RunnerError::InvalidLimit(
            "fuel must be non-zero and no larger than the runner ceiling",
        ));
    }
    if !(64 * 1024..=DEFAULT_MEMORY_BYTES).contains(&limits.memory_bytes) {
        return Err(RunnerError::InvalidLimit(
            "memory must be between one WebAssembly page and 16 MiB",
        ));
    }
    if limits.stdout_bytes == 0 || limits.stdout_bytes > MAX_FRAME_BYTES {
        return Err(RunnerError::InvalidLimit(
            "stdout must be non-zero and no larger than the ABI frame limit",
        ));
    }
    if !(1..=DEFAULT_STDERR_BYTES).contains(&limits.stderr_bytes) {
        return Err(RunnerError::InvalidLimit(
            "stderr must be non-zero and no larger than the runner ceiling",
        ));
    }
    if !(1024..=DEFAULT_STACK_BYTES).contains(&limits.stack_bytes) {
        return Err(RunnerError::InvalidLimit(
            "stack must be between 1 KiB and the runner ceiling",
        ));
    }
    if !(1..=DEFAULT_TABLE_ELEMENTS).contains(&limits.table_elements) {
        return Err(RunnerError::InvalidLimit(
            "table elements must be non-zero and no larger than the runner ceiling",
        ));
    }
    Ok(())
}

struct HostState {
    wasi: WasiCtx,
    limits: StoreLimits,
}

struct OutputSnapshot {
    bytes: Vec<u8>,
    overflowed: bool,
}

fn snapshot_output(output: &Arc<RwLock<BoundedOutput>>) -> OutputSnapshot {
    let output = output
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    OutputSnapshot {
        bytes: output.bytes().to_vec(),
        overflowed: output.overflowed(),
    }
}

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("plugin package path must be absolute")]
    PackagePathNotAbsolute,
    #[error("invalid runner resource policy: {0}")]
    InvalidLimit(&'static str),
    #[error(transparent)]
    Package(#[from] PluginPackageError),
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error("plugin package digest mismatch: expected {expected}, loaded {actual}")]
    DigestMismatch {
        expected: BundleDigest,
        actual: BundleDigest,
    },
    #[error("plugin command `{0}` is not declared by the verified manifest")]
    CommandNotDeclared(String),
    #[error("plugin action `{0}` is not declared by the verified manifest")]
    ActionNotDeclared(ActionScope),
    #[error("failed to compile plugin module: {0}")]
    Compile(#[source] wasmi::Error),
    #[error("failed to construct the empty WASI context: {0}")]
    WasiContext(String),
    #[error("failed to register WASIp1 imports: {0}")]
    WasiLinker(String),
    #[error("failed to configure plugin fuel: {0}")]
    Fuel(#[source] wasmi::Error),
    #[error("failed to instantiate plugin module: {0}")]
    Instantiate(#[source] wasmi::Error),
    #[error("plugin module does not export a typed `_start` function: {0}")]
    MissingStart(#[source] wasmi::Error),
    #[error("plugin execution failed: {0}")]
    GuestExecution(#[source] wasmi::Error),
    #[error("plugin stdout exceeded its {limit}-byte limit")]
    StdoutLimitExceeded { limit: usize },
    #[error("plugin stderr exceeded its {limit}-byte limit")]
    StderrLimitExceeded { limit: usize },
}

#[cfg(test)]
mod tests {
    use std::fs;

    use base64::Engine as _;
    use cterm_plugin_api::{encode_request_frame, PluginBundle, ABI_MAJOR, ABI_MINOR};
    use wasmi::TrapCode;

    use super::*;

    const LOOP: &str = include_str!("../tests/fixtures/loop.wat");
    const MEMORY_GROWTH: &str = include_str!("../tests/fixtures/memory_growth.wat");
    const STDOUT_FLOOD: &str = include_str!("../tests/fixtures/stdout_flood.wat");
    const STDERR_FLOOD: &str = include_str!("../tests/fixtures/stderr_flood.wat");
    const MALFORMED: &str = include_str!("../tests/fixtures/malformed_response.wat");
    const TRAP: &str = include_str!("../tests/fixtures/trap.wat");
    const UNSUPPORTED_IMPORT: &str = include_str!("../tests/fixtures/unsupported_import.wat");
    const EMPTY_AMBIENT_STATE: &str = include_str!("../tests/fixtures/empty_ambient_state.wat");
    const UI_NEW_TAB_WAT: &str = include_str!("../tests/fixtures/ui_new_tab.wat");
    const UI_NEW_TAB_BASE64: &str = include_str!("../tests/fixtures/ui_new_tab.wasm.base64");

    struct TestBundle {
        _directory: tempfile::TempDir,
        root: std::path::PathBuf,
        digest: BundleDigest,
    }

    fn bundle(wat_source: &str, declared_actions: &[&str]) -> TestBundle {
        let directory = tempfile::tempdir().unwrap();
        let allow = declared_actions
            .iter()
            .map(|action| format!("\"{action}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let manifest = format!(
            r#"manifest_version = 1
id = "org.example.runner"
name = "Runner fixture"
version = "1.0.0"
abi = "1.0"

[[commands]]
id = "run"
title = "Run"

[capabilities.invoke-actions]
allow = [{allow}]
"#
        );
        fs::write(directory.path().join("cterm-plugin.toml"), manifest).unwrap();
        fs::write(
            directory.path().join("plugin.wasm"),
            wat::parse_str(wat_source).unwrap(),
        )
        .unwrap();
        let verified = PluginBundle::load(directory.path()).unwrap();
        TestBundle {
            root: directory.path().to_path_buf(),
            digest: verified.digest(),
            _directory: directory,
        }
    }

    fn request() -> Vec<u8> {
        encode_request_frame(&proto::PluginRequest {
            abi_major: ABI_MAJOR,
            abi_minor: ABI_MINOR,
            command_id: "run".to_string(),
        })
        .unwrap()
    }

    fn response_writer(action: &str) -> String {
        let response = proto::PluginResponse {
            abi_major: ABI_MAJOR,
            abi_minor: ABI_MINOR,
            actions: vec![proto::ActionInvocation {
                id: action.to_string(),
                parameter: None,
            }],
            diagnostics: Vec::new(),
        };
        let frame = cterm_plugin_api::encode_response_frame(&response).unwrap();
        let escaped = frame
            .iter()
            .map(|byte| format!("\\{byte:02x}"))
            .collect::<String>();
        format!(
            r#"(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 32) "{escaped}")
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 32))
    (i32.store (i32.const 4) (i32.const {length}))
    (drop (call $fd_write
      (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))))"#,
            length = frame.len()
        )
    }

    fn run(wat_source: &str, declared_actions: &[&str]) -> Result<InvocationOutput, RunnerError> {
        let bundle = bundle(wat_source, declared_actions);
        invoke(
            &bundle.root,
            bundle.digest,
            &request(),
            InvocationLimits::default(),
        )
    }

    #[test]
    fn valid_command_runs_in_fresh_stores() {
        let module = response_writer("cterm:new-tab");
        let bundle = bundle(&module, &["cterm:new-tab"]);
        for _ in 0..2 {
            let output = invoke(
                &bundle.root,
                bundle.digest,
                &request(),
                InvocationLimits::default(),
            )
            .unwrap();
            assert_eq!(output.response().actions[0].id, "cterm:new-tab");
            assert!(output.stderr().is_empty());
        }
    }

    #[test]
    fn ui_fixture_binary_matches_its_reviewable_wat_source() {
        let committed = base64::engine::general_purpose::STANDARD
            .decode(UI_NEW_TAB_BASE64.trim())
            .unwrap();
        assert_eq!(committed, wat::parse_str(UI_NEW_TAB_WAT).unwrap());

        let output = run(UI_NEW_TAB_WAT, &["cterm:new-tab"]).unwrap();
        assert_eq!(output.response().actions[0].id, "cterm:new-tab");
    }

    #[test]
    fn resource_policy_cannot_raise_runner_ceilings() {
        let defaults = InvocationLimits::default();
        let raised = [
            InvocationLimits {
                fuel: defaults.fuel + 1,
                ..defaults
            },
            InvocationLimits {
                memory_bytes: defaults.memory_bytes + 1,
                ..defaults
            },
            InvocationLimits {
                stdout_bytes: defaults.stdout_bytes + 1,
                ..defaults
            },
            InvocationLimits {
                stderr_bytes: defaults.stderr_bytes + 1,
                ..defaults
            },
            InvocationLimits {
                stack_bytes: defaults.stack_bytes + 1,
                ..defaults
            },
            InvocationLimits {
                table_elements: defaults.table_elements + 1,
                ..defaults
            },
        ];
        for limits in raised {
            assert!(matches!(
                validate_limits(limits),
                Err(RunnerError::InvalidLimit(_))
            ));
        }
    }

    #[test]
    fn guest_has_no_environment_or_preopened_directories() {
        let output = run(EMPTY_AMBIENT_STATE, &["cterm:new-tab"]).unwrap();
        assert_eq!(output.response().actions[0].id, "cterm:new-tab");
    }

    #[test]
    fn infinite_loop_exhausts_fuel() {
        let error = run(LOOP, &["cterm:new-tab"]).unwrap_err();
        assert!(matches!(
            error,
            RunnerError::GuestExecution(ref source)
                if source.as_trap_code() == Some(TrapCode::OutOfFuel)
        ));
    }

    #[test]
    fn memory_growth_traps_at_sixteen_mibibytes() {
        let error = run(MEMORY_GROWTH, &["cterm:new-tab"]).unwrap_err();
        assert!(matches!(error, RunnerError::GuestExecution(_)));
    }

    #[test]
    fn stdout_and_stderr_floods_are_bounded() {
        assert!(matches!(
            run(STDOUT_FLOOD, &["cterm:new-tab"]),
            Err(RunnerError::StdoutLimitExceeded { .. })
        ));
        assert!(matches!(
            run(STDERR_FLOOD, &["cterm:new-tab"]),
            Err(RunnerError::StderrLimitExceeded { .. })
        ));
    }

    #[test]
    fn malformed_response_never_reaches_the_broker() {
        assert!(matches!(
            run(MALFORMED, &["cterm:new-tab"]),
            Err(RunnerError::Wire(WireError::Decode(_)))
        ));
    }

    #[test]
    fn explicit_guest_trap_is_contained() {
        let error = run(TRAP, &["cterm:new-tab"]).unwrap_err();
        assert!(matches!(
            error,
            RunnerError::GuestExecution(ref source)
                if source.as_trap_code() == Some(TrapCode::UnreachableCodeReached)
        ));
    }

    #[test]
    fn undeclared_action_is_rejected_after_wire_validation() {
        let module = response_writer("cterm:new-window");
        assert!(matches!(
            run(&module, &["cterm:new-tab"]),
            Err(RunnerError::ActionNotDeclared(scope))
                if scope.as_str() == "cterm:new-window"
        ));
    }

    #[test]
    fn digest_mismatch_prevents_module_compilation() {
        let module = response_writer("cterm:new-tab");
        let bundle = bundle(&module, &["cterm:new-tab"]);
        let wrong = "sha256:0000000000000000000000000000000000000000000000000000000000000000"
            .parse()
            .unwrap();
        assert!(matches!(
            invoke(&bundle.root, wrong, &request(), InvocationLimits::default()),
            Err(RunnerError::DigestMismatch { .. })
        ));
    }

    #[test]
    fn unsupported_import_is_never_linked() {
        assert!(matches!(
            run(UNSUPPORTED_IMPORT, &["cterm:new-tab"]),
            Err(RunnerError::Instantiate(_))
        ));
    }
}
