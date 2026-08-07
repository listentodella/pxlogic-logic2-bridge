# PXLogic Logic 2 Bridge

This tool feeds live PXLogic samples into an unmodified official Saleae Logic
application. It keeps Logic 2's GraphServer, renderer, digital trigger, glitch
filter, analyzers, measurements, markers, and `.sal` session handling intact.

The bridge is intentionally outside `saleae-priv`. The desktop client contains
the PXLogic `usb_smoke` binary, bitstreams, firmware, and the platform native
host. It reuses the Electron/Node runtime, GraphServer, and Python runtime from
the user's official Logic installation instead of packaging a second runtime.

## Support boundary

The native callback hook is verified for the official macOS arm64 Logic `2.4.46`
GraphServer. Windows x64 `2.4.46` is now an explicit experimental validation
target: its PE CodeView identity, callback RVA, prologue, and Microsoft x64
aggregate-argument layout are checked, but it must still complete a real PXLogic
capture before the profile is promoted to `verified`. An unknown build is
refused instead of being patched optimistically.

| Layer | macOS arm64 | macOS x64 | Windows x64 | Linux x64 |
| --- | --- | --- | --- | --- |
| WebSocket proxy and data conversion | Tested | Tested in CI | Tested in CI | Tested in CI |
| PXLogic `usb_smoke` helper | Supported by the PXLogic workspace | Supported by the PXLogic workspace | Supported by the PXLogic workspace | Supported by the PXLogic workspace |
| Logic 2.4.46 GraphServer identity | UUID + SHA verified | Profile pending | CodeView + SHA verified | Build ID + SHA recorded |
| Logic 2.4.46 GraphServer injection | Verified | Not implemented | Experimental host, pending capture | Reverse-engineered, pending capture |
| Complete live bridge | **Supported** | Not supported | Validation build | Not supported |

Building the portable JavaScript and PXLogic USB layers on another operating
system does not make its native injection path compatible. Each Logic build has
a platform-specific module format, binary fingerprint, calling convention, and
code-patching implementation. A platform is listed as supported only after
those details and a real capture have been verified.

## Prerequisites

- macOS arm64
- The official Logic app, version `2.4.46`

The packaged desktop client does not require Node.js, npm, Rust, Cargo, or the
Xcode Command Line Tools on the user's machine. Those tools are build-time
dependencies only.

### GraphServer profiles

`compatibility/profiles.json` is the compatibility manifest. It records the
platform-native identity, SHA-256, callback offset, and function prologue for
each inspected build. The identity is platform-specific: macOS uses Mach-O
`LC_UUID`, Linux uses the ELF GNU Build ID, and Windows uses PE/CodeView
identity when present (falling back to the PE timestamp for diagnostics).
These identifiers are not expected to be equal across platforms, even when
the Logic application version is the same.

The profile is selected at startup from the GraphServer binary in the
user-selected installation. The client extracts the native identity and
SHA-256 directly from that binary; the native host then receives the selected
identity, callback offset, and prologue as runtime data. The outer Logic version
is retained as metadata but is not a hard match: if a later Logic release ships
the byte-for-byte identical GraphServer, the existing profile can be reused.
A dynamically extracted UUID/Build ID/CodeView identity alone never authorizes
patching. A changed binary still requires a new callback offset, prologue, and
ABI review; the Windows package exposes this only through the explicit
experimental validation path.

## Continuous integration

`.github/workflows/build-logic2-bridge.yml` checks the portable bridge layer and
builds a Linux AppImage plus executable tarball and a Windows portable ZIP on
self-hosted x64 runners. Pushes run those two self-hosted builds by default;
`workflow_dispatch` can select `windows`, `linux`, `self-hosted`, `macos`, or
`all`. macOS checks and the runnable
`pxlogic-logic2-bridge-client-macos-arm64` Tauri app archive is manual so an unavailable
GitHub-hosted runner cannot block Windows or Linux artifacts.
The application bundle contains:

- the lightweight Tauri desktop launcher (it reuses the selected Logic app's
  Node/Electron runtime for the bridge);
- the bridge source and prebuilt native GraphServer host;
- the release `usb_smoke` capture helper;
- the PXLogic FPGA bitstreams and MCU firmware used by that helper.

The packages deliberately do not contain Saleae binaries. At runtime they load
GraphServer and the Python runtime from the user's selected official Logic
installation. The Windows portable package contains the experimental
`graph-host.exe`; its UI clearly labels the profile as experimental until a
PXLogic capture is confirmed. The Linux hook remains pending live validation.

The workflow has manual `analyze_official_packages` and `logic_version` inputs.
When enabled, GitHub downloads that official Logic version's Linux AppImage and
Windows installer, extracts the complete version metadata plus the platform
GraphServer, and writes JSON fingerprint reports to the workflow summary. The
`analysis-only` platform option skips all Bridge package builds for this task.
The Windows job also derives an `OnDataBuffer` candidate and prologue
from signature-string references and the PE runtime-function table. Its
extractor reverses the Advanced Installer first-sector XOR and uses Windows'
built-in CAB extractor; it does not execute or silently install the package.
The reports contain no Saleae binaries; artifact upload is best-effort because
the job summary remains available when the repository artifact quota is full.

## Desktop client

On macOS, unzip `pxlogic-logic2-bridge-macos-arm64.zip`, move `PXLogic Bridge.app`
to Applications, and launch it normally. On Windows, unzip
`pxlogic-logic2-bridge-windows-x64.zip` and double-click `PXLogic Bridge.exe`.
On Linux, make the AppImage executable and launch it, or extract the tarball
which preserves the executable bit.

On Windows, the client suppresses console windows for its Bridge helpers and
maximizes the Logic window that it launches. Existing Logic windows are left
unchanged.

The client fingerprints the GraphServer inside the user-selected installation
at runtime, so the package does not contain a hardcoded Saleae DLL.

The client searches `/Applications`, `~/Applications`, and Spotlight for Saleae
Logic. It displays the detected version, accepts a manually entered or selected
`.app` path, remembers the settings, and remains available from the macOS menu
bar while its window is hidden.

Click `Start Logic 2` after an installation matching a verified compatibility
profile is shown.
The client launches Logic with the required `--useExistingGraph` and
`--graphPort` arguments. In Logic 2 select the Demo Logic Pro 16 device as the
session device.

Automatic port mode asks macOS for a free loopback TCP port, then passes the
actual value to Logic 2. The port is not fixed by Logic. In fixed mode the
selected value is preferred; if it is occupied, the bridge falls back to an
available port and reports the actual endpoint in the client.

CI builds are unsigned unless Apple signing credentials are provided. For an
unsigned downloaded build, macOS may require the first launch through Finder's
`Open` context-menu action. Public distribution without that prompt requires
Developer ID signing and Apple notarization.

## Command-line development

The bridge core has no third-party Node package dependencies. From a source
checkout with Node.js 18 or newer, a built PXLogic helper, and Xcode Command
Line Tools, it can still be started directly:

```sh
node tools/logic2-bridge/index.cjs \
  --app "/Applications/Saleae Logic.app" \
  --screen-quadrant 3
```

The launcher starts a private GraphServer, exposes an automatically allocated
Logic-facing endpoint, then launches the official app with `--useExistingGraph`.
When capture starts, the bridge follows the channel, sample rate, and voltage
settings sent by Logic 2 and starts PXLogic automatically. Use `--port 12472`
to request a fixed port or `--port auto` explicitly.

For the local app used during development:

```sh
node tools/logic2-bridge/index.cjs \
  --app "/Volumes/tp7100s/work/logic2-anti/Saleae Logic.app" \
  --screen-quadrant 3 \
  --remote-debugging-port 9227
```

Use `--enabled-channels 0,1,2,3` only as the initial fallback. The Logic UI
selection is authoritative after the session sends `EnableChannels`.

## Data and control semantics

- PXLogic always receives `--glitch-filter` (one hardware sample period).
- Logic 2's Glitch Filter remains GraphServer software post-processing.
- Logic 2 digital triggers remain GraphServer real-time processing. Trigger
  conditions are deliberately not sent to PXLogic hardware.
- PXLogic Cross stripes are converted to the Logic Pro 16 callback layout
  before they enter the native GraphServer callback.
- The bridge uses `stream` mode and continuously forwards samples; GraphServer
  decides trigger time and Logic 2 decides when the post-trigger interval ends.

## Diagnostics

`--dry-run` validates the app version, GraphServer resources, PXLogic helper,
firmware, bitstreams, and native host build without launching Logic:

```sh
node tools/logic2-bridge/index.cjs \
  --app "/Applications/Saleae Logic.app" \
  --dry-run
```

GraphServer logs are written under:

```text
~/Library/Application Support/PXLogic/logic2-bridge/graphio.log
```

## Verified live path

The standalone bridge was verified with the official Logic `2.4.46` app and a
PX-Logic U3 device at 50 MS/s with eight enabled digital channels. A Logic UI
D4 rising-edge trigger with a one-second post-trigger duration produced:

```text
Logic digital-trigger=D4 rising (GraphServer only)
pxlogic-hardware-glitch-filter=1T pxlogic-hardware-trigger=off
[capture:info] ... trigger=false ... glitch_filter=true
StopCapture
```

This proves that the PXLogic hardware streams continuously while the unmodified
GraphServer detects the edge and Logic 2 ends the capture after its configured
post-trigger interval.
