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
