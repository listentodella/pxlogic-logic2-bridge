# PXLogic Logic 2 Bridge

Standalone bridge that exposes a PXLogic USB capture as a Logic Pro 16-compatible
GraphServer stream for an unmodified Saleae Logic 2 installation.

The repository contains the bridge protocol, native GraphServer host, Tauri
launcher, PXLogic capture helper, and the firmware/FPGA resources required by
the helper. It does not contain or modify Saleae Logic itself.

The desktop launcher detects connected PXLogic devices, reports their model,
serial number, and USB link speed, validates its packaged firmware/FPGA
resources, and exposes PXView-compatible 1.8 V, 2.5 V, 3.3 V, and 5.0 V
hardware-level choices. Logic 2 remains the source of truth for channels,
sample rate, capture control, triggers, filters, and analyzers.

When the launcher starts Logic 2, it also installs or refreshes three separate
High Level Analyzer extensions: `QMI8660`, `QMI8658A`, and `QMA6100P`. Attach
the matching HLA to Logic 2's built-in I2C or SPI analyzer to decode register
names, fields, FIFO status, and physical sensor samples.

## Development checks

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
