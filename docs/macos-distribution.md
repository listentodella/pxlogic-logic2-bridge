# macOS Distribution

The Bridge workflow has two explicit macOS signing modes:

- `adhoc` is the default for development and CI checks. It produces an app
  bundle that can be inspected locally, but Gatekeeper may require Finder's
  `Open` action.
- `notarized` is the public-distribution mode. It imports the configured
  Developer ID certificate, signs with `APPLE_SIGNING_IDENTITY` using the
  hardened runtime, submits the app to Apple's notary service, staples the
  ticket, and runs `spctl` and `xcrun stapler validate` checks.

For a manual workflow run, choose `notarized` only after configuring these
repository secrets:

`APPLE_CERTIFICATE` (base64-encoded `.p12`), `APPLE_CERTIFICATE_PASSWORD`,
`APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_TEAM_ID`, and `APPLE_PASSWORD`.

Tag builds use the repository variable `PXLOGIC_MACOS_SIGNING_MODE`; when it is
unset, the workflow remains `adhoc`. The release checker refuses to enter
`notarized` mode when any required secret is missing, so an unsigned or
ad-hoc artifact cannot be silently presented as a public release.
