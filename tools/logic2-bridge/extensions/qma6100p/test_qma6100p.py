import unittest

from qma6100p_decode import I2cAssembler, Qma6100pDecoder, SpiAssembler


class Qma6100pDecoderTests(unittest.TestCase):
    def test_chip_id_known_encodings(self):
        decoder = Qma6100pDecoder()
        for value in (0x90, 0x09):
            decoded = decoder.decode_spi([0x80, 0], [0, value])
            self.assertEqual(decoded.register_name, "CHIP_ID")
            self.assertIn("known QMA6100P", decoded.derived[0])

    def test_unknown_chip_id_is_reported_without_rejection(self):
        decoder = Qma6100pDecoder()
        decoded = decoder.decode_spi([0x80, 0], [0, 0x91])
        self.assertEqual(decoded.status, "OK")
        self.assertIn("without rejection", decoded.derived[0])

    def test_signed_shift_and_observed_range_scaling(self):
        decoder = Qma6100pDecoder()
        decoder.decode_spi([0x0F, 0x04], [0, 0])  # 8 g, 1024 LSB/g
        payload = [0x00, 0x40, 0x00, 0xC0, 0x00, 0x20]
        decoded = decoder.decode_spi([0x81] + [0] * 6, [0] + payload)
        self.assertEqual(decoded.register_name, "ACC_DATA")
        self.assertEqual(decoded.derived[1], "A=[4.0000, -4.0000, 2.0000] g")

    def test_manual_32g_range(self):
        decoder = Qma6100pDecoder()
        decoder.set_scale_override(32)
        decoded = decoder.decode_spi(
            [0x81] + [0] * 6,
            [0, 0x00, 0x04, 0x00, 0xFC, 0x00, 0x02],
        )
        self.assertEqual(decoded.derived[1], "A=[1.0000, -1.0000, 0.5000] g")

    def test_fifo_decodes_six_byte_frames(self):
        decoder = Qma6100pDecoder()
        decoder.set_scale_override(2)
        payload = [0x00, 0x40, 0x00, 0xC0, 0x00, 0x20] * 2
        decoded = decoder.decode_spi([0xBF] + [0] * 12, [0] + payload)
        self.assertEqual(decoded.derived[0], "FIFO 2 XYZ sample(s)")
        self.assertEqual(decoded.derived[1], "A=[1.0000, -1.0000, 0.5000] g")

    def test_fifo_pointer_does_not_increment(self):
        decoder = Qma6100pDecoder()
        self.assertEqual(decoder.advance_i2c_pointer(0x3F, 18), 0x3F)
        self.assertEqual(decoder.advance_i2c_pointer(0x01, 6), 0x07)

    def test_i2c_repeated_start(self):
        decoder = Qma6100pDecoder()
        i2c = I2cAssembler(decoder, (0x12, 0x13))
        emission = None
        for frame in [
            ("start", {}, 0.0, 0.1),
            ("address", {"address": [0x12], "read": False}, 0.1, 0.2),
            ("data", {"data": [0x00]}, 0.2, 0.3),
            ("start", {}, 0.3, 0.4),
            ("address", {"address": [0x12], "read": True}, 0.4, 0.5),
            ("data", {"data": [0x90], "ack": False}, 0.5, 0.6),
            ("stop", {}, 0.6, 0.7),
        ]:
            emission = i2c.feed(*frame) or emission
        self.assertIsNotNone(emission)
        self.assertEqual(emission.transaction.register_name, "CHIP_ID")
        self.assertEqual(emission.transaction.device_address, 0x12)

    def test_spi_assembler_uses_enable_boundary(self):
        decoder = Qma6100pDecoder()
        spi = SpiAssembler(decoder)
        self.assertIsNone(spi.feed("enable", {}, 0.0, 0.1))
        self.assertIsNone(
            spi.feed("result", {"mosi": [0x80, 0], "miso": [0, 0x90]}, 0.1, 0.2)
        )
        emission = spi.feed("disable", {}, 0.2, 0.3)
        self.assertEqual(emission.transaction.register_name, "CHIP_ID")


if __name__ == "__main__":
    unittest.main()
