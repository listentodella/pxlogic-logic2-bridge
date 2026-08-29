# Logic 2 MCP proxy and activity window

The Tauri launcher runs a local Streamable HTTP proxy in front of Logic 2's own
MCP server. It observes tool activity and adds a human approval gate; it is not
an MCP server implementation and does not contain an agent.

## Architecture and endpoints

The proxy starts with the desktop application, independently of a PXLogic
Bridge capture session. This distinction is intentional:

- a PXLogic session appears to Logic 2 as a Logic Pro 16 and can use Logic 2 MCP;
- a real Saleae Logic 8, Logic Pro 8, or Logic Pro 16 can use the same proxy and
  activity window without starting the Bridge;
- stopping or restarting the Bridge does not stop the MCP proxy.

Logic 2's default MCP endpoint is `http://127.0.0.1:10530`. The launcher tries
to bind `http://127.0.0.1:10531`. If 10531 is occupied it binds a free loopback
port and displays a prominent warning; clients must use the actual URL shown in
the MCP window.

Enable **MCP Server** in Logic 2 under **Settings > Automation**, then configure
any MCP client to use the displayed URL with the **Streamable HTTP** transport.
The window deliberately shows a generic URL rather than instructions for one
specific agent product.

## Transport and security contract

The proxy forwards Streamable HTTP `POST`, `GET`, and `DELETE` requests. JSON
responses and long-lived SSE responses remain streaming; inspection never
collects an SSE body or changes its frames. End-to-end headers are forwarded,
including `Mcp-Session-Id`, `MCP-Protocol-Version`, `Last-Event-ID`, and future
extension headers. HTTP/1 hop-by-hop headers are removed.

There is no authentication on the proxy port, so both safeguards below are
mandatory:

1. it binds only `127.0.0.1`, never a LAN interface;
2. requests with an `Origin` are accepted only for `localhost`, `127.0.0.1`, or
   `::1`. Missing `Origin` is accepted for command-line MCP clients; foreign and
   `null` origins are rejected to prevent browser/DNS-rebinding access.

## Activity window

Open **MCP 活动** from the main window. The independent, always-on-top window
shows:

- the actual proxy URL, fallback-port warning, and Logic 2 reachability;
- the tools returned by the real `tools/list` response, retaining each complete
  schema rather than relying on a compiled-in catalogue;
- a bounded request/response activity feed paired by MCP session and JSON-RPC
  id;
- pending approvals and their complete arguments.

The first agent request can reveal the window automatically, without focusing
it. Hiding the window does not stop the proxy. Its position and auto-show choice
are stored independently of the PXLogic status panel.

## Approval policy

Inspection and known configuration operations proceed automatically. Current
known read/export/save tools and analyzer/HLA add/remove tools are in this
allow-list. Calls that start, load, stop, or close a capture are held because
they can replace, truncate, or discard current data. Every unknown tool is held
by default; a newly introduced Logic 2 tool cannot become trusted merely because
it appeared in `tools/list`.

A held call has 30 seconds for a decision. **允许** forwards the unchanged call;
**拒绝** and timeout return JSON-RPC error `-32000` with the original request id,
so the agent receives a terminal response rather than hanging. When a session id
exists, **本次 MCP 会话内该工具免问** remembers an allowed tool only until that
session is deleted. `DELETE` clears both remembered decisions and pending calls.
Approvals always reveal the window without stealing focus.

## Validation boundaries

Automated tests use a local stand-in server to verify JSON, SSE, transport
headers, `DELETE`, refusal, unreachable-upstream recovery, Origin rejection,
activity pairing, schemas, and approval state. They do not prove behavior of a
particular installed Logic 2 build or physical analyzer. Before release, perform
one manual pass with Logic 2 MCP enabled:

1. connect a generic MCP client to the URL shown by the window;
2. request `tools/list` and confirm the real catalogue appears;
3. run one read-only tool and confirm it proceeds;
4. request a capture lifecycle tool and exercise allow, deny, timeout, and
   session-only allow;
5. repeat once with PXLogic and once with a supported real Saleae device when
   both hardware paths are release requirements.

## Timing markers, which Logic 2's MCP does not offer

Logic 2 exposes exactly fifteen MCP tools, and none of them writes anything down
on a capture. Its `defineTool` handlers run inside the renderer and read
`rapidDataStore` directly, so the surface is not a protocol boundary that could
be extended from outside -- it is whichever calls Saleae chose to wrap. Timing
markers sit on that same store with no tool in front of them.

That leaves an agent able to capture and decode but unable to record where it
found something. These four tools close the gap:

| Tool | What it does |
|------|--------------|
| `add_timing_marker` | Adds a marker at `timeSec`, optionally with `note`, `label`, `color` |
| `list_timing_markers` | Lists markers in time order with ids, labels, notes |
| `set_timing_marker_note` | Sets or clears the note on one marker |
| `remove_timing_marker` | Removes one marker by id |

They appear in `tools/list` alongside Logic 2's own, and an agent cannot tell the
two apart. The proxy rewrites only that one response, and only when it is JSON --
an SSE tool list is streamed untouched, because collecting a stream to edit it is
exactly what this proxy must never do. A name Logic 2 already serves always wins,
so a future official marker tool would shadow ours rather than the reverse.

`timeSec` counts from the start of the capture, and a capture has to exist first.
Both facts are stated in the tool descriptions, since neither is discoverable by
trying: without them an agent guesses at wall-clock time and reads an empty
result as "no markers" rather than "no capture".

### How it reaches the renderer

The marker tools are served by the Tauri client, which forwards them to the
Bridge session over the same stdin/stderr channel the comparator threshold uses.
The session holds the Chrome DevTools Protocol connection and evaluates the store
calls. Rust has no WebSocket client among Tauri's locked dependencies while Node
has `crypto` and the frame codec already written for the GraphServer proxy, so
the CDP client lives on the Node side and no new dependency was added to either.

```
agent -> proxy :10531 -+-> Logic 2 MCP :10530        (its fifteen tools, forwarded)
                       |
                       +-> Bridge session -> CDP -> renderer   (the four marker tools)
```

Each request carries an id and is answered exactly once. A session that dies with
requests outstanding fails them immediately rather than leaving the agent to wait
out its own patience, and a Bridge that does not answer within ten seconds is
reported as such.

### No inspector is ever shown

The debugging port is a transport, nothing more. Chromium displays nothing for
the port being open: no window, no banner, no badge. Keeping it that way is a
set of things this deliberately does not do, each asserted in the delivery
contract:

- `--auto-open-devtools-for-tabs` is never passed;
- `Page.inspect`, the one CDP method that raises Logic 2's own inspector, is
  never sent;
- `--enable-automation`, which is what produces the "controlled by automated
  software" infobar, is never passed;
- no CDP domain is enabled, so being connected costs the renderer nothing.

The port is taken from the OS rather than fixed, so two Logic instances cannot
collide over it, and it binds loopback only.

### What this depends on, and what breaks it

`rapidDataStore.activeSession.markers.addMarker` is Logic 2's internal shape, not
a published API. A Logic 2 release that moves it will break these four tools and
nothing else -- the fifteen forwarded tools are untouched by this. Every failure
says which step failed, so a moved store reports "this Logic 2 version may have
moved it" rather than returning an empty list.

Two consequences worth stating plainly:

- **Only a Logic 2 the Bridge launched has the channel.** Someone running a real
  Saleae device who starts Logic 2 themselves gets the proxy, the activity stream
  and the gate, but not the marker tools. The tools are still listed and say the
  channel is unavailable, because a tool that appears only sometimes teaches an
  agent nothing.
- **A marker needs an active capture.** With no session the tools report that,
  rather than failing in a way that looks like a broken endpoint.

Markers are not gated. Annotating a capture cannot lose sample data, and being
asked to confirm every note an agent writes down would make the feature not worth
having. They are named in the policy on purpose: the unknown-tool branch gates by
default, so silence there would have meant a dialog per note.

### Marker validation boundaries

The CDP transport is verified against a local stand-in for Chromium's debugging
port: handshake, target selection, frames split across reads, evaluation
round-trip, an exception inside the page, a protocol error, an unanswered
request, a port with no renderer target, and a closed port. The marker layer is
verified with a stubbed renderer, including that a note carrying quotes and a
comment arrives as data rather than executing.

**Verified against a running Logic 2 2.4.46**, PXLogic on `usb:16c0:05dc`, after a
capture had finished:

- the debugging port answered and exposed exactly one page target, whose title was
  the ordinary capture window (`Logic 2 [Logic Pro 16 - Demo] [Session 0]`);
- no DevTools window appeared and no `devtools://` target existed;
- `add` / `list` / `set note` / `remove` completed in order, with the marker count
  returning to its starting value;
- a note carrying `"` and `\` round-tripped intact, as did Chinese text;
- `tools/list` through the proxy returned 19 tools: Logic 2's 15 plus these 4.

Two assumptions were wrong and were corrected by that run, both worth recording
because they are exactly the kind of thing a Logic 2 upgrade can change again:

- **`window.__saleaeTest` does not exist in the shipped build.** It is in the
  bundle, and would have been the steadier route, but it is not installed. The
  fiber walk is the only way in.
- **The React root key is `__reactContainere$<hash>`**, React 16's spelling, not
  the `__reactContainer$` / `__reactFiber$` of later versions. The store is a
  context value, so it is found on a provider's `memoizedProps.value` rather than
  on any component instance -- about sixty fibers from the root.

**Still not verified: the tool call path through a Bridge session the Tauri client
started.** The renderer half was driven directly, and the proxy half was driven
over HTTP, but the client has no command-line entry point, so starting a session
the way a user does needs the window. Against a session the client does not own,
the tools correctly report `Bridge 未在运行` as a tool error rather than hanging.
Before release, start the Bridge from the window and confirm one marker call
completes through the full chain.
