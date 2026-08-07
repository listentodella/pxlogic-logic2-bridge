# QMI8660 Logic 2 HLA

This local Logic 2 extension decodes QMI8660 register traffic from either the
built-in I2C analyzer or the built-in SPI analyzer.

## Use

1. Capture and configure Logic 2's I2C or SPI analyzer first. QMI8660 SPI uses
   an 8-bit word, bit 7 as the read flag, and bits 6:0 as the register address.
2. Add a High Level Analyzer and select `QMI8660`.
3. For I2C, leave the address setting at `0x6A or 0x6B` unless the device uses a
   different address. For SPI, include Enable/CS when possible. The gap setting
   is only a fallback when the SPI analyzer has no Enable channel.

The decoder tracks register-page changes, accelerometer and gyroscope ranges,
and FIFO configuration from both writes and readback. FIFO rows put the first
sample's physical values before the raw payload so Logic's bubbles show
`G=[...] dps`, `A=[...] g`, and temperature directly. Common 12-byte 6-axis and
14-byte 6-axis-plus-temperature frames can be inferred when initialization was
not captured; use `FIFO layout` for ambiguous or partial reads. Full-scale
settings can likewise be selected manually when `ACTL1`/`GCTL1` are absent.

Interrupt-status reads put asserted events in a leading `TRIGGERED` section and
append deasserted events as `inactive`. Logic therefore shows useful active
events in narrow bubbles and reveals the inactive detail as the view expands.

The register database is generated from `/Users/leo/work/rseq/qmi8660.yaml` by:

```sh
ruby scripts/generate_registers.rb \
  /Users/leo/work/rseq/qmi8660.yaml qmi8660_registers.json
```

The extension has no runtime dependency on RSEQ, Ruby, or third-party Python
packages.
