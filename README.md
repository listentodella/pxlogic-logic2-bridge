# PXLogic Logic 2 Bridge

Standalone bridge that exposes a PXLogic USB capture as a Logic Pro 16-compatible
GraphServer stream for an unmodified Saleae Logic 2 installation.

The repository contains the bridge protocol, native GraphServer host, Tauri
launcher, PXLogic capture helper, and the firmware/FPGA resources required by
the helper. It does not contain or modify Saleae Logic itself.

The desktop launcher detects connected PXLogic devices, reports their model,
serial number, and USB link speed, validates its packaged firmware/FPGA
resources, and exposes a manually entered PXView-compatible hardware voltage
threshold. Logic 2 remains the source of truth for channels, sample rate,
capture control, triggers, filters, and analyzers.

A first-run walkthrough explains that split, inline `?` affordances explain the
hardware terms at the point of use, and an always-on-top status panel reveals
itself once the Bridge goes live so the data link stays visible while Logic 2
has focus. See [Bridge guidance and status panel
behavior](docs/logic2-bridge-guidance.md).

PXLogic FPGA setup is performed once per Bridge launch. Logic 2 Start/Stop
operations reuse that prepared state, keep PXLogic hardware outputs disabled,
and only arm or stop the input sampler. The voltage field is a comparator
threshold. It must be selected for the actual probe, target, and signal quality;
the target's nominal logic voltage alone is not enough to derive a reliable
threshold.

GraphServer compatibility is resolved entirely offline. Exact built-in profiles
are used directly; an unknown binary is analyzed locally and recorded as either
an experimental candidate or unsupported. See the
[manual GraphServer profile procedure](docs/graphserver-profile-manual.md) when
automatic analysis cannot produce a unique candidate or before promoting a
candidate to verified support.

## Logic 2 MCP activity and approval

The Tauri launcher also runs a loopback-only Streamable HTTP proxy in front of
Logic 2's MCP server. It is independent of Bridge capture sessions, so its
always-on-top activity window works with either PXLogic or supported real Saleae
hardware. The proxy defaults to `127.0.0.1:10531` and forwards to Logic 2 on
`127.0.0.1:10530`; the window reports a fallback port if the preferred one is
occupied.

The window observes the real tool catalogue and request/response activity. Known
read/configuration calls proceed automatically; capture lifecycle and unknown
tools wait for approval and are refused after 30 seconds. The launcher does not
implement an agent or bind the feature to a specific MCP client. See [Logic 2 MCP
proxy and activity window](docs/logic2-mcp-proxy.md).

The proxy also adds four timing-marker tools Logic 2's MCP does not offer, so an
agent can record where it found something instead of only reading. They appear in
`tools/list` beside Logic 2's own fifteen. Markers are reached through the
renderer of a Logic 2 the Bridge launched, which means they need a Bridge session
and an open capture; no DevTools window is ever shown. The details, and what a
Logic 2 upgrade could break, are in the same document.

## Development checks

Every Bridge feature must pass the same delivery gate locally and on the
macOS GitHub Actions runner:

```sh
pnpm run verify:delivery
```

The command does not access PXLogic hardware. It writes a machine-readable
report when called with `--report PATH`, records Git provenance and manifest
versions, and fails when any required Node, PXLogic core/helper, or Tauri check
fails. See the [Bridge delivery contract](docs/logic2-bridge-delivery.md) for
the feature brief and acceptance-evidence requirements.

```sh
npm --prefix tools/logic2-bridge/client ci
npm --prefix tools/logic2-bridge/tauri-client ci

# Or install both UI/runtime workspaces from the repository root with pnpm.
pnpm install
pnpm tauri dev

npm --prefix tools/logic2-bridge ci
npm --prefix tools/logic2-bridge check
npm --prefix tools/logic2-bridge test
cargo check --release --bin usb_smoke
```

Tauri source checks do not need generated payload binaries:

```sh
TAURI_CONFIG='{"bundle":{"resources":[]}}' \
  npm --prefix tools/logic2-bridge/tauri-client run check
```

Use the GitHub Actions workflow for platform-specific helper/native-host
payloads and packaged applications.
