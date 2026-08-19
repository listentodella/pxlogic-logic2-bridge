# PXLogic Logic 2 Bridge

This tool feeds live PXLogic samples into an unmodified official Saleae Logic
application. It keeps Logic 2's GraphServer, renderer, digital trigger, glitch
filter, analyzers, measurements, markers, and `.sal` session handling intact.

The bridge is intentionally outside `saleae-priv`. The desktop client contains
the PXLogic `usb_smoke` binary, bitstreams, firmware, and the platform native
host. It reuses the Electron/Node runtime, GraphServer, and Python runtime from
the user's official Logic installation instead of packaging a second runtime.

## Support boundary

The native callback hook and live PXLogic capture are verified for the official
macOS arm64 Logic `2.4.36`, `2.4.45`, and `2.4.46` GraphServers. Windows x64
`2.4.46` is an explicit experimental validation target: its PE CodeView
identity, callback RVA, prologue, and Microsoft x64 aggregate-argument layout
are checked, but it must still complete a real PXLogic capture before the
profile is promoted to `verified`. Unknown builds are analyzed offline and may
run only when the evidence converges to one exact experimental candidate.

| Layer | macOS arm64 | macOS x64 | Windows x64 | Linux x64 |
| --- | --- | --- | --- | --- |
| WebSocket proxy and data conversion | Tested | Tested in CI | Tested in CI | Tested in CI |
| PXLogic `usb_smoke` helper | Supported by the PXLogic workspace | Supported by the PXLogic workspace | Supported by the PXLogic workspace | Supported by the PXLogic workspace |
| GraphServer identity | Logic 2.4.36/2.4.45/2.4.46 UUID + SHA verified | Profile pending | Logic 2.4.46 CodeView + SHA verified | Logic 2.4.46 Build ID + SHA recorded |
| GraphServer injection | Logic 2.4.36/2.4.45/2.4.46 verified | Not implemented | Experimental host, pending capture | Reverse-engineered, pending capture |
| Complete live bridge | **Supported** | Not supported | Validation build | Not supported |

Building the portable JavaScript and PXLogic USB layers on another operating
system does not make its native injection path compatible. Each Logic build has
a platform-specific module format, binary fingerprint, calling convention, and
code-patching implementation. A platform is listed as supported only after
those details and a real capture have been verified.

## Prerequisites

- macOS arm64
- An official macOS arm64 Logic app. Versions `2.4.36`, `2.4.45`, and `2.4.46`
  have built-in verified profiles; other exact builds use the offline
  experimental-candidate path when structural analysis succeeds.

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
patching. A changed binary still requires a new callback offset and prologue,
and remains experimental until its ABI and real capture behavior are reviewed.

For an unknown exact fingerprint, the desktop client runs a read-only analyzer
with Logic 2's embedded Node runtime. On macOS arm64 it resolves the
`OnDataBuffer` and source-file diagnostic string references through ARM64
`ADRP + ADD` instructions, then uses `LC_FUNCTION_STARTS` to recover the unique
function entry. It rejects an entry whose 16-byte patch window contains a
PC-relative instruction and records buffer-register/size-load patterns as ABI
confidence evidence. A known long signature is only a fallback. Windows x64
requires the method name and signature references to converge through the PE
runtime-function table and the entry bytes to match a maintained
trampoline-safe prologue. Linux x64 currently requires a unique locator
signature from a known profile. Success is stored as a `candidate`; ambiguity
or unsafe entry code is stored as `unsupported`. The built-in manifest always
takes precedence over local data.

The local result is stored in the platform application configuration directory
as `compatibility-analysis.json`. Records are keyed by platform, architecture,
native identity, and SHA-256, so updating Logic does not reuse a result for a
different GraphServer. Candidate and failure records are tied to the analyzer
version and are automatically retried after the analyzer changes. The Logic
section's `重新分析` action forces a retry with the current analyzer. No profile
lookup, telemetry, or other network request is made.

An automatic candidate is runnable through the UI's explicitly labeled
experimental path, but it is not equivalent to verified support. Use the
[manual GraphServer profile procedure](../../docs/graphserver-profile-manual.md)
to resolve automatic failures, inspect ABI/prologue safety, validate real
hardware capture, and promote a profile.

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

The PXLogic hardware panel uses the packaged capture helper for read-only USB
discovery. It shows the detected model, serial number, USB link speed, and the
packaged firmware/FPGA resource status, and it supports selecting a specific
device when more than one is connected. The selected device is checked again
immediately before the Bridge starts.

The PXLogic panel accepts the PXView-compatible voltage threshold directly from
`0.000 V` through `6.668 V` (default `1.800 V`). The value is passed unchanged
to the PXLogic helper; the driver's internal `0.5` factor is part of the device
DAC formula and is not a second user-facing conversion. The Bridge-owned value
is independent from Logic 2's nominal I/O-level selector. Logic 2 remains
authoritative for enabled channels, sample rate, capture control, triggers,
software glitch filters, and analyzers.

The launch checklist labels a matched GraphServer as `正式支持`, `实验验证`, or
`不可用`; a runnable experimental profile is never presented as formally
supported. It also keeps the session mapping visible: `Demo Logic Pro 16` is
the Logic 2 compatibility device, while the selected PXLogic remains the real
sample source.

During a capture, the desktop client shows the helper's effective sample rate,
enabled channels, comparator threshold, converted bytes, and the native host's
injected/queued bytes, callback underflows, and dropped bytes. A clean quality
status is shown only after native injection counters have been observed. The
trigger field reports the Logic 2 GraphServer trigger configuration; it does
not claim that a trigger fired because GraphServer does not expose that event
to the Bridge.

Click `Start Logic 2` after a verified profile is shown. For a locally analyzed
candidate, review the automatic-candidate status and use `启动实验验证`; an
unsupported result is not injected.
The client launches Logic with the required `--useExistingGraph` and
`--graphPort` arguments. In Logic 2 select the Demo Logic Pro 16 device as the
session device.

The Bridge does not bundle or install Logic 2 extensions and does not read or
write Logic 2's extension configuration. QST sensor analyzers are available
from the independent
[QST Sensor Decoders](https://github.com/listentodella/qst-sensor-decoders)
repository.

Automatic port mode asks macOS for a free loopback TCP port, then passes the
actual value to Logic 2. The port is not fixed by Logic. In fixed mode the
selected value is preferred; if it is occupied, the bridge falls back to an
available port and reports the actual endpoint in the client.

CI builds are ad-hoc signed so the application bundle and packaged resources
can be checked for integrity, but they are not Developer ID signed or notarized.
macOS may require the first launch through Finder's `Open` context-menu action.
Public distribution without that prompt requires Developer ID signing and
Apple notarization.

## Command-line development

The bridge core has no third-party Node package dependencies. From a source
checkout with Node.js 18 or newer, a built PXLogic helper, and Xcode Command
Line Tools, it can still be started directly:

```sh
node tools/logic2-bridge/index.cjs \
  --app "/Applications/Saleae Logic.app"
```

The launcher starts a private GraphServer, exposes an automatically allocated
Logic-facing endpoint, then launches the official app with `--useExistingGraph`.
Logic opens maximized by default. Use `--screen-quadrant 1` through `4` only
when a compact debugging layout is preferred.
When capture starts, the bridge follows the channel, sample rate, and voltage
settings sent by Logic 2 and starts PXLogic automatically. Use `--port 12472`
to request a fixed port or `--port auto` explicitly.

PXLogic's supported rates include the Logic Pro 16 `6.25 MHz` setting. It is
generated exactly from the 100 MHz hardware divider (`mode=7`, `div=15`), so the
GraphServer time base and physical sample clock remain identical.

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

- The Bridge prepares the PXLogic FPGA once before Logic 2 starts. Every Logic
  Start/Stop after that uses `--skip-prepare`, so starting a capture does not
  upload reset/main bitstreams or cycle the FPGA I/O banks.
- A capture explicitly disables PXLogic PWM0/PWM1, External Trigger, Trigger
  Out, and hardware trigger masks before arming the input sampler. A failed
  helper locks capture for the remainder of the Bridge session instead of
  automatically reconfiguring the FPGA; restart the Bridge to recover.
- PXLogic always receives `--glitch-filter` (one hardware sample period).
- Logic 2's Glitch Filter remains GraphServer software post-processing.
- Logic 2 digital triggers remain GraphServer real-time processing. Trigger
  conditions are deliberately not sent to PXLogic hardware.
- PXLogic Cross stripes are converted to the Logic Pro 16 callback layout
  before they enter the native GraphServer callback.
- The bridge uses `stream` mode and continuously forwards samples; GraphServer
  decides trigger time and Logic 2 decides when the post-trigger interval ends.

The hardware threshold is the actual comparator decision voltage, not the
target circuit's nominal high level. Do not derive it from `VCC` alone: probe
loading, ground quality, ringing, edge rate, and board-level noise determine
which threshold produces correct digital decisions. Keep the user-selected
Bridge value authoritative and validate it against a known protocol result.
For the current 3.3 V STM32 SPI fixture, `2.2 V` is verified: after the D4
interrupt rises, the following four-byte SPI read contains the expected
`0x43`. A `1.5 V` capture showed activity but decoded the transaction
incorrectly, so edge counts alone are not an acceptable threshold test.

The desktop client provides common logic-level midpoint values only as starting
references, not correctness claims. It stores the chosen voltage, reference,
and user-confirmed protocol-validation state separately for each PXLogic device.
Changing the voltage or reference clears that validation state. The `2.2 V`
STM32 SPI fixture option remains unverified for a different target until the
user confirms known protocol contents and explicitly marks it as validated.

The one-time FPGA prepare can still produce a hardware-front-end transient. If
the target bus is affected when the Bridge itself starts, stop testing and
verify probe grounding and input impedance with an oscilloscope. Repeated Logic
Start operations must not log `uploading reset bitstream` or
`uploading main bitstream`; those lines are only valid once during Bridge
startup.

## Diagnostics

The desktop client's `导出诊断` action writes a local JSON report containing
the selected settings, Logic fingerprint result, compatibility cache, current
Bridge state, recent runtime logs, and the tail of `graphio.log`. The report is
created only at the path selected by the user and is never uploaded.

Capture helper failures are classified with stable error codes. A rate,
channel-mapping, conversion, helper-start, or helper-exit failure changes the
client to a recovery-required state. `重新初始化 Bridge` first stops the current
Bridge process, waits for it to exit, and only then starts a fresh process that
performs the one-time FPGA prepare again. No automatic hardware reconfiguration
is attempted after a capture failure.

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
