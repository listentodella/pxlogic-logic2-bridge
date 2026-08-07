# QMI8658A Logic 2 HLA

This local extension decodes QMI8658A traffic from Logic 2's built-in I2C or
SPI analyzer. It tracks CTRL1/CTRL2/CTRL3/CTRL7 readback and writes to decode
sensor byte order, full-scale ranges, and FIFO layout.

The HLA renders the 14-byte DATA_ALL block as temperature, acceleration in g,
and angular velocity in dps. FIFO reads render 6-byte acceleration and gyro
vectors in the device's acceleration-then-gyro order. Manual range, byte-order,
and FIFO-layout settings cover captures that start after device initialization.

Generate the runtime register database with:

```sh
ruby scripts/generate_registers.rb \
  /Users/leo/work/reg/qmi8658.yaml qmi8658_registers.json
```

The generated extension has no runtime dependency on Ruby or the source YAML.
