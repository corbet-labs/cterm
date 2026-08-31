# Command plugin architecture

cterm's plugin work currently provides a fail-closed package and ABI
foundation, an isolated one-shot runner, and an application-side broker. It
does **not** yet discover or execute plugins in released UI builds. Permission
prompts, grant-file persistence, and frontend command integration are the next
implementation stage. Release packages already include the runner as a
verified sibling executable.

## Version 1 package

Version 1 deliberately uses a fixed two-file directory. There is no
manifest-controlled entrypoint and no downloader.

```text
<platform-local-data>/plugins/io.example.split/
  cterm-plugin.toml
  plugin.wasm
```

```toml
manifest_version = 1
id = "io.example.split"
name = "Split helper"
version = "0.1.0"
abi = "1.0"

[[commands]]
id = "split-right"
title = "Split Right"

[capabilities.invoke-actions]
allow = ["cterm:split-pane"]
```

Plugin IDs are portable lowercase reverse-DNS identifiers. Command IDs are
portable lowercase slugs and appear to cterm as
`plugin:<plugin-id>/<command-id>`. Manifests reject unknown fields, duplicate
commands or scopes, wildcard scopes, unsupported versions, oversized files,
invalid WebAssembly modules, and paths escaping the package directory.

## ABI and authority

The guest ABI is a small, versioned protobuf request/response exchange. One
request invokes one declared command; the response may request at most 32
exact `cterm:*` built-in actions and return bounded diagnostics. It cannot
request plugin recursion, raw terminal writes, terminal contents, filesystem,
network, environment, daemon, or native UI access.

The ABI is intentionally independent of cterm's Rust UI enum layout. The
application broker verifies each wire action against the exact manifest and
grant before a future frontend converts it and passes it through the same
native action policy used by shortcuts. Managed mode will initially disable
plugins.

## Package identity and grants

cterm computes the package identity over the exact manifest and module bytes
it loaded:

```text
SHA256(
  "cterm-plugin-package-v1\0" ||
  manifest_length || manifest_bytes ||
  module_length || module_bytes
)
```

The loaded module bytes remain in memory between validation and execution so
authorization cannot be separated from the content that was checked. Local
grants bind a plugin ID and this digest to an exact set of allowed `cterm:*`
actions. Changing either file invalidates the old grant automatically.

Package size limits are enforced from file metadata before allocation and
again with a `limit + 1` bounded read. Length changes between metadata, open,
and completed read are rejected, so sparse oversized files and concurrent
growth cannot bypass the in-memory bounds.

Grants are machine-local integrity decisions. Frontend integration will store
them in the platform local-data directory using atomic replacement, outside
cterm's Git-synchronized configuration. The broker accepts a validated
in-memory grant store and never creates or expands grants itself.

## Application broker

`cterm-app` resolves the canonical `cterm-plugin-host` sibling next to the
canonical application executable; it never searches `PATH`. A plugin package
must resolve as the exact named direct child of one canonical plugin root, so
package-directory symlinks cannot redirect execution elsewhere.

Before launch, the broker reloads the package, verifies that the command is
declared, and requires an exact digest grant covering every manifest action.
It sends only the bounded v1 request over a piped stdin. Stdout is limited to
one 1 MiB response frame and stderr to 64 KiB, both read concurrently to avoid
pipe deadlocks. Successful responses are decoded and checked against the same
manifest and grant again before they can reach native policy.

The broker starts a two-second deadline immediately before launch and includes
all process and stdio work in it (configurable downward or up to a hard
ten-second ceiling). On Unix the runner leads a dedicated process group; on
Windows it is suspended, assigned to a Job Object, then resumed. Timeout or
I/O-policy failure force-kills and reaps the whole group/job. This lifecycle is
provided by the tested Rust `process-wrap` implementation rather than local
platform FFI.

## Runner boundary

`cterm-plugin-host` is a separate, package-relative sibling executable. It
accepts exactly one framed request and exits, and creates a fresh Wasmi 1.1
engine, WASIp1 context, and store for that request. Before compilation it
reloads the fixed package files and verifies their digest against the digest
supplied by the broker. The guest gets only bounded stdin/stdout/stderr,
one fixed argv entry, an empty environment, and no preopened directory or
cterm-specific import.

Each store has fuel metering, Wasmi's strict compile limits, a 16 MiB memory
ceiling, at most one memory, instance, and table, bounded table and interpreter
stack growth, and 1 MiB stdout / 64 KiB stderr ceilings. The runner validates
the response frame and rejects actions absent from the verified manifest.
Adversarial fixtures cover loops, memory growth, output flooding, traps,
malformed protobuf, undeclared actions, digest mismatches, unsupported imports,
and ambient environment/filesystem state.

Fuel is not a wall-clock deadline, so the application broker independently
enforces the process deadline and tree termination. Future frontend work must
apply managed-mode and native action policy after the broker returns. The
runner deliberately contains no downloader, grant storage, permission UI,
daemon dependency, or frontend dependency.

Keeping that runtime out of the UI process limits the effect of guest bugs. It
is not a complete operating-system sandbox against a native runtime exploit;
platform sandboxing remains a separate hardening layer.
