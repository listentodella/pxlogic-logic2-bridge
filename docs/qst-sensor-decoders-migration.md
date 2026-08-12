# QST Sensor Decoders Migration

The QST sensor High Level Analyzers now have an independent public repository:

<https://github.com/listentodella/qst-sensor-decoders>

It publishes one Saleae extension package named `QST Sensor Decoders` with
three independent entries in the Logic 2 analyzer menu:

- `QMI8660`
- `QMI8658A`
- `QMA6100P`

The Bridge no longer keeps a bundled copy, installs the analyzers, or updates
Logic 2's extension configuration. New analyzer fixes and releases belong only
in the independent repository. Previously published Bridge versions are not
changed by this source migration.

The independent package is intentionally one repository/package rather than
three repositories: it shares the I2C/SPI transaction assembly and display
model, while each chip remains a separate HLA class and a separate selectable
entry point. Users therefore choose the exact chip in Logic 2 without a model
setting that could silently decode a capture with the wrong register map.
