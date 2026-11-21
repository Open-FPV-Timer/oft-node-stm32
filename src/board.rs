//! Board facts and tuning numbers: the S32_BPill pin map (NuclearHazard Core, Atom and, by
//! every sign, Fission), clocks, and how raw readings become levels.
//!
//! Pins, for reference (the pin types themselves are named in `main.rs`):
//!
//! | Signal | Pin |
//! |---|---|
//! | RSSI 1-8 | PA0-PA7 (ADC1 channels 0-7), pulled down on the board |
//! | Select 1-8 | PB6, PB7, PB8, PB9, PB12, PB13, PB14, PB15 |
//! | RX5808 data, clock (shared) | PB3, PB4 (JTAG pins: JTAG is switched off, SWD stays) |
//! | UART to the Pi | PA9 TX, PA10 RX (USART1), 921600 8N1, Pi `/dev/ttyAMA0` |
//! | Status LED | PC13 |
//! | Not used yet | PA8 buzzer, PB0 line from Pi GPIO24, PB1 input voltage, PA15 |

use embassy_stm32::adc::SampleTime;
use embassy_stm32::gpio::SwjCfg;
use embassy_stm32::rcc::{ADCPrescaler, AHBPrescaler, APBPrescaler, Hse, HseMode, Pll, PllMul, PllPreDiv, PllSource, Sysclk};
use embassy_stm32::time::Hertz;
use embassy_stm32::Config;

pub const FW_VERSION: [u8; 3] = [0, 1, 0];

/// Receiver slots on the board.
pub const SLOTS: usize = 8;
/// The baud rate RotorHazard uses on the same wires, so the Pi side needs no new setup.
pub const BAUD: u32 = 921_600;

/// Sample period per receiver, µs.
pub const DT_US: u16 = 1000;
/// Samples per `Samples` message: 20 ms of signal.
pub const BATCH: usize = 20;
/// Wait after tuning before samples mean anything. An RX5808's RSSI takes tens of
/// milliseconds to settle on a new frequency.
pub const SETTLE_MS: u64 = 40;

/// 20 µs at the 12 MHz ADC clock: the RSSI outputs are not low-impedance sources.
pub const SAMPLE_TIME: SampleTime = SampleTime::CYCLES239_5;
/// A slot reading below this (about 40 mV) at start-up has no receiver: an idle RX5808
/// sits near 0.35 V.
pub const PRESENT_MIN_RAW: u32 = 50;
/// At the ADC's rail. An RX5808 saturates near 1.1 V, well below it, so this only catches
/// a fault; saturation shows as a flat top.
pub const CLIP_RAW: u16 = 4000;

/// "No noise floor estimate" in `Samples`: the Timer tracks the floor itself.
pub const NO_FLOOR: i16 = i16::MIN;

/// RX5808 RSSI slope, mV per dB. PROVISIONAL: about 15 mV/dB by one informal measurement;
/// to be set from passes recorded beside a C5 Node.
pub const MV_PER_DB: i32 = 15;

/// A 12-bit reading (3.3 V full scale) as a level in 0.1 dB. Relative, not calibrated: zero
/// is 0 V, not a known power.
pub fn raw_to_ddb(raw: u16) -> i16 {
    let mv = raw as i32 * 3300 / 4095;
    (mv * 10 / MV_PER_DB) as i16
}

/// 72 MHz from the board's 8 MHz resonator (the stock firmware runs the same way), the ADC
/// at 12 MHz, and JTAG off so PB3 and PB4 are ordinary pins.
pub fn clocks(mut c: Config) -> Config {
    c.rcc.hse = Some(Hse { freq: Hertz(8_000_000), mode: HseMode::Oscillator });
    c.rcc.pll = Some(Pll { src: PllSource::HSE, prediv: PllPreDiv::DIV1, mul: PllMul::MUL9 });
    c.rcc.sys = Sysclk::PLL1_P;
    c.rcc.ahb_pre = AHBPrescaler::DIV1;
    c.rcc.apb1_pre = APBPrescaler::DIV2;
    c.rcc.apb2_pre = APBPrescaler::DIV1;
    c.rcc.adc_pre = ADCPrescaler::DIV6;
    c.swj = SwjCfg::SwdOnly;
    c
}
