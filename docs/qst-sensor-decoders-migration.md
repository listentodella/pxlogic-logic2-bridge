# QST Sensor Decoders Migration

The QST sensor High Level Analyzers now have an independent public repository:

<https://github.com/listentodella/qst-sensor-decoders>

It publishes one Saleae extension package named `QST Sensor Decoders` with
three independent entries in the Logic 2 analyzer menu:

- `QMI8660`
- `QMI8658A`
- `QMA6100P`

The Bridge repository keeps its existing copies under
`tools/logic2-bridge/extensions/` for compatibility with already-published
Bridge bundles. New analyzer fixes and releases should be made in the
independent repository first. The copies in this repository should only be
updated when a Bridge release intentionally needs to vendor a newer analyzer
revision.

The independent package is intentionally one repository/package rather than
three repositories: it shares the I2C/SPI transaction assembly and display
model, while each chip remains a separate HLA class and a separate selectable
entry point. Users therefore choose the exact chip in Logic 2 without a model
setting that could silently decode a capture with the wrong register map.
