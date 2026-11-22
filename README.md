# oft-node-stm32: OFT Node firmware for RotorHazard-style STM32 timers

Turns the STM32 on a RotorHazard timer board into an [OFT](../oft-spec/oft-spec.md) Node.
The board's Raspberry Pi then runs `oftd` (from [oft](../oft)) and the box is an OFT Timer.

**Status: first light.** On a NuclearHazard Fission (STM32F103, four RX5808s, Pi 5) it
finds the four receivers and streams all of them at 1 kHz to `oftd` on the Pi, and the
levels follow the tuned frequency. Flown beside a C5 Node on a 2 ft whoop gate (2026-11-21,
25 mW): 25 of 26 passes with no false crossings at a manual 850 mV threshold (see Levels).

- One STM32F103 drives up to eight RX5808 receivers. Each receiver is one seat: it stays on
  its frequency and is sampled at 1 kHz. Nothing hops.
- The STM32 only tunes, samples and timestamps. Detection runs on the Pi in `oft-core`, the
  same code every other OFT Timer uses.
- It speaks the Node link (spec §5) on the wires RotorHazard uses: the Pi's UART
  (`/dev/ttyAMA0`, 921600 baud). So `oftd --serial /dev/ttyAMA0` is the whole Pi side.
- Going back to RotorHazard is a reflash, done from the Pi. The STM32's bootloader is in
  ROM and cannot be overwritten.

It is written from the board's pin map and the RTC6715 datasheet, and shares no code with
RotorHazard's node firmware.

## Boards

Any board with RotorHazard's S32_BPill pin map and an STM32F103C8 should work (`src/board.rs`
has the pins). That is the NuclearHazard Core and Atom by their schematics. No schematic is
published for the Fission; it runs RotorHazard's generic F103 firmware, so its receiver
pins must be the same. The firmware finds which of the eight slots are fitted at start-up.

Not supported: STM32F4 boards, and timers with one Arduino per receiver.

## Build

```sh
rustup target add thumbv7m-none-eabi
rustup component add llvm-tools
cargo build --release
"$(find ~/.rustup/toolchains -name llvm-objcopy | head -1)" -O binary \
    target/thumbv7m-none-eabi/release/oft-node-stm32 oft-node-stm32.bin
```

## Flash, from the board's Pi

`tools/nodeflash.py` needs only Python and Raspberry Pi OS's `pinctrl`. Copy it and the
`.bin` to the Pi. Stop whatever uses the UART first.

```sh
sudo systemctl stop rotorhazard
./nodeflash.py info                       # chip id and flash size; changes nothing
./nodeflash.py backup stock.bin           # keep what is on the chip now
./nodeflash.py flash oft-node-stm32.bin  # erase, write, verify, restart
```

It talks to the STM32's ROM bootloader over the UART, with GPIO17 on reset and GPIO27 on
BOOT0, as RotorHazard's updater does.

### Back to RotorHazard

Flash the backup, or RotorHazard's own image, then start RotorHazard again:

```sh
./nodeflash.py flash stock.bin
# or: cd ~/RotorHazard/src/server && python server.py --flashbpill
sudo systemctl start rotorhazard
```

## Run the Timer on the Pi

Build `oftd` for the Pi with `oft/cross/build-pi.sh` (Docker), copy it over, and:

```sh
./oftd --serial /dev/ttyAMA0      # serves http://<pi>:7270/
```

To have it start at every boot, run `oft/dist/install-service.sh` on the Pi instead, and
stop RotorHazard from starting: `sudo systemctl disable --now rotorhazard`.

## Levels

An RX5808 reports signal strength as a voltage on its RSSI pin, and that is what this Node
reports: **millivolts**, one per count (`board::raw_to_mv`, `Features::LEVEL_MV`). The Timer
passes the unit on (`"level":{"unit":"mV","scale":1}` in `/info`) and its page shows mV.

It is not converted to dB because the output is not a straight line in dB. Measured beside
a C5 Node (whoop at 25 mW, 2 ft gate, this Node on the floor under it):

| | RSSI |
|---|---|
| Nothing transmitting | about 380 mV |
| VTX powered in the same room | about 700 mV (the C5 barely hears it yet) |
| Return flights past the outside of the gate | 710 to 870 mV |
| Passes through the gate | 846 to 912 mV, flattening near the top |

So the RX5808 is far more sensitive than the C5 and spends a close-range race in the top,
compressed part of its range. Use `manual` thresholds, as with RotorHazard's EnterAt and
ExitAt: enter 850 mV and exit 810 mV worked here. The `auto` mode works too, but its
numbers are then millivolts above the floor, and its defaults are tuned for dB Nodes.
RotorHazard shows the same voltage as a 0 to 255 count (about 6.5 mV per count on this
board), so an RH level is roughly millivolts divided by 6.5.

## Not done yet

- SYNC input. The board has a line from Pi GPIO24 to PB0 that could carry it; until then
  the clock comes from pings.
- Buzzer (PA8), input voltage (PB1), Identify.
- The extra register writes some RX5808 firmware does at start-up (power-down of unused
  blocks). Tuning alone has been enough on other projects; to confirm here.
- Flashing from the Timer's page (`oftd` would need the bootloader steps `nodeflash.py` has).

GPL-3.0-or-later.
