# Logic 2 Bridge delivery contract

This contract applies to every P0/P1 Bridge feature. P2/P3 work remains out of
scope until the active P0/P1 acceptance criteria are complete.

## Definition of Ready

A feature may enter implementation only when its brief records:

- the macOS user problem and expected user-visible outcome;
- priority, scope, and explicit non-goals;
- affected runtime layers and failure/recovery behavior;
- measurable acceptance criteria and the commands that prove them;
- the last pushed checkpoint commit and target branch;
- whether hardware or running-process access is required.

The checkpoint must be pushed before implementation begins. Hardware and
running applications must not be touched unless the brief explicitly requires
that validation and the user has placed them in scope.

## Definition of Done

A feature is complete only when all of the following are true:

1. User-visible behavior and recovery behavior match the brief.
2. Focused regression tests cover the new contract and its failure path.
3. `pnpm run verify:delivery` finishes without `FAIL`.
4. The macOS `Bridge delivery gate` workflow passes for the pushed commit.
5. The delivery report identifies the exact branch and full commit SHA.
6. The final commit is pushed and reachable from the reported remote branch.
7. Remaining `WARN` items and any manual hardware validation are stated
   explicitly; a warning must not be described as a pass.

## Self-check

Run from the repository root:

```sh
pnpm run verify:delivery -- --report /tmp/bridge-delivery-report.json
```

The gate performs these non-hardware checks:

- Bridge JavaScript syntax and Node tests;
- Rust formatting for the PXLogic workspace and Tauri client;
- PXLogic core tests and capture-helper compilation;
- Tauri client tests;
- application manifest version alignment;
- Git branch, upstream, commit, and dirty-path capture.

Required checks use `PASS` or `FAIL`. Version drift is currently reported as
`WARN` so it is visible without hiding functional test results; release work
must resolve that warning before a new public tag.

## Delivery report

Every implementation task reports the following facts in its final handoff:

- priority and accepted user outcome;
- checkpoint SHA;
- final full SHA and remote branch;
- delivery-gate status and report artifact;
- focused test count/results;
- unverified hardware or long-duration behavior;
- rollback commit or procedure.

Statements such as "tests passed" without the exact command and commit are not
accepted as delivery evidence.

## Logic 2 MCP evidence

For the MCP proxy feature, focused automated evidence must cover the loopback
bind/fallback, local Origin enforcement, POST/GET/DELETE and SSE forwarding,
MCP transport headers, activity pairing, real `tools/list` schema retention,
and approval allow/deny/session cleanup. The renderer delivery test also keeps
the independent hidden always-on-top window, main-window entry, generic MCP URL,
and approval controls from disappearing silently.

These checks use a stand-in upstream and are intentionally hardware-free. The
handoff must separately state whether an installed Logic 2 MCP server, PXLogic,
and a supported real Saleae device were exercised. An unperformed manual pass
is an explicit validation boundary, not a failed automated gate. The operating
and manual validation procedure is in
[Logic 2 MCP proxy and activity window](logic2-mcp-proxy.md).
