from saleae.analyzers import AnalyzerFrame, ChoicesSetting, HighLevelAnalyzer, NumberSetting

from qmi8660_decode import I2cAssembler, Qmi8660Decoder, SpiAssembler


class Hla(HighLevelAnalyzer):
    i2c_address = ChoicesSetting(
        label="I2C address",
        choices=("0x6A or 0x6B", "0x6A", "0x6B", "Any"),
    )
    spi_gap_us = NumberSetting(
        label="SPI transaction gap without Enable (us)", min_value=1, max_value=1_000_000
    )
    accel_full_scale = ChoicesSetting(
        label="Accelerometer full scale",
        choices=("Auto", "4 g", "8 g", "16 g", "32 g"),
    )
    gyro_full_scale = ChoicesSetting(
        label="Gyroscope full scale",
        choices=("Auto", "128 dps", "256 dps", "512 dps", "1024 dps", "2048 dps", "4096 dps"),
    )
    fifo_layout = ChoicesSetting(
        label="FIFO layout",
        choices=(
            "Auto",
            "Gyro XYZ + Accel XYZ + Temp",
            "Gyro XYZ + Accel XYZ",
            "Gyro XYZ + Temp",
            "Accel XYZ + Temp",
            "Gyro XYZ",
            "Accel XYZ",
        ),
    )

    result_types = {
        "qmi8660": {
            "format": (
                "{{data.Bus}}{{data.Address}} {{data.Op}} {{data.Register}} "
                "{{data.Hex}} {{data.Detail}} {{data.Status}}"
            )
        },
        "qmi8660_fifo": {
            "format": "{{data.Bus}} FIFO {{data.Detail}}"
        },
        "qmi8660_interrupt": {
            "format": "{{data.Bus}} IRQ {{data.Detail}}"
        }
    }

    def __new__(cls, settings, *args, **kwargs):
        compatible_settings = dict(settings)
        compatible_settings.setdefault("fifo_layout", "Auto")
        return super().__new__(cls, compatible_settings, *args, **kwargs)

    def __init__(self):
        self.decoder = Qmi8660Decoder()
        self.i2c = I2cAssembler(self.decoder)
        self.spi = SpiAssembler(self.decoder)

    def decode(self, frame: AnalyzerFrame):
        frame_type = str(frame.type).lower()
        frame_data = frame.data or {}
        self._apply_settings()

        if frame_type in ("start", "address", "data", "stop"):
            emission = self.i2c.feed(frame_type, frame_data, frame.start_time, frame.end_time)
        elif frame_type in ("enable", "disable", "result", "error"):
            emission = self.spi.feed(frame_type, frame_data, frame.start_time, frame.end_time)
        else:
            return None

        if emission is None:
            return None
        transaction = emission.transaction
        if (
            transaction.page_before == 0
            and transaction.register == 0x57
            and transaction.operation == "READ"
        ):
            result_type = "qmi8660_fifo"
        elif (
            transaction.page_before == 0
            and transaction.register is not None
            and 0x58 <= transaction.register <= 0x5B
            and transaction.operation == "READ"
        ):
            result_type = "qmi8660_interrupt"
        else:
            result_type = "qmi8660"
        return AnalyzerFrame(
            result_type,
            emission.start_time,
            emission.end_time,
            emission.transaction.frame_data(),
        )

    def _apply_settings(self):
        self.i2c.set_address_mode(str(self.i2c_address))
        self.spi.gap_us = float(self.spi_gap_us)
        self.decoder.set_scale_overrides(
            self._scale_value(self.accel_full_scale),
            self._scale_value(self.gyro_full_scale),
        )
        self.decoder.set_fifo_layout_override(str(self.fifo_layout))

    @staticmethod
    def _scale_value(setting):
        text = str(setting)
        if text == "Auto":
            return None
        try:
            return float(text.split()[0])
        except (TypeError, ValueError):
            return None
