#!/usr/bin/env python3
"""Flash, back up or inspect the timer board's STM32 from its Raspberry Pi.

Runs on the Pi, with nothing but Python's standard library and Raspberry Pi OS's `pinctrl`.
It uses the same wires RotorHazard's own updater does: the Pi's UART for data, GPIO17 for
the STM32's reset and GPIO27 for BOOT0, and talks to the STM32's built-in ROM bootloader
(ST application note AN3155). The bootloader is in ROM, so a failed or wrong flash cannot
lock the board out: run this again, or put RotorHazard's firmware back (README).

  nodeflash.py info                 chip id, flash size, bootloader version
  nodeflash.py backup OUT.bin       read the whole flash to a file
  nodeflash.py flash  FILE.bin      erase, write, verify, restart
  nodeflash.py run                  restart the STM32 into its firmware

Anything else using the port must be stopped first (RotorHazard: `sudo systemctl stop
rotorhazard`; oftd likewise). The user needs the `dialout` and `gpio` groups.

GPL-3.0-or-later.
"""
import argparse
import os
import select
import subprocess
import sys
import termios
import time

ACK, NACK = 0x79, 0x1F
FLASH_BASE = 0x0800_0000
# STM32F1 system memory: flash size in KiB, then the 96-bit unique id.
F1_FLASH_SIZE_ADDR = 0x1FFF_F7E0
F1_UID_ADDR = 0x1FFF_F7E8
PRODUCTS = {0x410: "STM32F103 medium-density (C8/CB)", 0x412: "STM32F103 low-density", 0x414: "STM32F103 high-density"}


class Refused(Exception):
    """The bootloader said no, or did not answer."""


class Pins:
    """The reset and BOOT0 lines, through `pinctrl`."""

    def __init__(self, reset, boot0):
        self.reset, self.boot0 = reset, boot0

    @staticmethod
    def _set(pin, mode):
        subprocess.run(["pinctrl", "set", str(pin), *mode.split()], check=True)

    def restart(self, bootloader):
        self._set(self.boot0, "op dh" if bootloader else "op dl")
        self._set(self.reset, "op dl")
        time.sleep(0.05)
        self._set(self.reset, "ip pn")  # released: the STM32 pulls its own reset line up
        time.sleep(0.1)
        if not bootloader:
            self._set(self.boot0, "ip pd")


class Port:
    """The UART, raw, 8 data bits, even parity (what the bootloader speaks)."""

    def __init__(self, path, baud):
        self.fd = os.open(path, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
        a = termios.tcgetattr(self.fd)
        speed = getattr(termios, f"B{baud}")
        a[0] = 0  # iflag: no translation, no parity stripping
        a[1] = 0  # oflag
        a[2] = termios.CS8 | termios.PARENB | termios.CREAD | termios.CLOCAL
        a[3] = 0  # lflag: raw
        a[4] = a[5] = speed
        a[6][termios.VMIN], a[6][termios.VTIME] = 0, 0
        termios.tcsetattr(self.fd, termios.TCSANOW, a)

    def flush(self):
        termios.tcflush(self.fd, termios.TCIOFLUSH)

    def write(self, data):
        data = bytes(data)
        while data:
            select.select([], [self.fd], [], 1)
            data = data[os.write(self.fd, data):]

    def read(self, n, timeout=1.0):
        out, end = b"", time.monotonic() + timeout
        while len(out) < n:
            left = end - time.monotonic()
            if left <= 0 or not select.select([self.fd], [], [], left)[0]:
                raise Refused(f"no answer (wanted {n} bytes, got {len(out)})")
            out += os.read(self.fd, n - len(out))
        return out


def xor(data):
    x = 0
    for b in data:
        x ^= b
    return x


class Bootloader:
    def __init__(self, port):
        self.p = port

    def _ack(self, what, timeout=1.0):
        b = self.p.read(1, timeout)[0]
        if b == NACK:
            raise Refused(f"{what}: refused")
        if b != ACK:
            raise Refused(f"{what}: unexpected answer 0x{b:02x}")

    def connect(self):
        """Send the byte the bootloader measures the baud rate from."""
        self.p.flush()
        self.p.write([0x7F])
        b = self.p.read(1)[0]
        if b not in (ACK, NACK):  # NACK: it had already measured the baud rate
            raise Refused(f"not a bootloader answer: 0x{b:02x}")

    def _cmd(self, c):
        self.p.write([c, c ^ 0xFF])
        self._ack(f"command 0x{c:02x}")

    def _addr(self, a):
        raw = a.to_bytes(4, "big")
        self.p.write(raw + bytes([xor(raw)]))
        self._ack(f"address 0x{a:08x}")

    def get(self):
        """Bootloader version and the commands it supports."""
        self._cmd(0x00)
        n = self.p.read(1)[0]
        data = self.p.read(n + 1)
        self._ack("get")
        return data[0], data[1:]

    def product_id(self):
        self._cmd(0x02)
        n = self.p.read(1)[0]
        pid = int.from_bytes(self.p.read(n + 1), "big")
        self._ack("get id")
        return pid

    def read(self, addr, n):
        self._cmd(0x11)
        self._addr(addr)
        self.p.write([n - 1, (n - 1) ^ 0xFF])
        self._ack("read length")
        return self.p.read(n)

    def write(self, addr, data):
        assert 0 < len(data) <= 256 and len(data) % 4 == 0
        self._cmd(0x31)
        self._addr(addr)
        body = bytes([len(data) - 1]) + data
        self.p.write(body + bytes([xor(body)]))
        self._ack("write", timeout=2.0)

    def erase_pages(self, pages, commands):
        if 0x43 in commands:
            self._cmd(0x43)
            body = bytes([len(pages) - 1, *pages])
        else:  # the two-byte form newer bootloaders use
            self._cmd(0x44)
            body = (len(pages) - 1).to_bytes(2, "big") + b"".join(p.to_bytes(2, "big") for p in pages)
        self.p.write(body + bytes([xor(body)]))
        self._ack("erase", timeout=30.0)


def chip_facts(bl):
    version, commands = bl.get()
    pid = bl.product_id()
    facts = {"bootloader": f"{version >> 4}.{version & 15}", "commands": commands, "pid": pid}
    try:
        facts["flash_kib"] = int.from_bytes(bl.read(F1_FLASH_SIZE_ADDR, 2), "little")
        facts["uid"] = bl.read(F1_UID_ADDR, 12).hex()
    except Refused as e:
        # A read-protected chip refuses every read.
        facts["read_error"] = str(e)
    return facts


def show(facts):
    print(f"bootloader   {facts['bootloader']}")
    print(f"product id   0x{facts['pid']:03x}  {PRODUCTS.get(facts['pid'], '(not one this tool knows)')}")
    if "read_error" in facts:
        print(f"reads        refused ({facts['read_error']}): the chip may be read-protected")
    else:
        print(f"flash size   {facts['flash_kib']} KiB (as the chip reports it)")
        print(f"unique id    {facts['uid']}")


def progress(done, total):
    print(f"\r  {done * 100 // total:3d}%  {done}/{total} bytes", end="", flush=True)


def port_users(path):
    """Services known to hold the port."""
    busy = []
    for unit in ("rotorhazard", "oftd"):
        r = subprocess.run(["systemctl", "is-active", "--quiet", unit])
        if r.returncode == 0:
            busy.append(unit)
    return busy


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("action", choices=["info", "backup", "flash", "run"])
    ap.add_argument("file", nargs="?")
    ap.add_argument("--port", default="/dev/ttyAMA0")
    ap.add_argument("--baud", type=int, default=57600)
    ap.add_argument("--reset-gpio", type=int, default=17)
    ap.add_argument("--boot0-gpio", type=int, default=27)
    ap.add_argument("--page", type=int, default=1024, help="flash page size in bytes (1024 on the F103C8)")
    ap.add_argument("--force", action="store_true", help="go ahead although a service seems to be using the port")
    args = ap.parse_args()
    if args.action in ("backup", "flash") and not args.file:
        ap.error(f"{args.action} needs a file")

    busy = port_users(args.port)
    if busy and not args.force:
        sys.exit(f"{' and '.join(busy)} is running and uses the port. Stop it first: sudo systemctl stop {' '.join(busy)}")

    pins = Pins(args.reset_gpio, args.boot0_gpio)
    if args.action == "run":
        pins.restart(bootloader=False)
        print("restarted into the firmware")
        return

    image = None
    if args.action == "flash":
        try:
            image = open(args.file, "rb").read()
        except OSError as e:
            sys.exit(f"cannot read {args.file}: {e.strerror}")
        if len(image) < 8 or not 0x2000_0000 <= int.from_bytes(image[:4], "little") <= 0x2002_0000 or int.from_bytes(image[4:8], "little") >> 24 != 0x08:
            sys.exit(f"{args.file} does not start like STM32 firmware for 0x08000000 (stack pointer, then reset address)")
        image += b"\xff" * (-len(image) % 4)

    port = Port(args.port, args.baud)
    try:
        pins.restart(bootloader=True)
        bl = Bootloader(port)
        bl.connect()
        facts = chip_facts(bl)
        show(facts)

        if args.action == "backup":
            if "read_error" in facts:
                sys.exit("cannot back up: the chip refuses reads")
            total = facts["flash_kib"] * 1024
            data = b""
            while len(data) < total:
                data += bl.read(FLASH_BASE + len(data), min(256, total - len(data)))
                progress(len(data), total)
            print()
            with open(args.file, "wb") as f:
                f.write(data)
            used = len(data.rstrip(b"\xff"))
            print(f"wrote {args.file}: {len(data)} bytes, {used} in use")

        if args.action == "flash":
            if "flash_kib" in facts and len(image) > facts["flash_kib"] * 1024:
                sys.exit(f"the firmware ({len(image)} bytes) does not fit in {facts['flash_kib']} KiB")
            pages = list(range((len(image) + args.page - 1) // args.page))
            print(f"erasing {len(pages)} pages")
            bl.erase_pages(pages, facts["commands"])
            print("writing")
            for at in range(0, len(image), 256):
                bl.write(FLASH_BASE + at, image[at:at + 256])
                progress(min(at + 256, len(image)), len(image))
            print("\nverifying")
            for at in range(0, len(image), 256):
                chunk = image[at:at + 256]
                if bl.read(FLASH_BASE + at, len(chunk)) != chunk:
                    sys.exit(f"\nverify failed at 0x{FLASH_BASE + at:08x}: flash it again")
                progress(min(at + 256, len(image)), len(image))
            print("\nflashed and verified")
    except Refused as e:
        sys.exit(f"bootloader: {e}")
    finally:
        # Always leave the STM32 running its firmware, never parked in the bootloader.
        pins.restart(bootloader=False)
    print("restarted into the firmware")


if __name__ == "__main__":
    main()
