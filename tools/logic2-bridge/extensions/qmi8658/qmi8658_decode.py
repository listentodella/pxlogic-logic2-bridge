from __future__ import annotations

import re
from typing import Any, Dict, List, Optional, Sequence, Tuple

from qst_hla_common import (
    DecodedTransaction,
    I2cAssembler,
    SpiAssembler,
    decode_register_fields,
    field_bounds,
    load_registers,
    signed_i16,
)


WHO_AM_I = 0x00
CTRL1 = 0x02
CTRL2 = 0x03
CTRL3 = 0x04
CTRL7 = 0x08
FIFO_COUNT = 0x15
FIFO_DATA = 0x17
DATA_ALL = 0x33
RESET = 0x60

REGISTERS = load_registers("qmi8658_registers.json")


class Qmi8658Decoder:
    register_mask = 0x7F

    def __init__(self) -> None:
        self.accel_fs_seen: Optional[float] = None
        self.gyro_fs_seen: Optional[float] = None
        self.accel_fs_override: Optional[float] = None
        self.gyro_fs_override: Optional[float] = None
        self.big_endian_seen: Optional[bool] = None
        self.byte_order_override: Optional[str] = None
        self.accel_enabled: Optional[bool] = None
        self.gyro_enabled: Optional[bool] = None
        self.fifo_layout_override: Optional[Tuple[str, ...]] = None

    def set_scale_overrides(
        self, accel_fs_g: Optional[float], gyro_fs_dps: Optional[float]
    ) -> None:
        self.accel_fs_override = accel_fs_g
        self.gyro_fs_override = gyro_fs_dps

    def set_byte_order_override(self, setting: str) -> None:
        if setting == "Little endian":
            self.byte_order_override = "little"
        elif setting == "Big endian":
            self.byte_order_override = "big"
        else:
            self.byte_order_override = None

    def set_fifo_layout_override(self, setting: str) -> None:
        layouts = {
            "Accel XYZ + Gyro XYZ": ("accel", "gyro"),
            "Accel XYZ": ("accel",),
            "Gyro XYZ": ("gyro",),
        }
        self.fifo_layout_override = layouts.get(setting)

    def decode_spi(self, mosi: Sequence[int], miso: Sequence[int]) -> DecodedTransaction:
        mosi_bytes = [value & 0xFF for value in mosi]
        miso_bytes = [value & 0xFF for value in miso]
        if not mosi_bytes:
            return DecodedTransaction(
                bus="SPI",
                operation="UNKNOWN",
                register=None,
                register_name="NO_COMMAND",
                data=[],
                status="No MOSI command byte",
                raw_mosi=mosi_bytes,
                raw_miso=miso_bytes,
            )
        command = mosi_bytes[0]
        operation = "READ" if command & 0x80 else "WRITE"
        register = command & self.register_mask
        data = miso_bytes[1:] if operation == "READ" else mosi_bytes[1:]
        transaction = self.decode_register("SPI", operation, register, data)
        transaction.raw_mosi = mosi_bytes
        transaction.raw_miso = miso_bytes
        if operation == "READ" and len(miso_bytes) <= 1:
            transaction.status = "Read transaction has no MISO payload"
        return transaction

    def decode_i2c(
        self,
        device_address: int,
        operation: str,
        register: Optional[int],
        data: Sequence[int],
    ) -> DecodedTransaction:
        if register is None:
            return DecodedTransaction(
                bus="I2C",
                operation=operation,
                register=None,
                register_name="UNKNOWN_POINTER",
                data=[value & 0xFF for value in data],
                device_address=device_address,
                status="Read without a known register pointer",
            )
        return self.decode_register(
            "I2C", operation, register & self.register_mask, data, device_address
        )

    def decode_register(
        self,
        bus: str,
        operation: str,
        register: int,
        data: Sequence[int],
        device_address: Optional[int] = None,
    ) -> DecodedTransaction:
        values = [value & 0xFF for value in data]
        definition = REGISTERS.get(register, {})
        register_name = definition.get("name", f"REG_0x{register:02X}")
        if register == DATA_ALL and len(values) > 1:
            register_name = "DATA_ALL"
        status = "OK" if definition else "Register is not present in the QMI8658A map"
        access = definition.get("access", "")
        if operation == "WRITE" and access == "RO":
            status = "Write to read-only register"
        elif operation == "READ" and access == "WO":
            status = "Read from write-only register"

        transaction = DecodedTransaction(
            bus=bus,
            operation=operation,
            register=register,
            register_name=register_name,
            data=values,
            device_address=device_address,
            status=status,
        )
        if register not in (DATA_ALL, FIFO_DATA):
            if self.is_event_status_register(register):
                transaction.fields = self._decode_event_status(register, values)
            else:
                transaction.fields = decode_register_fields(REGISTERS, register, values)
        transaction.derived = self._derive_values(register, operation, values)
        if operation in ("READ", "WRITE"):
            self._observe_configuration(register, values)
        return transaction

    def advance_i2c_pointer(self, pointer: int, count: int) -> int:
        if (pointer & self.register_mask) == FIFO_DATA:
            return FIFO_DATA
        return (pointer + count) & self.register_mask

    def is_event_status_register(self, register: int) -> bool:
        roles = set(REGISTERS.get(register, {}).get("roles", []))
        return bool(
            roles
            & {"interrupt_status", "data_ready_status", "activity_status", "tap_status"}
        )

    def _decode_event_status(self, register: int, values: Sequence[int]) -> List[str]:
        active: List[str] = []
        inactive: List[str] = []
        for offset, value in enumerate(values[:4]):
            definition = REGISTERS.get((register + offset) & self.register_mask, {})
            if not definition:
                continue
            for item in definition.get("fields", []):
                name = item["name"]
                if "reserved" in name.lower() or "rsvd" in name.lower():
                    continue
                high, low = field_bounds(item["bits"])
                if high > 7:
                    continue
                field_value = (value >> low) & ((1 << (high - low + 1)) - 1)
                label = item.get("event") or re.sub(r"^(int_|status_)", "", name)
                (active if field_value else inactive).append(label)
        triggered = "TRIGGERED: " + (", ".join(active) if active else "none")
        return [triggered] if not inactive else [triggered, "inactive: " + ", ".join(inactive)]

    def _derive_values(
        self, register: int, operation: str, values: Sequence[int]
    ) -> List[str]:
        if register == WHO_AM_I and operation == "READ" and values:
            if values[0] == 0x05:
                return ["WHO_AM_I matched QMI8658A (0x05)"]
            return [f"Unexpected WHO_AM_I 0x{values[0]:02X}, expected 0x05"]
        if register == RESET and operation == "WRITE" and values:
            return ["Soft reset command" if values[0] == 0xB0 else "Unknown reset value"]
        if register == DATA_ALL and operation == "READ":
            return self._decode_data_all(values)
        if register == FIFO_DATA and operation == "READ":
            return self._decode_fifo(values)
        if register == FIFO_COUNT and operation == "READ" and len(values) >= 2:
            words = values[0] | ((values[1] & 0x03) << 8)
            flags = [
                name
                for mask, name in (
                    (0x80, "full"),
                    (0x40, "watermark"),
                    (0x20, "overflow"),
                    (0x10, "not-empty"),
                )
                if values[1] & mask
            ]
            suffix = "" if not flags else f" ({', '.join(flags)})"
            return [f"FIFO contains {words} word(s), {words * 2} byte(s){suffix}"]
        if register == 0x30 and operation == "READ" and len(values) >= 3:
            timestamp = values[0] | (values[1] << 8) | (values[2] << 16)
            return [f"Timestamp={timestamp} ticks"]
        return []

    def _decode_data_all(self, values: Sequence[int]) -> List[str]:
        if len(values) < 14:
            return [f"DATA_ALL partial block ({len(values)}/14 bytes)"]
        order = self._byte_order()
        raw = {
            "temp": signed_i16(values, 0, order),
            "ax": signed_i16(values, 2, order),
            "ay": signed_i16(values, 4, order),
            "az": signed_i16(values, 6, order),
            "gx": signed_i16(values, 8, order),
            "gy": signed_i16(values, 10, order),
            "gz": signed_i16(values, 12, order),
        }
        accel_fs = self.accel_fs_override or self.accel_fs_seen
        gyro_fs = self.gyro_fs_override or self.gyro_fs_seen
        return [
            f"T={raw['temp'] / 256.0:.3f} C",
            self._scaled_vector("A", raw, ("ax", "ay", "az"), accel_fs, "g"),
            self._scaled_vector("G", raw, ("gx", "gy", "gz"), gyro_fs, "dps"),
            f"byte-order={order}",
        ]

    def _decode_fifo(self, values: Sequence[int]) -> List[str]:
        layout, source = self._fifo_layout(values)
        if not layout:
            if not values:
                return ["FIFO is empty"]
            return [
                f"FIFO {len(values)} byte(s), layout unknown; select FIFO layout or capture CTRL7"
            ]
        frame_size = 6 * len(layout)
        complete = len(values) // frame_size
        remainder = len(values) % frame_size
        summary = f"FIFO {complete} frame(s), layout {source}: " + "+".join(layout)
        if remainder:
            summary += f", trailing {remainder} byte(s)"
        result = [summary]
        if not complete:
            return result
        order = self._byte_order()
        offset = 0
        for sensor in layout:
            raw = {
                f"{sensor[0]}x": signed_i16(values, offset, order),
                f"{sensor[0]}y": signed_i16(values, offset + 2, order),
                f"{sensor[0]}z": signed_i16(values, offset + 4, order),
            }
            if sensor == "accel":
                result.append(
                    self._scaled_vector(
                        "A", raw, ("ax", "ay", "az"),
                        self.accel_fs_override or self.accel_fs_seen, "g"
                    )
                )
            else:
                result.append(
                    self._scaled_vector(
                        "G", raw, ("gx", "gy", "gz"),
                        self.gyro_fs_override or self.gyro_fs_seen, "dps"
                    )
                )
            offset += 6
        return result

    def _fifo_layout(self, values: Sequence[int]) -> Tuple[Tuple[str, ...], str]:
        if self.fifo_layout_override:
            return self.fifo_layout_override, "manual"
        observed = tuple(
            sensor
            for enabled, sensor in (
                (self.accel_enabled, "accel"),
                (self.gyro_enabled, "gyro"),
            )
            if enabled
        )
        if observed:
            return observed, "observed CTRL7"
        if values and len(values) % 12 == 0:
            return ("accel", "gyro"), "inferred 6-axis"
        return (), "unknown"

    def _byte_order(self) -> str:
        if self.byte_order_override:
            return self.byte_order_override
        return "big" if self.big_endian_seen else "little"

    @staticmethod
    def _scaled_vector(
        label: str,
        raw: Dict[str, int],
        axes: Sequence[str],
        full_scale: Optional[float],
        unit: str,
    ) -> str:
        if full_scale is None:
            values = ", ".join(str(raw[axis]) for axis in axes)
            return f"{label}raw=[{values}]"
        values = ", ".join(f"{raw[axis] * full_scale / 32768.0:.4f}" for axis in axes)
        return f"{label}=[{values}] {unit}"

    def _observe_configuration(self, register: int, values: Sequence[int]) -> None:
        for offset, value in enumerate(values):
            address = (register + offset) & self.register_mask
            if address == CTRL1:
                self.big_endian_seen = bool(value & 0x20)
            elif address == CTRL2:
                self.accel_fs_seen = {0: 2.0, 1: 4.0, 2: 8.0, 3: 16.0}.get(
                    (value >> 4) & 0x07
                )
            elif address == CTRL3:
                self.gyro_fs_seen = float(16 << ((value >> 4) & 0x07))
            elif address == CTRL7:
                self.accel_enabled = bool(value & 0x01)
                self.gyro_enabled = bool(value & 0x02)


__all__ = ["I2cAssembler", "Qmi8658Decoder", "SpiAssembler"]
