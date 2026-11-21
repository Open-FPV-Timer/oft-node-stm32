//! RX5808 (RTC6715) tuning over its three-wire serial port, from the RTC6715 datasheet.
//!
//! One write is 25 bits, least significant first: a 4-bit register address, a write bit,
//! then 20 data bits. Data is taken on the clock's rising edge while the receiver's select
//! line is low. All receivers share data and clock; each has its own select.

use embassy_stm32::gpio::Output;
use embassy_time::Timer;

/// Synthesizer register B: the frequency.
pub const REG_SYNTH_B: u8 = 0x01;

/// The 5.8 GHz FPV band, with a little margin.
pub const FREQ_MIN_MHZ: u16 = 5645;
pub const FREQ_MAX_MHZ: u16 = 5945;

/// The receiver's intermediate frequency, MHz: the local oscillator runs this far below
/// the channel.
const IF_MHZ: u16 = 479;

/// Half a clock period. The chip is far faster; this is slow enough for long traces and
/// eight loads on the bus.
const HALF_BIT_US: u64 = 20;

/// The register B value for a channel frequency, or `None` outside the band.
///
/// The oscillator is `2 * (32 * N + A)` MHz with `A` in 0..32; the register holds
/// `N << 7 | A`.
pub fn synth_word(freq_mhz: u16) -> Option<u32> {
    if !(FREQ_MIN_MHZ..=FREQ_MAX_MHZ).contains(&freq_mhz) {
        return None;
    }
    let half_lo = ((freq_mhz - IF_MHZ) / 2) as u32;
    Some((half_lo / 32) << 7 | (half_lo % 32))
}

/// The shared data and clock lines.
pub struct Bus {
    data: Output<'static>,
    clock: Output<'static>,
}

impl Bus {
    pub fn new(data: Output<'static>, clock: Output<'static>) -> Self {
        Self { data, clock }
    }

    /// Write 20 bits to one register of the receiver behind `select`.
    pub async fn write(&mut self, select: &mut Output<'static>, register: u8, value: u32) {
        let word = (register as u32 & 0x0F) | 1 << 4 | (value & 0x000F_FFFF) << 5;
        self.clock.set_low();
        select.set_low();
        Timer::after_micros(HALF_BIT_US).await;
        for bit in 0..25 {
            if word >> bit & 1 == 1 {
                self.data.set_high();
            } else {
                self.data.set_low();
            }
            Timer::after_micros(HALF_BIT_US).await;
            self.clock.set_high();
            Timer::after_micros(HALF_BIT_US).await;
            self.clock.set_low();
        }
        Timer::after_micros(HALF_BIT_US).await;
        select.set_high();
        self.data.set_low();
        Timer::after_micros(HALF_BIT_US).await;
    }
}
