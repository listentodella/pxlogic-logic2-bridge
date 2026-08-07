# QMA6100P Logic 2 HLA

This local extension decodes QMA6100P traffic from Logic 2's built-in I2C or
SPI analyzer. It supports I2C addresses 0x12/0x13 and the SPI bit-7 read flag.

The HLA converts the six-byte XYZ data block and FIFO stream from signed
little-endian words into the device's 14-bit values (`int16 >> 2`). It tracks
the range register and renders acceleration in g. A manual full-scale setting
is available when device initialization was not captured.

Register 0x00 is shown as CHIP_ID. Driver sources use both 0x90 and a
high-nibble 0x09 identity encoding, so other observed values are reported but
not rejected.

Generate the runtime register database with:

```sh
ruby scripts/generate_registers.rb \
  /Users/leo/work/reg/qma6100p.yaml qma6100p_registers.json
```

The generated extension has no runtime dependency on Ruby or the source YAML.
