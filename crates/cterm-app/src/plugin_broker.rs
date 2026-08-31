//! Application-side policy and process boundary for command plugins.
//!
//! The broker never searches `PATH`: it resolves the runner next to the
//! application executable and packages as direct children of one canonical
//! plugin root. Every invocation re-loads the package and checks its exact
//! digest grants before the runner is launched.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use cterm_plugin_api::{
    decode_response_frame, encode_request_frame, proto, ActionScope, CommandId, GrantDecision,
    GrantStore, PluginBundle, PluginId, PluginPackageError, WireError, ABI_MAJOR, ABI_MINOR,
    MAX_FRAME_BYTES, PLUGIN_HOST_EXECUTABLE_NAME,
};
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::task::{AbortHandle, JoinError, JoinHandle};
use tokio::time::{timeout, Instant};

const DEFAULT_INVOCATION_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_INVOCATION_TIMEOUT: Duration = Duration::from_secs(10);
const TERMINATION_TIMEOUT: Duration = Duration::from_secs(1);
const HOST_STDERR_LIMIT: usize = 64 * 1024;

/// Validated wall-clock limit for one runner process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginBrokerTimeout(Duration);

impl PluginBrokerTimeout {
    pub fn new(value: Duration) -> Result<Self, PluginBrokerError> {
        if value.is_zero() || value > MAX_INVOCATION_TIMEOUT {
            return Err(PluginBrokerError::InvalidTimeout {
                maximum: MAX_INVOCATION_TIMEOUT,
            });
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> Duration {
        self.0
    }
}

impl Default for PluginBrokerTimeout {
    fn default() -> Self {
        Self(DEFAULT_INVOCATION_TIMEOUT)
    }
}

/// Validated output from one isolated plugin invocation.
#[derive(Debug)]
pub struct PluginBrokerOutput {
    response: proto::PluginResponse,
    host_stderr: Vec<u8>,
}

impl PluginBrokerOutput {
    pub fn response(&self) -> &proto::PluginResponse {
        &self.response
    }

    pub fn host_stderr(&self) -> &[u8] {
        &self.host_stderr
    }
}

/// Resolves, authorizes, launches, and validates one-shot plugin runners.
#[derive(Debug, Clone)]
pub struct PluginBroker {
    host: HostCommand,
    plugins_root: PathBuf,
    invocation_timeout: PluginBrokerTimeout,
}

impl PluginBroker {
    /// Resolve the installed runner next to the current cterm executable.
    pub fn discover(plugins_root: impl AsRef<Path>) -> Result<Self, PluginBrokerError> {
        let executable = std::env::current_exe().map_err(PluginBrokerError::CurrentExecutable)?;
        Self::from_application_executable(executable, plugins_root)
    }

    /// Resolve the installed runner next to a known cterm executable.
    ///
    /// This constructor is useful to callers which already resolved the
    /// package's application binary during relaunch or upgrade handling.
    pub fn from_application_executable(
        application_executable: impl AsRef<Path>,
        plugins_root: impl AsRef<Path>,
    ) -> Result<Self, PluginBrokerError> {
        let application_executable = canonical_file(
            application_executable.as_ref(),
            PluginBrokerError::ApplicationExecutable,
        )?;
        let application_directory = application_executable
            .parent()
            .ok_or(PluginBrokerError::ApplicationDirectory)?;
        let expected_host = application_directory.join(PLUGIN_HOST_EXECUTABLE_NAME);
        let host = canonical_file(&expected_host, PluginBrokerError::HostExecutable)?;
        if host.parent() != Some(application_directory)
            || host.file_name() != Some(PLUGIN_HOST_EXECUTABLE_NAME.as_ref())
        {
            return Err(PluginBrokerError::HostOutsideApplicationDirectory(host));
        }

        let plugins_root = canonical_directory(plugins_root.as_ref())?;
        Ok(Self {
            host: HostCommand::new(host),
            plugins_root,
            invocation_timeout: PluginBrokerTimeout::default(),
        })
    }

    /// Override the default two-second limit with another bounded timeout.
    pub fn with_timeout(mut self, invocation_timeout: PluginBrokerTimeout) -> Self {
        self.invocation_timeout = invocation_timeout;
        self
    }

    pub fn host_path(&self) -> &Path {
        &self.host.executable
    }

    pub fn plugins_root(&self) -> &Path {
        &self.plugins_root
    }

    /// Invoke one command after exact package and grant validation.
    pub async fn invoke(
        &self,
        grants: &GrantStore,
        plugin: &PluginId,
        command: &CommandId,
    ) -> Result<PluginBrokerOutput, PluginBrokerError> {
        let bundle = self.load_package(plugin)?;
        if bundle.manifest().command(command).is_none() {
            return Err(PluginBrokerError::CommandNotDeclared {
                plugin: plugin.clone(),
                command: command.clone(),
            });
        }
        ensure_granted(grants, &bundle)?;

        let request = encode_request_frame(&proto::PluginRequest {
            abi_major: ABI_MAJOR,
            abi_minor: ABI_MINOR,
            command_id: command.as_str().to_string(),
        })?;
        let process_output = self.run_host(&bundle, request).await?;
        let response = validate_response(grants, &bundle, &process_output.stdout)?;
        Ok(PluginBrokerOutput {
            response,
            host_stderr: process_output.stderr,
        })
    }

    fn load_package(&self, plugin: &PluginId) -> Result<PluginBundle, PluginBrokerError> {
        let requested = self.plugins_root.join(plugin.as_str());
        let bundle = PluginBundle::load(&requested)?;
        if bundle.root().parent() != Some(self.plugins_root.as_path())
            || bundle.root().file_name() != Some(plugin.as_str().as_ref())
        {
            return Err(PluginBrokerError::PackageOutsidePluginRoot {
                root: self.plugins_root.clone(),
                package: bundle.root().to_path_buf(),
            });
        }
        Ok(bundle)
    }

    async fn run_host(
        &self,
        bundle: &PluginBundle,
        request: Vec<u8>,
    ) -> Result<ProcessOutput, PluginBrokerError> {
        let started = Instant::now();
        let deadline = started + self.invocation_timeout.get();
        let mut command = CommandWrap::with_new(&self.host.executable, |command| {
            command
                .args(&self.host.prefix_arguments)
                .arg("--package")
                .arg(bundle.root())
                .arg("--expected-digest")
                .arg(bundle.digest().to_string())
                .current_dir(
                    self.host
                        .executable
                        .parent()
                        .expect("a canonical executable always has a parent"),
                )
                .env_clear()
                // Production HostCommand values keep this empty. Windows unit
                // tests add only SystemRoot because Windows PowerShell 5.1
                // cannot initialize its managed runtime without it.
                .envs(
                    self.host
                        .required_environment
                        .iter()
                        .map(|(key, value)| (key, value)),
                )
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        });
        command.wrap(KillOnDrop);
        #[cfg(unix)]
        command.wrap(ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(JobObject);

        let mut child = command.spawn().map_err(|source| PluginBrokerError::Spawn {
            path: self.host.executable.clone(),
            source,
        })?;
        let stdin = child
            .stdin()
            .take()
            .ok_or(PluginBrokerError::MissingPipe("stdin"))?;
        let stdout = child
            .stdout()
            .take()
            .ok_or(PluginBrokerError::MissingPipe("stdout"))?;
        let stderr = child
            .stderr()
            .take()
            .ok_or(PluginBrokerError::MissingPipe("stderr"))?;

        let input_task = tokio::spawn(write_request(stdin, request));
        let stdout_task = tokio::spawn(read_bounded(stdout, MAX_FRAME_BYTES, Stream::Stdout));
        let stderr_task = tokio::spawn(read_bounded(stderr, HOST_STDERR_LIMIT, Stream::Stderr));
        let abort_handles = [
            input_task.abort_handle(),
            stdout_task.abort_handle(),
            stderr_task.abort_handle(),
        ];

        let interaction = async {
            tokio::try_join!(
                async { child.wait().await.map_err(PluginBrokerError::Wait) },
                join_task(input_task),
                join_task(stdout_task),
                join_task(stderr_task),
            )
        };
        let outcome = timeout(
            deadline.saturating_duration_since(Instant::now()),
            interaction,
        )
        .await;

        let (status, (), stdout, stderr) = match outcome {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => {
                abort_all(&abort_handles);
                terminate_tree(&mut *child).await?;
                return Err(error);
            }
            Err(_) => {
                abort_all(&abort_handles);
                terminate_tree(&mut *child).await?;
                return Err(PluginBrokerError::TimedOut {
                    limit: self.invocation_timeout.get(),
                    elapsed: started.elapsed(),
                });
            }
        };

        if !status.success() {
            return Err(PluginBrokerError::HostFailed {
                status: status.to_string(),
                stderr: escape_bytes(&stderr),
            });
        }
        Ok(ProcessOutput { stdout, stderr })
    }

    #[cfg(test)]
    fn for_test(
        host: HostCommand,
        plugins_root: PathBuf,
        invocation_timeout: PluginBrokerTimeout,
    ) -> Self {
        Self {
            host,
            plugins_root,
            invocation_timeout,
        }
    }
}

#[derive(Debug, Clone)]
struct HostCommand {
    executable: PathBuf,
    prefix_arguments: Vec<OsString>,
    required_environment: Vec<(OsString, OsString)>,
}

impl HostCommand {
    fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            prefix_arguments: Vec::new(),
            required_environment: Vec::new(),
        }
    }
}

struct ProcessOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

async fn write_request(
    mut stdin: tokio::process::ChildStdin,
    request: Vec<u8>,
) -> Result<(), PluginBrokerError> {
    stdin
        .write_all(&request)
        .await
        .map_err(PluginBrokerError::WriteRequest)?;
    stdin
        .shutdown()
        .await
        .map_err(PluginBrokerError::WriteRequest)
}

async fn read_bounded<R: AsyncRead + Unpin>(
    reader: R,
    limit: usize,
    stream: Stream,
) -> Result<Vec<u8>, PluginBrokerError> {
    let take_limit = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    let mut reader = reader.take(take_limit);
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    reader
        .read_to_end(&mut bytes)
        .await
        .map_err(|source| match stream {
            Stream::Stdout => PluginBrokerError::ReadStdout(source),
            Stream::Stderr => PluginBrokerError::ReadStderr(source),
        })?;
    if bytes.len() > limit {
        return Err(match stream {
            Stream::Stdout => PluginBrokerError::StdoutLimitExceeded { limit },
            Stream::Stderr => PluginBrokerError::StderrLimitExceeded { limit },
        });
    }
    Ok(bytes)
}

async fn join_task<T>(
    task: JoinHandle<Result<T, PluginBrokerError>>,
) -> Result<T, PluginBrokerError> {
    task.await.map_err(PluginBrokerError::IoTask)?
}

fn abort_all(handles: &[AbortHandle]) {
    for handle in handles {
        handle.abort();
    }
}

async fn terminate_tree(child: &mut dyn ChildWrapper) -> Result<(), PluginBrokerError> {
    let kill_error = child.start_kill().err();
    match timeout(TERMINATION_TIMEOUT, child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(source)) => Err(PluginBrokerError::TerminateWait(source)),
        Err(_) => match kill_error {
            Some(source) => Err(PluginBrokerError::Terminate(source)),
            None => Err(PluginBrokerError::TerminationTimedOut {
                limit: TERMINATION_TIMEOUT,
            }),
        },
    }
}

fn ensure_granted(grants: &GrantStore, bundle: &PluginBundle) -> Result<(), PluginBrokerError> {
    match grants.decision(bundle) {
        GrantDecision::Granted => Ok(()),
        GrantDecision::ApprovalRequired {
            missing,
            content_changed,
        } => Err(PluginBrokerError::GrantRequired {
            missing,
            content_changed,
        }),
    }
}

fn validate_response(
    grants: &GrantStore,
    bundle: &PluginBundle,
    response_frame: &[u8],
) -> Result<proto::PluginResponse, PluginBrokerError> {
    let response = decode_response_frame(response_frame)?;
    ensure_granted(grants, bundle)?;
    for action in &response.actions {
        let scope = ActionScope::parse(&action.id)?;
        if !bundle.manifest().invoke_actions().contains(&scope) {
            return Err(PluginBrokerError::ResponseActionDenied(scope));
        }
    }
    Ok(response)
}

type PathError = fn(PathBuf, io::Error) -> PluginBrokerError;

fn canonical_file(path: &Path, error: PathError) -> Result<PathBuf, PluginBrokerError> {
    let canonical =
        std::fs::canonicalize(path).map_err(|source| error(path.to_path_buf(), source))?;
    if !canonical.is_file() {
        return Err(error(
            canonical.clone(),
            io::Error::new(io::ErrorKind::InvalidInput, "path is not a regular file"),
        ));
    }
    Ok(canonical)
}

fn canonical_directory(path: &Path) -> Result<PathBuf, PluginBrokerError> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|source| PluginBrokerError::PluginDirectory(path.to_path_buf(), source))?;
    if !canonical.is_dir() {
        return Err(PluginBrokerError::PluginDirectory(
            canonical.clone(),
            io::Error::new(io::ErrorKind::InvalidInput, "path is not a directory"),
        ));
    }
    Ok(canonical)
}

fn escape_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|byte| std::ascii::escape_default(*byte))
        .map(char::from)
        .collect()
}

#[derive(Debug, Error)]
pub enum PluginBrokerError {
    #[error("failed to locate the current cterm executable: {0}")]
    CurrentExecutable(#[source] io::Error),
    #[error("failed to resolve cterm executable `{0}`: {1}")]
    ApplicationExecutable(PathBuf, #[source] io::Error),
    #[error("the resolved cterm executable has no package directory")]
    ApplicationDirectory,
    #[error("failed to resolve plugin host `{0}`: {1}")]
    HostExecutable(PathBuf, #[source] io::Error),
    #[error("resolved plugin host `{0}` is not the package-relative cterm sibling")]
    HostOutsideApplicationDirectory(PathBuf),
    #[error("failed to resolve plugin directory `{0}`: {1}")]
    PluginDirectory(PathBuf, #[source] io::Error),
    #[error(transparent)]
    Package(#[from] PluginPackageError),
    #[error("plugin package `{package}` is not a direct child of `{root}`")]
    PackageOutsidePluginRoot { root: PathBuf, package: PathBuf },
    #[error("plugin `{plugin}` does not declare command `{command}`")]
    CommandNotDeclared {
        plugin: PluginId,
        command: CommandId,
    },
    #[error("plugin approval is required for {missing:?}; content changed: {content_changed}")]
    GrantRequired {
        missing: BTreeSet<ActionScope>,
        content_changed: bool,
    },
    #[error("plugin response requested an ungranted action `{0}`")]
    ResponseActionDenied(ActionScope),
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error("plugin timeout must be non-zero and at most {maximum:?}")]
    InvalidTimeout { maximum: Duration },
    #[error("failed to launch plugin host `{path}`: {source}")]
    Spawn {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("plugin host did not expose its piped {0}")]
    MissingPipe(&'static str),
    #[error("failed to write the bounded plugin request: {0}")]
    WriteRequest(#[source] io::Error),
    #[error("failed to read plugin host stdout: {0}")]
    ReadStdout(#[source] io::Error),
    #[error("failed to read plugin host stderr: {0}")]
    ReadStderr(#[source] io::Error),
    #[error("plugin host stdout exceeded its {limit}-byte limit")]
    StdoutLimitExceeded { limit: usize },
    #[error("plugin host stderr exceeded its {limit}-byte limit")]
    StderrLimitExceeded { limit: usize },
    #[error("plugin I/O task failed: {0}")]
    IoTask(#[source] JoinError),
    #[error("failed while waiting for the plugin process tree: {0}")]
    Wait(#[source] io::Error),
    #[error("plugin invocation exceeded {limit:?} (termination began after {elapsed:?})")]
    TimedOut { limit: Duration, elapsed: Duration },
    #[error("failed to terminate the plugin process tree: {0}")]
    Terminate(#[source] io::Error),
    #[error("failed while reaping the terminated plugin process tree: {0}")]
    TerminateWait(#[source] io::Error),
    #[error("plugin process tree did not terminate within {limit:?}")]
    TerminationTimedOut { limit: Duration },
    #[error("plugin host failed with {status}: {stderr}")]
    HostFailed { status: String, stderr: String },
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Instant as StdInstant;

    use cterm_plugin_api::{encode_response_frame, MANIFEST_FILE, MODULE_FILE};

    use super::*;

    const MANIFEST: &str = r#"manifest_version = 1
id = "org.example.broker"
name = "Broker fixture"
version = "1.0.0"
abi = "1.0"

[[commands]]
id = "run"
title = "Run"

[capabilities.invoke-actions]
allow = ["cterm:new-tab"]
"#;

    struct Fixture {
        _directory: tempfile::TempDir,
        plugins_root: PathBuf,
        plugin: PluginId,
        command: CommandId,
        bundle: PluginBundle,
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let plugins_root = directory.path().join("plugins");
        let plugin = PluginId::parse("org.example.broker").unwrap();
        let package = plugins_root.join(plugin.as_str());
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join(MANIFEST_FILE), MANIFEST).unwrap();
        fs::write(package.join(MODULE_FILE), b"\0asm\x01\0\0\0").unwrap();
        let plugins_root = fs::canonicalize(plugins_root).unwrap();
        let bundle = PluginBundle::load(&package).unwrap();
        Fixture {
            _directory: directory,
            plugins_root,
            plugin,
            command: CommandId::parse("run").unwrap(),
            bundle,
        }
    }

    fn approved(bundle: &PluginBundle) -> GrantStore {
        let mut grants = GrantStore::default();
        grants
            .approve(bundle, bundle.manifest().invoke_actions().clone())
            .unwrap();
        grants
    }

    fn broker(fixture: &Fixture) -> PluginBroker {
        PluginBroker::for_test(
            HostCommand::new(std::env::current_exe().unwrap()),
            fixture.plugins_root.clone(),
            PluginBrokerTimeout::default(),
        )
    }

    #[cfg(unix)]
    fn scripted_host(script: impl Into<OsString>) -> HostCommand {
        let mut host = HostCommand::new(fs::canonicalize("/bin/sh").unwrap());
        host.prefix_arguments = vec!["-c".into(), script.into(), "cterm-broker-fixture".into()];
        host
    }

    #[cfg(unix)]
    fn responding_host(script: impl AsRef<str>) -> HostCommand {
        scripted_host(format!(
            "while IFS= read -r _; do :; done; {}",
            script.as_ref()
        ))
    }

    #[cfg(windows)]
    fn responding_host(script: impl AsRef<str>) -> HostCommand {
        scripted_host(format!(
            "[Console]::OpenStandardInput().CopyTo([IO.Stream]::Null); {}",
            script.as_ref()
        ))
    }

    #[cfg(windows)]
    fn scripted_host(script: impl Into<OsString>) -> HostCommand {
        let system_root = std::env::var_os("SystemRoot").unwrap();
        let system_root_path = PathBuf::from(&system_root);
        let powershell = system_root_path
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        let mut host = HostCommand::new(fs::canonicalize(powershell).unwrap());
        host.prefix_arguments = vec![
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script.into(),
        ];
        host.required_environment = vec![("SystemRoot".into(), system_root)];
        host
    }

    fn response(action: &str) -> Vec<u8> {
        encode_response_frame(&proto::PluginResponse {
            abi_major: ABI_MAJOR,
            abi_minor: ABI_MINOR,
            actions: vec![proto::ActionInvocation {
                id: action.to_string(),
                parameter: None,
            }],
            diagnostics: Vec::new(),
        })
        .unwrap()
    }

    #[cfg(unix)]
    fn response_host(frame: &[u8]) -> HostCommand {
        let escaped = frame
            .iter()
            .map(|byte| format!("\\{byte:03o}"))
            .collect::<String>();
        responding_host(format!("printf '{escaped}'"))
    }

    #[cfg(windows)]
    fn response_host(frame: &[u8]) -> HostCommand {
        use base64::Engine as _;

        let frame = base64::engine::general_purpose::STANDARD.encode(frame);
        responding_host(format!(
            "$b=[Convert]::FromBase64String('{frame}'); [Console]::OpenStandardOutput().Write($b,0,$b.Length)"
        ))
    }

    #[test]
    fn timeout_policy_has_a_hard_ceiling() {
        assert!(PluginBrokerTimeout::new(Duration::ZERO).is_err());
        assert!(PluginBrokerTimeout::new(MAX_INVOCATION_TIMEOUT).is_ok());
        assert!(
            PluginBrokerTimeout::new(MAX_INVOCATION_TIMEOUT + Duration::from_nanos(1)).is_err()
        );
    }

    #[test]
    fn runner_is_resolved_only_as_a_canonical_package_sibling() {
        let directory = tempfile::tempdir().unwrap();
        let application_directory = directory.path().join("bin");
        let plugins_root = directory.path().join("plugins");
        fs::create_dir_all(&application_directory).unwrap();
        fs::create_dir_all(&plugins_root).unwrap();
        let application = application_directory.join("cterm-fixture");
        let host = application_directory.join(PLUGIN_HOST_EXECUTABLE_NAME);
        fs::write(&application, b"fixture").unwrap();
        fs::write(&host, b"fixture").unwrap();

        let broker =
            PluginBroker::from_application_executable(&application, &plugins_root).unwrap();
        assert!(broker.host_path().is_absolute());
        assert_eq!(broker.host_path(), fs::canonicalize(host).unwrap());
        assert_eq!(
            broker.plugins_root(),
            fs::canonicalize(plugins_root).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn package_symlinks_cannot_escape_the_plugin_root() {
        let fixture = fixture();
        let escaped = fixture._directory.path().join("escaped");
        fs::create_dir(&escaped).unwrap();
        fs::write(
            escaped.join(MANIFEST_FILE),
            MANIFEST.replace("org.example.broker", "org.example.escape"),
        )
        .unwrap();
        fs::write(escaped.join(MODULE_FILE), b"\0asm\x01\0\0\0").unwrap();
        std::os::unix::fs::symlink(&escaped, fixture.plugins_root.join("org.example.escape"))
            .unwrap();

        let error = broker(&fixture)
            .load_package(&PluginId::parse("org.example.escape").unwrap())
            .unwrap_err();
        assert!(matches!(
            error,
            PluginBrokerError::PackageOutsidePluginRoot { .. }
        ));
    }

    #[tokio::test]
    async fn package_and_grants_are_checked_before_launch() {
        let fixture = fixture();
        let broker = broker(&fixture);
        let missing = PluginId::parse("org.example.missing").unwrap();
        let error = broker
            .invoke(&GrantStore::default(), &missing, &fixture.command)
            .await
            .unwrap_err();
        assert!(matches!(error, PluginBrokerError::Package(_)));

        let error = broker
            .invoke(&GrantStore::default(), &fixture.plugin, &fixture.command)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PluginBrokerError::GrantRequired {
                content_changed: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn changed_package_never_inherits_a_stale_grant() {
        let fixture = fixture();
        let grants = approved(&fixture.bundle);
        fs::write(
            fixture.bundle.root().join(MODULE_FILE),
            b"\0asm\x01\0\0\0changed",
        )
        .unwrap();
        let error = broker(&fixture)
            .invoke(&grants, &fixture.plugin, &fixture.command)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PluginBrokerError::GrantRequired {
                content_changed: true,
                ..
            }
        ));
    }

    #[test]
    fn malformed_and_undeclared_responses_fail_closed() {
        let fixture = fixture();
        let grants = approved(&fixture.bundle);
        assert!(matches!(
            validate_response(&grants, &fixture.bundle, b"not protobuf"),
            Err(PluginBrokerError::Wire(_))
        ));
        assert!(matches!(
            validate_response(
                &grants,
                &fixture.bundle,
                &response("cterm:close-window")
            ),
            Err(PluginBrokerError::ResponseActionDenied(scope))
                if scope.as_str() == "cterm:close-window"
        ));
    }

    #[tokio::test]
    async fn output_readers_reject_limit_plus_one() {
        let (mut writer, reader) = tokio::io::duplex(32);
        let write = tokio::spawn(async move {
            writer.write_all(b"12345").await.unwrap();
        });
        assert!(matches!(
            read_bounded(reader, 4, Stream::Stdout).await,
            Err(PluginBrokerError::StdoutLimitExceeded { limit: 4 })
        ));
        write.await.unwrap();

        let (mut writer, reader) = tokio::io::duplex(32);
        let write = tokio::spawn(async move {
            writer.write_all(b"12345").await.unwrap();
        });
        assert!(matches!(
            read_bounded(reader, 4, Stream::Stderr).await,
            Err(PluginBrokerError::StderrLimitExceeded { limit: 4 })
        ));
        write.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_host_response_is_rejected() {
        let fixture = fixture();
        #[cfg(unix)]
        let host = responding_host("printf malformed");
        #[cfg(windows)]
        let host = responding_host(
            "$b=[Text.Encoding]::ASCII.GetBytes('malformed'); [Console]::OpenStandardOutput().Write($b,0,$b.Length)",
        );
        let broker = PluginBroker::for_test(
            host,
            fixture.plugins_root.clone(),
            PluginBrokerTimeout::default(),
        );
        let error = broker
            .invoke(
                &approved(&fixture.bundle),
                &fixture.plugin,
                &fixture.command,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, PluginBrokerError::Wire(_)));
    }

    #[tokio::test]
    async fn valid_response_round_trips_through_the_sibling_protocol() {
        let fixture = fixture();
        let host = response_host(&response("cterm:new-tab"));
        let broker = PluginBroker::for_test(
            host,
            fixture.plugins_root.clone(),
            PluginBrokerTimeout::default(),
        );
        let output = broker
            .invoke(
                &approved(&fixture.bundle),
                &fixture.plugin,
                &fixture.command,
            )
            .await
            .unwrap();
        assert_eq!(output.response().actions[0].id, "cterm:new-tab");
        assert!(output.host_stderr().is_empty());
    }

    #[tokio::test]
    async fn oversized_host_stdout_is_rejected_and_reaped() {
        let fixture = fixture();
        #[cfg(unix)]
        let host = responding_host(format!(
            "chunk='{}'; i=0; while [ \"$i\" -lt 257 ]; do printf %s \"$chunk\"; i=$((i+1)); done",
            "x".repeat(4096)
        ));
        #[cfg(windows)]
        let host = responding_host(format!(
            "$b=New-Object byte[] {}; [Console]::OpenStandardOutput().Write($b,0,$b.Length)",
            MAX_FRAME_BYTES + 1
        ));
        let broker = PluginBroker::for_test(
            host,
            fixture.plugins_root.clone(),
            PluginBrokerTimeout::default(),
        );
        let error = broker
            .invoke(
                &approved(&fixture.bundle),
                &fixture.plugin,
                &fixture.command,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PluginBrokerError::StdoutLimitExceeded {
                limit: MAX_FRAME_BYTES
            }
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_the_unix_process_tree() {
        let fixture = fixture();
        let pid_file = fixture._directory.path().join("descendant.pid");
        let mut host = responding_host("(while :; do :; done) & echo $! > \"$1\"");
        host.prefix_arguments.push(pid_file.as_os_str().to_owned());
        let broker = PluginBroker::for_test(
            host,
            fixture.plugins_root.clone(),
            PluginBrokerTimeout::new(Duration::from_millis(150)).unwrap(),
        );
        let grants = approved(&fixture.bundle);
        let started = StdInstant::now();
        let error = broker
            .invoke(&grants, &fixture.plugin, &fixture.command)
            .await
            .unwrap_err();
        assert!(
            matches!(error, PluginBrokerError::TimedOut { .. }),
            "unexpected broker error: {error:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));

        let pid = fs::read_to_string(pid_file).unwrap();
        let pid = pid.trim();
        let deadline = StdInstant::now() + Duration::from_secs(1);
        loop {
            let status = std::process::Command::new("/bin/sh")
                .args([
                    "-c",
                    "kill -0 \"$1\" 2>/dev/null",
                    "cterm-broker-fixture",
                    pid,
                ])
                .status()
                .unwrap();
            if !status.success() {
                break;
            }
            assert!(StdInstant::now() < deadline, "descendant {pid} survived");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn timeout_terminates_the_windows_job_after_leader_exit() {
        let fixture = fixture();
        let host = responding_host(
            "$null=Start-Process -FilePath (Join-Path $PSHOME 'powershell.exe') -ArgumentList '-NoLogo','-NoProfile','-NonInteractive','-Command','while ($true) {}' -PassThru; exit 0",
        );
        let broker = PluginBroker::for_test(
            host,
            fixture.plugins_root.clone(),
            PluginBrokerTimeout::new(Duration::from_millis(150)).unwrap(),
        );
        let grants = approved(&fixture.bundle);
        let started = StdInstant::now();
        let error = broker
            .invoke(&grants, &fixture.plugin, &fixture.command)
            .await
            .unwrap_err();
        assert!(
            matches!(error, PluginBrokerError::TimedOut { .. }),
            "unexpected broker error: {error:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
