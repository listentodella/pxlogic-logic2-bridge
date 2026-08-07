from saleae.analyzers import AnalyzerFrame, ChoicesSetting, HighLevelAnalyzer, NumberSetting

from qma6100p_decode import I2cAssembler, Qma6100pDecoder, SpiAssembler


class Hla(HighLevelAnalyzer):
    i2c_address = ChoicesSetting(
        label="I2C address",
        choices=("0x12 or 0x13", "0x12", "0x13", "Any"),
    )
    spi_gap_us = NumberSetting(
        label="SPI transaction gap without Enable (us)", min_value=1, max_value=1_000_000
    )
    accel_full_scale = ChoicesSetting(
        label="Accelerometer full scale",
        choices=("Auto", "2 g", "4 g", "8 g", "16 g", "32 g"),
    )

    result_types = {
        "qma6100p": {
            "format": (
                "{{data.Bus}}{{data.Address}} {{data.Op}} {{data.Register}} "
                "{{data.Hex}} {{data.Detail}} {{data.Status}}"
            )
        },
        "qma6100p_fifo": {"format": "{{data.Bus}} FIFO {{data.Detail}}"},
        "qma6100p_accel": {"format": "{{data.Bus}} ACC {{data.Detail}}"},
    }

    def __init__(self):
        self.decoder = Qma6100pDecoder()
        self.i2c = I2cAssembler(self.decoder, (0x12, 0x13))
        self.spi = SpiAssembler(self.decoder)

    def decode(self, frame: AnalyzerFrame):
        frame_type = str(frame.type).lower()
        self._apply_settings()
        if frame_type in ("start", "address", "data", "stop"):
            emission = self.i2c.feed(
                frame_type, frame.data or {}, frame.start_time, frame.end_time
            )
        elif frame_type in ("enable", "disable", "result", "error"):
            emission = self.spi.feed(
                frame_type, frame.data or {}, frame.start_time, frame.end_time
            )
        else:
            return None
        if emission is None:
            return None
        transaction = emission.transaction
        if transaction.operation == "READ" and transaction.register == 0x3F:
            result_type = "qma6100p_fifo"
        elif transaction.operation == "READ" and transaction.register == 0x01:
            result_type = "qma6100p_accel"
        else:
            result_type = "qma6100p"
        return AnalyzerFrame(
            result_type,
            emission.start_time,
            emission.end_time,
            transaction.frame_data(),
        )

    def _apply_settings(self):
        self.i2c.set_address_mode(str(self.i2c_address))
        self.spi.gap_us = float(self.spi_gap_us)
        self.decoder.set_scale_override(self._scale_value(self.accel_full_scale))

    @staticmethod
    def _scale_value(setting):
        text = str(setting)
        if text == "Auto":
            return None
        try:
            return float(text.split()[0])
        except (TypeError, ValueError):
            return None
