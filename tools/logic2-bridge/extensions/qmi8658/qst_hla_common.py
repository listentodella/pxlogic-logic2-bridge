from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence, Tuple


def load_registers(filename: str) -> Dict[int, Dict[str, Any]]:
    raw = json.loads(Path(__file__).with_name(filename).read_text(encoding="utf-8"))
    page = raw.get("pages", {}).get("0", {})
    return {int(address): definition for address, definition in page.items()}


def hex_bytes(data: Sequence[int]) -> str:
    return " ".join(f"{value & 0xFF:02X}" for value in data)


def signed_i16(data: Sequence[int], offset: int, byte_order: str = "little") -> int:
    if byte_order == "big":
        value = ((data[offset] & 0xFF) << 8) | (data[offset + 1] & 0xFF)
    else:
        value = (data[offset] & 0xFF) | ((data[offset + 1] & 0xFF) << 8)
    return value - 0x10000 if value & 0x8000 else value


def field_bounds(bits: str) -> Tuple[int, int]:
    if ":" in bits:
        high, low = bits.split(":", 1)
        return int(high), int(low)
    bit = int(bits)
    return bit, bit


def enum_label(description: str, value: int, width: int) -> Optional[str]:
    pattern = re.compile(r"\s*(0[xX][0-9a-fA-F]+|0[bB][01]+|[0-9]+)\s*=\s*(.+?)\s*$")
    for line in description.splitlines():
        match = pattern.match(line)
        if not match:
            continue
        token, label = match.groups()
        if token.lower().startswith(("0x", "0b")):
            candidate = int(token, 0)
        elif len(token) == width and set(token) <= {"0", "1"}:
            candidate = int(token, 2)
        elif len(token) > 1 and token.startswith("0") and set(token) <= {"0", "1"}:
            candidate = int(token, 2)
        else:
            candidate = int(token, 10)
        if candidate == value:
            return label
    return None


def decode_register_fields(
    registers: Dict[int, Dict[str, Any]], register: int, values: Sequence[int]
) -> List[str]:
    decoded: List[str] = []
    for offset, value in enumerate(values[:16]):
        address = (register + offset) & 0x7F
        definition = registers.get(address)
        if not definition:
            continue
        parts: List[str] = []
        for item in definition.get("fields", []):
            high, low = field_bounds(item["bits"])
            if high > 7:
                continue
            width = high - low + 1
            field_value = (value >> low) & ((1 << width) - 1)
            label = enum_label(item.get("description", ""), field_value, width)
            rendered = f"{item['name']}={field_value}"
            if label:
                rendered += f" ({label})"
            parts.append(rendered)
        if parts:
            decoded.append(f"{definition['name']}: " + ", ".join(parts))
    return decoded


@dataclass
class DecodedTransaction:
    bus: str
    operation: str
    register: Optional[int]
    register_name: str
    data: List[int]
    device_address: Optional[int] = None
    status: str = "OK"
    fields: List[str] = field(default_factory=list)
    derived: List[str] = field(default_factory=list)
    raw_mosi: List[int] = field(default_factory=list)
    raw_miso: List[int] = field(default_factory=list)

    def frame_data(self) -> Dict[str, str]:
        address = "" if self.device_address is None else f" @0x{self.device_address:02X}"
        register = (
            self.register_name
            if self.register is None
            else f"{self.register_name} (0x{self.register:02X})"
        )
        return {
            "Bus": self.bus,
            "Op": self.operation,
            "Address": address,
            "Register": register,
            "Hex": hex_bytes(self.data),
            "Detail": "; ".join((self.derived + self.fields)[:12]),
            "Status": self.status,
        }


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
    def __init__(self, decoder: Any, default_addresses: Sequence[int]) -> None:
        self.decoder = decoder
        self.default_addresses = {address & 0x7F for address in default_addresses}
        self.accepted_addresses: Optional[set[int]] = set(self.default_addresses)
        self.register_pointers: Dict[int, int] = {}
        self._reset()

    def _reset(self) -> None:
        self.active = False
        self.start_time: Any = None
        self.end_time: Any = None
        self.segments: List[_I2cSegment] = []
        self.current: Optional[_I2cSegment] = None

    def set_address_mode(self, mode: str) -> None:
        if mode == "Any":
            self.accepted_addresses = None
            return
        addresses = {int(token, 16) for token in re.findall(r"0x([0-9a-fA-F]+)", mode)}
        self.accepted_addresses = addresses or set(self.default_addresses)

    def feed(
        self, frame_type: str, data: Dict[str, Any], start_time: Any, end_time: Any
    ) -> Optional[Emission]:
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
            address = first_byte(data.get("address"))
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
            value = first_byte(data.get("data"))
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
                self.register_pointers[last.address] = self.decoder.advance_i2c_pointer(
                    pointer, len(last.data)
                )
        else:
            if not last.data:
                return None
            pointer = last.data[0]
            self.register_pointers[last.address] = pointer
            if len(last.data) == 1:
                transaction = self.decoder.decode_i2c(last.address, "POINTER", pointer, [])
                transaction.derived = [f"Register pointer set to 0x{pointer:02X}"]
            else:
                transaction = self.decoder.decode_i2c(
                    last.address, "WRITE", pointer, last.data[1:]
                )
                self.register_pointers[last.address] = self.decoder.advance_i2c_pointer(
                    pointer, len(last.data) - 1
                )
        ack_errors = sum(1 for segment in segments if segment.address_nak)
        for segment in segments:
            if segment.read:
                final_index = len(segment.data) - 1
                ack_errors += sum(index != final_index for index in segment.data_naks)
            else:
                ack_errors += len(segment.data_naks)
        if ack_errors:
            transaction.status = f"{ack_errors} NAK frame(s)"
        return Emission(transaction, self.start_time, self.end_time)

    def _accepts(self, address: int) -> bool:
        return self.accepted_addresses is None or address in self.accepted_addresses


class SpiAssembler:
    def __init__(self, decoder: Any) -> None:
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

    def feed(
        self, frame_type: str, data: Dict[str, Any], start_time: Any, end_time: Any
    ) -> Optional[Emission]:
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
        mosi = bytes_value(data.get("mosi"))
        miso = bytes_value(data.get("miso"))
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


def first_byte(value: Any) -> Optional[int]:
    values = bytes_value(value)
    return values[0] if values else None


def bytes_value(value: Any) -> List[int]:
    if value is None:
        return []
    if isinstance(value, int):
        return [value & 0xFF]
    if isinstance(value, (bytes, bytearray, list, tuple)):
        return [int(item) & 0xFF for item in value]
    if isinstance(value, str):
        try:
            return [int(value.strip(), 0) & 0xFF]
        except ValueError:
            return []
    return []
