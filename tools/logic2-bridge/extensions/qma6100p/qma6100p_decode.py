from __future__ import annotations

from typing import Any, Dict, List, Optional, Sequence

from qst_hla_common import (
    DecodedTransaction,
    I2cAssembler,
    SpiAssembler,
    decode_register_fields,
    load_registers,
    signed_i16,
)


CHIP_ID = 0x00
ACC_DATA = 0x01
RANGE = 0x0F
FIFO_DATA = 0x3F

REGISTERS = load_registers("qma6100p_registers.json")
REGISTERS.setdefault(
    CHIP_ID,
    {
        "name": "CHIP_ID",
        "access": "RO",
        "width": 1,
        "description": "QMA6100P chip identity; known driver encodings include 0x90 and 0x09",
        "roles": ["identity"],
        "fields": [
            {
                "name": "chip_id",
                "bits": "7:0",
                "description": "Observed chip identity value",
                "roles": ["identity_value"],
            }
        ],
    },
)


class Qma6100pDecoder:
    register_mask = 0x7F

    def __init__(self) -> None:
        self.accel_fs_seen: Optional[float] = None
        self.accel_fs_override: Optional[float] = None

    def set_scale_override(self, accel_fs_g: Optional[float]) -> None:
        self.accel_fs_override = accel_fs_g

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
        if register == ACC_DATA and len(values) > 1:
            register_name = "ACC_DATA"
        status = "OK" if definition else "Register is not present in the QMA6100P map"
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
        if register not in (ACC_DATA, FIFO_DATA):
            transaction.fields = decode_register_fields(REGISTERS, register, values)
        transaction.derived = self._derive_values(register, operation, values)
        if operation in ("READ", "WRITE"):
            self._observe_configuration(register, values)
        return transaction

    def advance_i2c_pointer(self, pointer: int, count: int) -> int:
        if (pointer & self.register_mask) == FIFO_DATA:
            return FIFO_DATA
        return (pointer + count) & self.register_mask

    def _derive_values(
        self, register: int, operation: str, values: Sequence[int]
    ) -> List[str]:
        if register == CHIP_ID and operation == "READ" and values:
            if values[0] in (0x90, 0x09):
                return [f"CHIP_ID=0x{values[0]:02X} (known QMA6100P encoding)"]
            return [
                f"CHIP_ID=0x{values[0]:02X}; source revisions disagree, value retained without rejection"
            ]
        if register == ACC_DATA and operation == "READ":
            return self._decode_samples(values, "ACC_DATA")
        if register == FIFO_DATA and operation == "READ":
            return self._decode_samples(values, "FIFO")
        return []

    def _decode_samples(self, values: Sequence[int], source: str) -> List[str]:
        complete = len(values) // 6
        remainder = len(values) % 6
        if complete == 0:
            return [f"{source} partial sample ({len(values)}/6 bytes)"]
        summary = f"{source} {complete} XYZ sample(s)"
        if remainder:
            summary += f", trailing {remainder} byte(s)"
        raw = [signed_i16(values, offset, "little") >> 2 for offset in (0, 2, 4)]
        full_scale = self.accel_fs_override or self.accel_fs_seen
        if full_scale is None:
            vector = f"Araw14=[{raw[0]}, {raw[1]}, {raw[2]}]"
        else:
            lsb_per_g = 8192.0 / full_scale
            vector = "A=[" + ", ".join(f"{axis / lsb_per_g:.4f}" for axis in raw) + "] g"
        return [summary, vector]

    def _observe_configuration(self, register: int, values: Sequence[int]) -> None:
        for offset, value in enumerate(values):
            if ((register + offset) & self.register_mask) != RANGE:
                continue
            self.accel_fs_seen = {
                0x1: 2.0,
                0x2: 4.0,
                0x4: 8.0,
                0x8: 16.0,
                0xF: 32.0,
            }.get(value & 0x0F)


__all__ = ["I2cAssembler", "Qma6100pDecoder", "SpiAssembler"]
