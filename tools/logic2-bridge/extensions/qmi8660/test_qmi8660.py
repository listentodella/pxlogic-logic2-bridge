import unittest

from qmi8660_decode import I2cAssembler, Qmi8660Decoder, SpiAssembler


class Qmi8660DecoderTests(unittest.TestCase):
    def test_spi_whoami_read(self):
        decoder = Qmi8660Decoder()
        decoded = decoder.decode_spi([0x82, 0x00], [0x00, 0x06])
        self.assertEqual(decoded.operation, "READ")
        self.assertEqual(decoded.register_name, "WHOAMI")
        self.assertEqual(decoded.data, [0x06])
        self.assertIn("matched", decoded.derived[0])

    def test_spi_configuration_and_data_all_scaling(self):
        decoder = Qmi8660Decoder()
        decoder.decode_spi([0x37, 0x02], [0x00, 0x00])
        decoder.decode_spi([0x39, 0x07], [0x00, 0x00])
        payload = [
            0x00, 0x40, 0x00, 0xC0, 0x01, 0x00,
            0x00, 0x20, 0x00, 0xE0, 0x00, 0x40,
            0x00, 0x01,
        ]
        decoded = decoder.decode_spi([0xE0] + [0] * 14, [0] + payload)
        self.assertEqual(decoded.register_name, "DATA_ALL")
        self.assertIn("2048.0000 dps", decoded.derived[0])
        self.assertIn("4.0000 g", decoded.derived[1])
        self.assertIn("1.000 C", decoded.derived[2])

    def test_page_switch_selects_ois_register_map(self):
        decoder = Qmi8660Decoder()
        page = decoder.decode_spi([0x7E, 0xFF, 0x00], [0, 0, 0])
        self.assertEqual(page.page_after, 0x00FF)
        decoded = decoder.decode_spi([0xB1, 0x00], [0x00, 0x08])
        self.assertEqual(decoded.page_name, "OIS")
        self.assertEqual(decoded.register_name, "ACTL_OIS0")

    def test_interrupt_status_prioritizes_active_events_across_burst(self):
        decoder = Qmi8660Decoder()
        decoded = decoder.decode_spi(
            [0xD8, 0, 0, 0, 0],
            [0, 0x40, 0x02, 0x00, 0x00],
        )
        self.assertEqual(
            decoded.fields[0], "TRIGGERED: fifo_watermark, motion_b"
        )
        self.assertTrue(decoded.fields[1].startswith("inactive: fifo_full"))
        self.assertNotIn("rsvd", " ".join(decoded.fields))

    def test_interrupt_status_with_no_active_event_says_none_first(self):
        decoder = Qmi8660Decoder()
        decoded = decoder.decode_spi([0xD9, 0], [0, 0])
        self.assertEqual(decoded.fields[0], "TRIGGERED: none")
        self.assertIn("inactive: orient", decoded.fields[1])

    def test_fifo_configuration_decodes_first_frame(self):
        decoder = Qmi8660Decoder()
        decoder.set_scale_overrides(16, 4096)
        decoder.decode_spi([0x52, 0xFC], [0, 0])
        decoder.decode_spi([0x53, 0x48], [0, 0])
        payload = [0x01, 0] * 7
        decoded = decoder.decode_spi([0xD7] + [0] * len(payload), [0] + payload)
        self.assertIn("1 frame", decoded.derived[0])
        self.assertEqual(decoded.derived[1], "G=[0.1250, 0.1250, 0.1250] dps")
        self.assertEqual(decoded.derived[2], "A=[0.0005, 0.0005, 0.0005] g")
        self.assertEqual(decoded.derived[3], "T=0.004 C")

    def test_fifo_infers_common_14_byte_layout_without_configuration(self):
        decoder = Qmi8660Decoder()
        decoder.set_scale_overrides(16, 4096)
        payload = [
            0x00, 0x40, 0x00, 0xC0, 0x00, 0x20,
            0x00, 0x10, 0x00, 0xF0, 0x00, 0x08,
            0x00, 0x1E,
        ]
        decoded = decoder.decode_spi([0xD7] + [0] * 14, [0] + payload)
        self.assertIn("inferred 6-axis+temp", decoded.derived[0])
        self.assertEqual(decoded.derived[1], "G=[2048.0000, -2048.0000, 1024.0000] dps")
        self.assertEqual(decoded.derived[2], "A=[2.0000, -2.0000, 1.0000] g")
        self.assertEqual(decoded.derived[3], "T=30.000 C")

    def test_configuration_readback_is_enough_to_decode_fifo(self):
        decoder = Qmi8660Decoder()
        decoder.decode_spi([0xB7, 0], [0, 0x01])
        decoder.decode_spi([0xB9, 0], [0, 0x06])
        decoder.decode_spi([0xD2, 0], [0, 0xFC])
        decoder.decode_spi([0xD3, 0], [0, 0x48])
        decoded = decoder.decode_spi([0xD7] + [0] * 14, [0] + [0x00, 0x40] * 7)
        self.assertIn("layout observed", decoded.derived[0])
        self.assertIn("1024.0000", decoded.derived[1])
        self.assertIn("4.0000", decoded.derived[2])

    def test_manual_fifo_layout_resolves_ambiguous_payload_size(self):
        decoder = Qmi8660Decoder()
        decoder.set_scale_overrides(8, 2048)
        decoder.set_fifo_layout_override("Gyro XYZ + Accel XYZ")
        decoded = decoder.decode_spi([0xD7] + [0] * 84, [0] + [0] * 84)
        self.assertIn("7 frame(s)", decoded.derived[0])
        self.assertIn("layout manual", decoded.derived[0])

    def test_rseq_fifo_capture_sample_has_physical_units(self):
        decoder = Qmi8660Decoder()
        decoder.set_scale_overrides(16, 4096)
        decoder.set_fifo_layout_override("Gyro XYZ + Accel XYZ + Temp")
        payload = [
            0xE1, 0xFF, 0xFD, 0xFF, 0x08, 0x00,
            0x41, 0x00, 0x13, 0x00, 0x25, 0x08,
            0xDE, 0x1D,
        ]
        decoded = decoder.decode_spi([0xD7] + [0] * 14, [0] + payload)
        self.assertEqual(decoded.derived[1], "G=[-3.8750, -0.3750, 1.0000] dps")
        self.assertEqual(decoded.derived[2], "A=[0.0317, 0.0093, 1.0181] g")
        self.assertEqual(decoded.derived[3], "T=29.867 C")

    def test_i2c_repeated_start_read(self):
        decoder = Qmi8660Decoder()
        i2c = I2cAssembler(decoder)
        frames = [
            ("start", {}, 0.0, 0.1),
            ("address", {"address": bytes([0x6A]), "read": False, "ack": True}, 0.1, 0.2),
            ("data", {"data": bytes([0x02]), "ack": True}, 0.2, 0.3),
            ("start", {}, 0.3, 0.4),
            ("address", {"address": bytes([0x6A]), "read": True, "ack": True}, 0.4, 0.5),
            ("data", {"data": bytes([0x06]), "ack": False}, 0.5, 0.6),
            ("stop", {}, 0.6, 0.7),
        ]
        emission = None
        for frame in frames:
            emission = i2c.feed(*frame) or emission
        self.assertIsNotNone(emission)
        self.assertEqual(emission.transaction.operation, "READ")
        self.assertEqual(emission.transaction.register_name, "WHOAMI")
        self.assertEqual(emission.transaction.device_address, 0x6A)

    def test_i2c_pointer_survives_stop(self):
        decoder = Qmi8660Decoder()
        i2c = I2cAssembler(decoder)
        for frame in [
            ("start", {}, 0.0, 0.1),
            ("address", {"address": [0x6B], "read": False}, 0.1, 0.2),
            ("data", {"data": [0x54]}, 0.2, 0.3),
            ("stop", {}, 0.3, 0.4),
        ]:
            i2c.feed(*frame)
        emission = None
        for frame in [
            ("start", {}, 1.0, 1.1),
            ("address", {"address": [0x6B], "read": True}, 1.1, 1.2),
            ("data", {"data": [0x34]}, 1.2, 1.3),
            ("data", {"data": [0x10]}, 1.3, 1.4),
            ("stop", {}, 1.4, 1.5),
        ]:
            emission = i2c.feed(*frame) or emission
        self.assertEqual(emission.transaction.register_name, "FIFO_STATUSL")
        self.assertIn("52 bytes", emission.transaction.derived[0])

    def test_i2c_fifo_pointer_does_not_auto_increment(self):
        decoder = Qmi8660Decoder()
        i2c = I2cAssembler(decoder)
        for frame in [
            ("start", {}, 0.0, 0.1),
            ("address", {"address": [0x6A], "read": False}, 0.1, 0.2),
            ("data", {"data": [0x57]}, 0.2, 0.3),
            ("start", {}, 0.3, 0.4),
            ("address", {"address": [0x6A], "read": True}, 0.4, 0.5),
            ("data", {"data": [0x01]}, 0.5, 0.6),
            ("data", {"data": [0x02]}, 0.6, 0.7),
            ("stop", {}, 0.7, 0.8),
        ]:
            i2c.feed(*frame)
        self.assertEqual(i2c.register_pointers[0x6A], 0x57)

    def test_spi_assembler_uses_enable_boundaries(self):
        decoder = Qmi8660Decoder()
        spi = SpiAssembler(decoder)
        spi.feed("enable", {}, 0.0, 0.1)
        spi.feed("result", {"mosi": [0x82], "miso": [0]}, 0.1, 0.2)
        spi.feed("result", {"mosi": [0], "miso": [0x06]}, 0.2, 0.3)
        emission = spi.feed("disable", {}, 0.3, 0.4)
        self.assertEqual(emission.transaction.register_name, "WHOAMI")
        self.assertEqual(emission.transaction.data, [0x06])


if __name__ == "__main__":
    unittest.main()
