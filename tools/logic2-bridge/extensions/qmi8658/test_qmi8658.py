import unittest

from qmi8658_decode import I2cAssembler, Qmi8658Decoder, SpiAssembler


class Qmi8658DecoderTests(unittest.TestCase):
    def test_spi_who_am_i(self):
        decoder = Qmi8658Decoder()
        decoded = decoder.decode_spi([0x80, 0], [0, 0x05])
        self.assertEqual(decoded.operation, "READ")
        self.assertEqual(decoded.register_name, "WHO_AM_I")
        self.assertIn("matched", decoded.derived[0])

    def test_ranges_and_little_endian_data_all(self):
        decoder = Qmi8658Decoder()
        decoder.decode_spi([0x02, 0x00], [0, 0])
        decoder.decode_spi([0x03, 0x20], [0, 0])  # 8 g
        decoder.decode_spi([0x04, 0x60], [0, 0])  # 1024 dps
        payload = [
            0x00, 0x19,
            0x00, 0x40, 0x00, 0xC0, 0x00, 0x20,
            0x00, 0x40, 0x00, 0xC0, 0x00, 0x20,
        ]
        decoded = decoder.decode_spi([0xB3] + [0] * 14, [0] + payload)
        self.assertEqual(decoded.register_name, "DATA_ALL")
        self.assertEqual(decoded.derived[0], "T=25.000 C")
        self.assertEqual(decoded.derived[1], "A=[4.0000, -4.0000, 2.0000] g")
        self.assertEqual(decoded.derived[2], "G=[512.0000, -512.0000, 256.0000] dps")
        self.assertEqual(decoded.derived[3], "byte-order=little")

    def test_ctrl1_big_endian_data(self):
        decoder = Qmi8658Decoder()
        decoder.set_scale_overrides(8, 1024)
        decoder.decode_spi([0x02, 0x20], [0, 0])
        payload = [
            0x19, 0x00,
            0x40, 0x00, 0xC0, 0x00, 0x20, 0x00,
            0x40, 0x00, 0xC0, 0x00, 0x20, 0x00,
        ]
        decoded = decoder.decode_spi([0xB3] + [0] * 14, [0] + payload)
        self.assertEqual(decoded.derived[1], "A=[4.0000, -4.0000, 2.0000] g")
        self.assertEqual(decoded.derived[3], "byte-order=big")

    def test_fifo_order_is_accel_then_gyro(self):
        decoder = Qmi8658Decoder()
        decoder.set_scale_overrides(16, 2048)
        decoder.decode_spi([0x08, 0x03], [0, 0])
        payload = [
            0x00, 0x10, 0x00, 0xF0, 0x00, 0x08,
            0x00, 0x20, 0x00, 0xE0, 0x00, 0x10,
        ]
        decoded = decoder.decode_spi([0x97] + [0] * 12, [0] + payload)
        self.assertEqual(decoded.derived[1], "A=[2.0000, -2.0000, 1.0000] g")
        self.assertEqual(decoded.derived[2], "G=[512.0000, -512.0000, 256.0000] dps")

    def test_fifo_pointer_does_not_increment(self):
        decoder = Qmi8658Decoder()
        self.assertEqual(decoder.advance_i2c_pointer(0x17, 24), 0x17)
        self.assertEqual(decoder.advance_i2c_pointer(0x33, 14), 0x41)

    def test_interrupt_status_prioritizes_active_event(self):
        decoder = Qmi8658Decoder()
        decoded = decoder.decode_spi([0xAD, 0], [0, 0x81])
        self.assertEqual(decoded.fields[0], "TRIGGERED: ctrl9_done, data_ready")
        self.assertIn("inactive: locked", decoded.fields[1])

    def test_i2c_repeated_start(self):
        decoder = Qmi8658Decoder()
        i2c = I2cAssembler(decoder, (0x6A, 0x6B))
        emission = None
        for frame in [
            ("start", {}, 0.0, 0.1),
            ("address", {"address": [0x6A], "read": False}, 0.1, 0.2),
            ("data", {"data": [0x00]}, 0.2, 0.3),
            ("start", {}, 0.3, 0.4),
            ("address", {"address": [0x6A], "read": True}, 0.4, 0.5),
            ("data", {"data": [0x05], "ack": False}, 0.5, 0.6),
            ("stop", {}, 0.6, 0.7),
        ]:
            emission = i2c.feed(*frame) or emission
        self.assertIsNotNone(emission)
        self.assertEqual(emission.transaction.register_name, "WHO_AM_I")
        self.assertEqual(emission.transaction.device_address, 0x6A)

    def test_spi_assembler_uses_enable_boundary(self):
        decoder = Qmi8658Decoder()
        spi = SpiAssembler(decoder)
        self.assertIsNone(spi.feed("enable", {}, 0.0, 0.1))
        self.assertIsNone(
            spi.feed("result", {"mosi": [0x80, 0], "miso": [0, 0x05]}, 0.1, 0.2)
        )
        emission = spi.feed("disable", {}, 0.2, 0.3)
        self.assertEqual(emission.transaction.register_name, "WHO_AM_I")


if __name__ == "__main__":
    unittest.main()
