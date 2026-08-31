use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use cterm_plugin_api::{BundleDigest, MAX_FRAME_BYTES};
use cterm_plugin_host::{invoke, read_bounded, InvocationLimits};

#[derive(Debug, Parser)]
#[command(about = "Run one verified cterm command plugin invocation")]
struct Args {
    /// Absolute plugin package directory containing the fixed manifest/module pair.
    #[arg(long)]
    package: PathBuf,
    /// Exact package digest previously observed and authorized by cterm.
    #[arg(long)]
    expected_digest: BundleDigest,
}

fn main() -> ExitCode {
    let args = Args::parse();
    clear_process_environment();
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            write_bounded_diagnostic(&error.to_string());
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let request = read_bounded(&mut io::stdin().lock(), MAX_FRAME_BYTES)?;
    let output = invoke(
        &args.package,
        args.expected_digest,
        &request,
        InvocationLimits::default(),
    )?;
    if !output.stderr().is_empty() {
        write_bounded_bytes_diagnostic("plugin stderr: ", output.stderr());
    }
    io::stdout().lock().write_all(output.response_frame())?;
    Ok(())
}

fn clear_process_environment() {
    let keys = std::env::vars_os()
        .map(|(key, _)| key)
        .collect::<Vec<OsString>>();
    for key in keys {
        std::env::remove_var(key);
    }
}

fn write_bounded_diagnostic(message: &str) {
    write_bounded_bytes_diagnostic("", message.as_bytes());
}

fn write_bounded_bytes_diagnostic(prefix: &str, bytes: &[u8]) {
    const MAX_RUNNER_DIAGNOSTIC_BYTES: usize = 4096;
    const RUNNER_PREFIX: &str = "cterm-plugin-host: ";
    let fixed_bytes = RUNNER_PREFIX.len() + prefix.len() + 1;
    let escaped = escape_bytes_bounded(
        bytes,
        MAX_RUNNER_DIAGNOSTIC_BYTES.saturating_sub(fixed_bytes),
    );
    let _ = writeln!(io::stderr().lock(), "{RUNNER_PREFIX}{prefix}{escaped}");
}

fn escape_bytes_bounded(bytes: &[u8], limit: usize) -> String {
    let mut escaped = String::with_capacity(limit.min(bytes.len()));
    for byte in bytes {
        let fragment = std::ascii::escape_default(*byte);
        if fragment.len() > limit.saturating_sub(escaped.len()) {
            break;
        }
        escaped.extend(fragment.into_iter().map(char::from));
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_never_splits_or_exceeds_the_output_budget() {
        assert_eq!(escape_bytes_bounded(b"a\nb", 5), "a\\nb");
        assert_eq!(escape_bytes_bounded(&[0xff, b'a'], 4), "\\xff");
        assert_eq!(escape_bytes_bounded(&[0xff], 3), "");
    }
}
