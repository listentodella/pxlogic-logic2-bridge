from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence, Tuple


UI_PAGE = 0x0000
OIS_PAGE = 0x00FF
PAGE_LOW = 0x7E
PAGE_HIGH = 0x7F
FIFO_DATA = 0x57
DATA_ALL_UI = 0x60
DATA_ALL_OIS = 0x52


def _load_registers() -> Dict[int, Dict[int, Dict[str, Any]]]:
    path = Path(__file__).with_name("qmi8660_registers.json")
    raw = json.loads(path.read_text(encoding="utf-8"))
    return {
        int(page): {int(address): register for address, register in registers.items()}
        for page, registers in raw["pages"].items()
    }


REGISTERS = _load_registers()


def _hex_bytes(data: Sequence[int]) -> str:
    return " ".join(f"{value & 0xFF:02X}" for value in data)


def _signed_i16_le(data: Sequence[int], offset: int) -> int:
    value = (data[offset] & 0xFF) | ((data[offset + 1] & 0xFF) << 8)
    return value - 0x10000 if value & 0x8000 else value


def _field_bounds(bits: str) -> Tuple[int, int]:
    if ":" in bits:
        high, low = bits.split(":", 1)
        return int(high), int(low)
    bit = int(bits)
    return bit, bit


def _enum_label(description: str, value: int, width: int) -> Optional[str]:
    for line in description.splitlines():
        match = re.match(r"\s*([0-9]+)\s*=\s*(.+?)\s*$", line)
        if not match:
            continue
        token, label = match.groups()
        if len(token) == width and set(token) <= {"0", "1"}:
            candidate = int(token, 2)
        elif len(token) > 1 and token.startswith("0") and set(token) <= {"0", "1"}:
            candidate = int(token, 2)
        else:
            candidate = int(token, 10)
        if candidate == value:
            return label
    return None


@dataclass
class DecodedTransaction:
    bus: str
    operation: str
    page_before: int
    page_after: int
    register: Optional[int]
    register_name: str
    data: List[int]
    device_address: Optional[int] = None
    status: str = "OK"
    fields: List[str] = field(default_factory=list)
    derived: List[str] = field(default_factory=list)
    raw_mosi: List[int] = field(default_factory=list)
    raw_miso: List[int] = field(default_factory=list)

    @property
    def page_name(self) -> str:
        if self.page_before == UI_PAGE:
            return "UI"
        if self.page_before == OIS_PAGE:
            return "OIS"
        return f"PAGE_0x{self.page_before:04X}"

    def frame_data(self) -> Dict[str, str]:
        address = "" if self.device_address is None else f" @0x{self.device_address:02X}"
        register = (
            self.register_name
            if self.register is None
            else f"{self.page_name}.{self.register_name} (0x{self.register:02X})"
        )
        details = self.derived + self.fields
        return {
            "Bus": self.bus,
            "Op": self.operation,
            "Address": address,
            "Register": register,
            "Hex": _hex_bytes(self.data),
            "Detail": "; ".join(details[:8]),
            "Status": self.status,
            "Page": self.page_name,
            "PageAfter": f"0x{self.page_after:04X}",
        }


class Qmi8660Decoder:
    def __init__(self) -> None:
        self.page = UI_PAGE
        self.page_low = 0
        self.page_high = 0
        self.accel_fs_seen: Optional[float] = None
        self.gyro_fs_seen: Optional[float] = None
        self.accel_fs_override: Optional[float] = None
        self.gyro_fs_override: Optional[float] = None
        self.fifo_axes: List[str] = []
        self.fifo_temperature = False
        self.fifo_layout_override: Optional[Tuple[List[str], bool]] = None

    def set_scale_overrides(
        self, accel_fs_g: Optional[float], gyro_fs_dps: Optional[float]
    ) -> None:
        self.accel_fs_override = accel_fs_g
        self.gyro_fs_override = gyro_fs_dps

    def set_fifo_layout_override(self, layout: Optional[str]) -> None:
        layouts = {
            "Gyro XYZ + Accel XYZ + Temp": (["gx", "gy", "gz", "ax", "ay", "az"], True),
            "Gyro XYZ + Accel XYZ": (["gx", "gy", "gz", "ax", "ay", "az"], False),
            "Gyro XYZ + Temp": (["gx", "gy", "gz"], True),
            "Accel XYZ + Temp": (["ax", "ay", "az"], True),
            "Gyro XYZ": (["gx", "gy", "gz"], False),
            "Accel XYZ": (["ax", "ay", "az"], False),
        }
        selected = layouts.get(layout or "")
        self.fifo_layout_override = (
            (list(selected[0]), selected[1]) if selected is not None else None
        )

    def decode_spi(self, mosi: Sequence[int], miso: Sequence[int]) -> DecodedTransaction:
        mosi_bytes = [value & 0xFF for value in mosi]
        miso_bytes = [value & 0xFF for value in miso]
        if not mosi_bytes:
            return DecodedTransaction(
                bus="SPI",
                operation="UNKNOWN",
                page_before=self.page,
                page_after=self.page,
                register=None,
                register_name="NO_COMMAND",
                data=[],
                status="No MOSI command byte",
                raw_mosi=mosi_bytes,
                raw_miso=miso_bytes,
            )
        command = mosi_bytes[0]
        operation = "READ" if command & 0x80 else "WRITE"
        register = command & 0x7F
        data = miso_bytes[1:] if operation == "READ" else mosi_bytes[1:]
        transaction = self.decode_register(
            bus="SPI",
            operation=operation,
            register=register,
            data=data,
        )
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
                page_before=self.page,
                page_after=self.page,
                register=None,
                register_name="UNKNOWN_POINTER",
                data=[value & 0xFF for value in data],
                device_address=device_address,
                status="Read without a known register pointer",
            )
        return self.decode_register(
            bus="I2C",
            operation=operation,
            register=register & 0x7F,
            data=data,
            device_address=device_address,
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
        page_before = self.page
        definition = self._register_definition(page_before, register)
        register_name = definition.get("name", f"REG_0x{register:02X}")
        status = "OK" if definition else "Register is not present in the QMI8660 map"
        access = definition.get("access", "")
        if operation == "WRITE" and access == "RO":
            status = "Write to read-only register"
        elif operation == "READ" and access == "WO":
            status = "Read from write-only register"

        transaction = DecodedTransaction(
            bus=bus,
            operation=operation,
            page_before=page_before,
            page_after=page_before,
            register=register,
            register_name=register_name,
            data=values,
            device_address=device_address,
            status=status,
        )
        transaction.fields = self._decode_register_fields(page_before, register, values)
        transaction.derived = self._derive_values(page_before, register, operation, values)

        if operation in ("READ", "WRITE"):
            self._observe_configuration(page_before, register, values)
        if operation == "WRITE":
            self._apply_page_write(register, values)
            transaction.page_after = self.page
        return transaction

    def _register_definition(self, page: int, address: int) -> Dict[str, Any]:
        return REGISTERS.get(page, {}).get(address, {})

    def _decode_register_fields(
        self, page: int, register: int, values: Sequence[int]
    ) -> List[str]:
        if not values:
            return []
        if self._is_data_block(page, register) or (page == UI_PAGE and register == FIFO_DATA):
            return []
        if self._is_interrupt_status_register(page, register):
            return self._decode_interrupt_status_fields(page, register, values)
        decoded: List[str] = []
        for offset, value in enumerate(values[:16]):
            address = (register + offset) & 0x7F
            definition = self._register_definition(page, address)
            if not definition:
                continue
            fields = definition.get("fields", [])
            parts = []
            for item in fields:
                high, low = _field_bounds(item["bits"])
                if high > 7:
                    continue
                width = high - low + 1
                field_value = (value >> low) & ((1 << width) - 1)
                label = _enum_label(item.get("description", ""), field_value, width)
                rendered = f"{item['name']}={field_value}"
                if label:
                    rendered += f" ({label})"
                parts.append(rendered)
            if parts:
                decoded.append(f"{definition['name']}: " + ", ".join(parts))
        return decoded

    def _decode_interrupt_status_fields(
        self, page: int, register: int, values: Sequence[int]
    ) -> List[str]:
        active: List[str] = []
        inactive: List[str] = []
        for offset, value in enumerate(values[:4]):
            address = (register + offset) & 0x7F
            definition = self._register_definition(page, address)
            if "interrupt_status" not in definition.get("roles", []):
                continue
            for item in definition.get("fields", []):
                name = item["name"]
                if "rsvd" in name.lower() or "reserved" in name.lower():
                    continue
                high, low = _field_bounds(item["bits"])
                if high > 7:
                    continue
                field_value = (value >> low) & ((1 << (high - low + 1)) - 1)
                label = item.get("event") or re.sub(r"^int_", "", name)
                (active if field_value else inactive).append(label)

        triggered = "TRIGGERED: " + (", ".join(active) if active else "none")
        if not inactive:
            return [triggered]
        return [triggered, "inactive: " + ", ".join(inactive)]

    def _derive_values(
        self, page: int, register: int, operation: str, values: Sequence[int]
    ) -> List[str]:
        if page == UI_PAGE and register == 0x02 and operation == "READ" and values:
            if values[0] == 0x06:
                return ["WHO_AM_I matched QMI8660 revision 1"]
            return [f"Unexpected WHO_AM_I 0x{values[0]:02X}, expected 0x06"]
        if page == UI_PAGE and register == 0x7B and operation == "WRITE" and values:
            return ["Soft reset command" if values[0] == 0x98 else "Unknown reset key"]
        if self._is_data_block(page, register):
            return self._decode_data_all(values)
        if page == UI_PAGE and register == FIFO_DATA and operation == "READ":
            return self._decode_fifo(values)
        if page == UI_PAGE and register == 0x54 and operation == "READ" and len(values) >= 2:
            count = values[0] | ((values[1] & 0x0F) << 8)
            flags = []
            for mask, name in ((0x80, "full"), (0x40, "watermark"), (0x20, "triggered"), (0x10, "not-empty")):
                if values[1] & mask:
                    flags.append(name)
            suffix = "" if not flags else f" ({', '.join(flags)})"
            return [f"FIFO contains {count} bytes{suffix}"]
        if operation == "WRITE" and register in (PAGE_LOW, PAGE_HIGH):
            low = values[0] if register == PAGE_LOW and values else self.page_low
            high = (
                values[1]
                if register == PAGE_LOW and len(values) > 1
                else values[0]
                if register == PAGE_HIGH and values
                else self.page_high
            )
            return [f"Select register page 0x{(low | (high << 8)):04X}"]
        return []

    def _decode_data_all(self, values: Sequence[int]) -> List[str]:
        if len(values) < 14:
            return [f"DATA_ALL partial block ({len(values)}/14 bytes)"]
        names = ("gx", "gy", "gz", "ax", "ay", "az", "temp")
        raw = {name: _signed_i16_le(values, index * 2) for index, name in enumerate(names)}
        gyro_fs = self.gyro_fs_override or self.gyro_fs_seen
        accel_fs = self.accel_fs_override or self.accel_fs_seen
        gyro = ", ".join(
            self._scaled_axis(name, raw[name], gyro_fs, "dps") for name in names[:3]
        )
        accel = ", ".join(
            self._scaled_axis(name, raw[name], accel_fs, "g") for name in names[3:6]
        )
        return [gyro, accel, f"temp={raw['temp']} ({raw['temp'] / 256.0:.3f} C raw scale)"]

    @staticmethod
    def _scaled_axis(name: str, raw: int, full_scale: Optional[float], unit: str) -> str:
        if full_scale is None:
            return f"{name}={raw}"
        return f"{name}={raw} ({raw * full_scale / 32768.0:.4f} {unit})"

    def _decode_fifo(self, values: Sequence[int]) -> List[str]:
        fields, layout_source = self._fifo_fields(values)
        if not fields:
            return [
                f"FIFO raw payload ({len(values)} bytes); select FIFO layout or capture FIFO_CTL0/1"
            ]
        frame_size = len(fields) * 2
        if frame_size == 0 or len(values) < frame_size:
            return [f"FIFO partial frame ({len(values)}/{frame_size} bytes)"]
        frame_count = len(values) // frame_size
        first_raw: Dict[str, int] = {}
        for index, name in enumerate(fields):
            first_raw[name] = _signed_i16_le(values, index * 2)
        decoded = [
            f"{frame_count} frame(s), {frame_size} B/frame, layout {layout_source}"
        ]
        gyro = [first_raw[name] for name in ("gx", "gy", "gz") if name in first_raw]
        accel = [first_raw[name] for name in ("ax", "ay", "az") if name in first_raw]
        gyro_fs = self.gyro_fs_override or self.gyro_fs_seen
        accel_fs = self.accel_fs_override or self.accel_fs_seen
        sample_prefix = "First " if frame_count > 1 else ""
        if gyro:
            decoded.append(
                self._physical_vector(
                    f"{sample_prefix}G", gyro, gyro_fs, "dps", "gyroscope"
                )
            )
        if accel:
            decoded.append(
                self._physical_vector(
                    f"{sample_prefix}A", accel, accel_fs, "g", "accelerometer"
                )
            )
        if "temp" in first_raw:
            decoded.append(f"{sample_prefix}T={first_raw['temp'] / 256.0:.3f} C")
        trailing = len(values) % frame_size
        if trailing:
            decoded.append(f"{trailing} trailing byte(s)")
        return decoded

    def _fifo_fields(self, values: Sequence[int]) -> Tuple[List[str], str]:
        if self.fifo_layout_override is not None:
            axes, temperature = self.fifo_layout_override
            return axes + (["temp"] if temperature else []), "manual"
        if self.fifo_axes or self.fifo_temperature:
            return self.fifo_axes + (["temp"] if self.fifo_temperature else []), "observed"

        # The two common QMI8660 FIFO frames are unambiguous for normal short
        # watermark reads. Large payloads divisible by both sizes stay raw so
        # that the decoder never invents an axis layout.
        byte_count = len(values)
        fits_14 = byte_count >= 14 and byte_count % 14 == 0
        fits_12 = byte_count >= 12 and byte_count % 12 == 0
        if fits_14 and not fits_12:
            return ["gx", "gy", "gz", "ax", "ay", "az", "temp"], "inferred 6-axis+temp"
        if fits_12 and not fits_14:
            return ["gx", "gy", "gz", "ax", "ay", "az"], "inferred 6-axis"
        return [], "unknown"

    @staticmethod
    def _physical_vector(
        label: str,
        raw_values: Sequence[int],
        full_scale: Optional[float],
        unit: str,
        setting_name: str,
    ) -> str:
        if full_scale is None:
            raw = ", ".join(str(value) for value in raw_values)
            return f"{label}_raw=[{raw}] (set {setting_name} full scale)"
        physical = ", ".join(
            f"{value * full_scale / 32768.0:.4f}" for value in raw_values
        )
        return f"{label}=[{physical}] {unit}"

    def _observe_configuration(self, page: int, register: int, values: Sequence[int]) -> None:
        if page != UI_PAGE:
            return
        for offset, value in enumerate(values):
            address = (register + offset) & 0x7F
            if address == 0x37:
                self.accel_fs_seen = (4.0, 8.0, 16.0, 32.0)[value & 0x03]
            elif address == 0x39:
                self.gyro_fs_seen = {
                    2: 128.0,
                    3: 256.0,
                    4: 512.0,
                    5: 1024.0,
                    6: 2048.0,
                    7: 4096.0,
                }.get(value & 0x07)
            elif address == 0x52:
                self.fifo_axes = [
                    name
                    for mask, name in (
                        (0x20, "gx"),
                        (0x40, "gy"),
                        (0x80, "gz"),
                        (0x04, "ax"),
                        (0x08, "ay"),
                        (0x10, "az"),
                    )
                    if value & mask
                ]
            elif address == 0x53:
                self.fifo_temperature = bool(value & 0x08)

    def _apply_page_write(self, register: int, values: Sequence[int]) -> None:
        if register == PAGE_LOW and values:
            self.page_low = values[0]
            if len(values) > 1:
                self.page_high = values[1]
        elif register == PAGE_HIGH and values:
            self.page_high = values[0]
        else:
            return
        self.page = self.page_low | (self.page_high << 8)

    @staticmethod
    def _is_data_block(page: int, register: int) -> bool:
        return (page == UI_PAGE and register == DATA_ALL_UI) or (
            page == OIS_PAGE and register == DATA_ALL_OIS
        )

    def _is_interrupt_status_register(self, page: int, register: int) -> bool:
        definition = self._register_definition(page, register)
        return "interrupt_status" in definition.get("roles", [])


@dataclass
class Emission:
    transaction: DecodedTransaction
    start_time: Any
    end_time: Any


@dataclass
class _I2cSegment:
    address: int
    read: bool
    data: List[int] = field(default_factory=list)
    address_nak: bool = False
    data_naks: List[int] = field(default_factory=list)


class I2cAssembler:
    def __init__(self, decoder: Qmi8660Decoder) -> None:
        self.decoder = decoder
        self.accepted_addresses: Optional[set[int]] = {0x6A, 0x6B}
        self.register_pointers: Dict[int, int] = {}
        self._reset()

    def _reset(self) -> None:
        self.active = False
        self.start_time: Any = None
        self.end_time: Any = None
        self.segments: List[_I2cSegment] = []
        self.current: Optional[_I2cSegment] = None

    def set_address_mode(self, mode: str) -> None:
        if mode == "0x6A":
            self.accepted_addresses = {0x6A}
        elif mode == "0x6B":
            self.accepted_addresses = {0x6B}
        elif mode == "Any":
            self.accepted_addresses = None
        else:
            self.accepted_addresses = {0x6A, 0x6B}

    def feed(self, frame_type: str, data: Dict[str, Any], start_time: Any, end_time: Any) -> Optional[Emission]:
        kind = frame_type.lower()
        if kind == "start":
            if not self.active:
                self.active = True
                self.start_time = start_time
            self.end_time = end_time
            self.current = None
            return None
        if kind == "address":
            if not self.active:
                self.active = True
                self.start_time = start_time
            address = _first_byte(data.get("address"))
            if address is None:
                return None
            segment = _I2cSegment(address=address & 0x7F, read=bool(data.get("read", False)))
            if data.get("ack") is False:
                segment.address_nak = True
            self.segments.append(segment)
            self.current = segment
            self.end_time = end_time
            return None
        if kind == "data":
            if self.current is None:
                return None
            value = _first_byte(data.get("data"))
            if value is not None:
                self.current.data.append(value)
            if data.get("ack") is False:
                self.current.data_naks.append(max(0, len(self.current.data) - 1))
            self.end_time = end_time
            return None
        if kind != "stop" or not self.active:
            return None
        self.end_time = end_time
        emission = self._finish()
        self._reset()
        return emission

    def _finish(self) -> Optional[Emission]:
        segments = [segment for segment in self.segments if self._accepts(segment.address)]
        if not segments:
            return None
        last = segments[-1]
        if last.read:
            pointer = None
            for segment in reversed(segments[:-1]):
                if segment.address == last.address and not segment.read and segment.data:
                    pointer = segment.data[0]
                    break
            if pointer is None:
                pointer = self.register_pointers.get(last.address)
            transaction = self.decoder.decode_i2c(last.address, "READ", pointer, last.data)
            if pointer is not None:
                self.register_pointers[last.address] = self._advance_pointer(pointer, len(last.data))
        else:
            if not last.data:
                return None
            pointer = last.data[0]
            self.register_pointers[last.address] = pointer
            if len(last.data) == 1:
                transaction = self.decoder.decode_i2c(last.address, "POINTER", pointer, [])
                transaction.derived = [f"Register pointer set to 0x{pointer:02X}"]
            else:
                transaction = self.decoder.decode_i2c(last.address, "WRITE", pointer, last.data[1:])
                self.register_pointers[last.address] = self._advance_pointer(
                    pointer, len(last.data) - 1
                )
        ack_errors = sum(1 for segment in segments if segment.address_nak)
        for segment in segments:
            if segment.read:
                # The controller normally NAKs the final byte to end an I2C read.
                final_index = len(segment.data) - 1
                ack_errors += sum(index != final_index for index in segment.data_naks)
            else:
                ack_errors += len(segment.data_naks)
        if ack_errors:
            transaction.status = f"{ack_errors} NAK frame(s)"
        return Emission(transaction, self.start_time, self.end_time)

    def _accepts(self, address: int) -> bool:
        return self.accepted_addresses is None or address in self.accepted_addresses

    def _advance_pointer(self, pointer: int, count: int) -> int:
        if self.decoder.page == UI_PAGE and pointer == FIFO_DATA:
            return FIFO_DATA
        return (pointer + count) & 0x7F


class SpiAssembler:
    def __init__(self, decoder: Qmi8660Decoder) -> None:
        self.decoder = decoder
        self.gap_us = 20.0
        self.seen_cs = False
        self._reset_transaction()

    def _reset_transaction(self) -> None:
        self.active = False
        self.start_time: Any = None
        self.end_time: Any = None
        self.last_end: Any = None
        self.mosi: List[int] = []
        self.miso: List[int] = []

    def feed(self, frame_type: str, data: Dict[str, Any], start_time: Any, end_time: Any) -> Optional[Emission]:
        kind = frame_type.lower()
        if kind == "enable":
            self.seen_cs = True
            self._reset_transaction()
            self.active = True
            self.start_time = start_time
            self.end_time = end_time
            self.last_end = end_time
            return None
        if kind == "disable":
            self.seen_cs = True
            if not self.active or not self.mosi:
                self._reset_transaction()
                return None
            self.end_time = end_time
            return self._emit()
        if kind != "result":
            return None
        mosi = _bytes_value(data.get("mosi"))
        miso = _bytes_value(data.get("miso"))
        if not mosi and not miso:
            return None
        if self.seen_cs and not self.active:
            return None
        if not self.seen_cs and self.active and self.last_end is not None:
            try:
                gap = float(start_time - self.last_end) * 1_000_000.0
            except (TypeError, ValueError):
                gap = 0.0
            if gap > self.gap_us:
                previous = self._emit()
                self.active = True
                self.start_time = start_time
                self.end_time = end_time
                self.last_end = end_time
                self.mosi.extend(mosi)
                self.miso.extend(miso)
                return previous
        if not self.active:
            self.active = True
            self.start_time = start_time
        self.end_time = end_time
        self.last_end = end_time
        self.mosi.extend(mosi)
        self.miso.extend(miso)
        return None

    def _emit(self) -> Emission:
        emission = Emission(
            self.decoder.decode_spi(self.mosi, self.miso), self.start_time, self.end_time
        )
        self._reset_transaction()
        return emission


def _first_byte(value: Any) -> Optional[int]:
    values = _bytes_value(value)
    return values[0] if values else None


def _bytes_value(value: Any) -> List[int]:
    if value is None:
        return []
    if isinstance(value, int):
        return [value & 0xFF]
    if isinstance(value, (bytes, bytearray, list, tuple)):
        return [int(item) & 0xFF for item in value]
    if isinstance(value, str):
        token = value.strip()
        try:
            return [int(token, 0) & 0xFF]
        except ValueError:
            return []
    return []
