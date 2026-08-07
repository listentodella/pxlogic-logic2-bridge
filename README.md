# PXLogic Logic 2 Bridge

Standalone bridge that exposes a PXLogic USB capture as a Logic Pro 16-compatible
GraphServer stream for an unmodified Saleae Logic 2 installation.

The repository contains the bridge protocol, native GraphServer host, Tauri
launcher, PXLogic capture helper, and the firmware/FPGA resources required by
the helper. It does not contain or modify Saleae Logic itself.

## Development checks

```sh
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
