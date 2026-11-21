//! OFT Node firmware for RotorHazard-style STM32 timers (the S32_BPill pin map, as on the
//! NuclearHazard boards): one STM32F103 with up to eight RX5808 receivers.
//!
//! The board becomes one OFT Node (spec §5). Each receiver is one seat: it stays on its
//! frequency and is sampled at 1 kHz, so nothing hops. No detection runs here; the Timer
//! (`oftd` on the board's Raspberry Pi) does that with `oft-core`.
//!
//! Written from the hardware's pin map and the RTC6715 datasheet. It shares no code with
//! RotorHazard's node firmware.

#![no_std]
#![no_main]

mod board;
mod rx5808;

use embassy_executor::Spawner;
use embassy_stm32::adc::{Adc, AdcChannel, AnyAdcChannel};
use embassy_stm32::gpio::{AnyPin, Level, Output, Speed};
use embassy_stm32::peripherals::{ADC1, USART1};
use embassy_stm32::usart::{BufferedUart, BufferedUartRx, BufferedUartTx, Config as UartConfig};
use embassy_stm32::{bind_interrupts, Config, Peri};
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Ticker, Timer};
use embedded_io_async::{Read, Write};
use oft_nodelink::msg::{Features, Hello, Message, Plan, Pong, SampleFlags, Samples, Status, GAIN_AUTO};
use oft_nodelink::{encode_frame, FrameDecoder, LINK_VERSION, MAX_FRAME};
use panic_halt as _;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use static_cell::StaticCell;

bind_interrupts!(struct Irqs {
    USART1 => embassy_stm32::usart::BufferedInterruptHandler<USART1>;
    ADC1_2 => embassy_stm32::adc::InterruptHandler<ADC1>;
});

type Tx = Mutex<ThreadModeRawMutex, BufferedUartTx<'static>>;

/// The newest plan from the Timer, for the sampler to act on.
static PLAN: Signal<ThreadModeRawMutex, Plan> = Signal::new();
/// A plan has arrived since the Timer was last heard from: it has seen our Hello.
static GOT_PLAN: AtomicBool = AtomicBool::new(false);
/// Seconds since the last Ping. A Timer pings a few times a second; silence means it went
/// away (or restarted), so Hello is repeated for the next one.
static QUIET_S: AtomicU32 = AtomicU32::new(0);
/// Receivers found at start-up.
static RECEIVERS: AtomicU16 = AtomicU16::new(0);
/// Plan entries beyond the receivers fitted, and frequencies a receiver cannot tune.
static PLAN_ERRORS: AtomicU16 = AtomicU16::new(0);
/// Sample ticks that ran late enough to skip.
static OVERRUNS: AtomicU16 = AtomicU16::new(0);

/// Node clock: µs since start-up.
fn now_us() -> u64 {
    Instant::now().as_micros()
}

async fn send(tx: &Tx, m: &Message) {
    let mut buf = [0u8; MAX_FRAME];
    if let Ok(n) = encode_frame(m, &mut buf) {
        let _ = tx.lock().await.write_all(&buf[..n]).await;
    }
}

fn hello() -> Message {
    // 96-bit unique id folded to 64 bits.
    let uid = embassy_stm32::uid::uid();
    let mut id = [0u8; 8];
    for (k, b) in uid.iter().enumerate() {
        id[k % 8] ^= b.rotate_left((k / 8) as u32 * 3);
    }
    let [major, minor, patch] = board::FW_VERSION;
    Message::Hello(Hello {
        chip_id: u64::from_le_bytes(id),
        fw: [major, minor, patch],
        link_version: LINK_VERSION,
        freq_min_mhz: rx5808::FREQ_MIN_MHZ,
        freq_max_mhz: rx5808::FREQ_MAX_MHZ,
        max_seats: RECEIVERS.load(Ordering::Relaxed) as u8,
        features: Features::empty(),
    })
}

/// Bytes from the Timer: answer pings at once, hand plans to the sampler.
#[embassy_executor::task]
async fn rx_task(mut rx: BufferedUartRx<'static>, tx: &'static Tx) {
    let mut dec = FrameDecoder::new();
    let mut buf = [0u8; 64];
    loop {
        let n = match rx.read(&mut buf).await {
            Ok(n) => n,
            Err(_) => continue,
        };
        for &b in &buf[..n] {
            let Some(Ok(m)) = dec.push(b) else { continue };
            match m {
                Message::Ping(p) => {
                    QUIET_S.store(0, Ordering::Relaxed);
                    send(tx, &Message::Pong(Pong { id: p.id, t_timer: p.t_timer, t_node: now_us() })).await;
                }
                Message::Plan(p) => {
                    GOT_PLAN.store(true, Ordering::Relaxed);
                    PLAN.signal(p);
                }
                // The Pi can also reset us with its GPIO; this is the polite way.
                Message::Reset => cortex_m::peripheral::SCB::sys_reset(),
                // No LED worth blinking for Identify yet, and the Pi enters the ROM
                // bootloader with its own reset and BOOT0 lines, so Dfu has nothing to do.
                _ => {}
            }
        }
    }
}

/// Once a second: Status, and Hello until a Timer has answered it with a plan.
#[embassy_executor::task]
async fn housekeeping_task(tx: &'static Tx, mut led: Output<'static>) {
    let mut tick = Ticker::every(Duration::from_secs(1));
    loop {
        tick.next().await;
        led.toggle();
        if QUIET_S.fetch_add(1, Ordering::Relaxed) >= 3 {
            GOT_PLAN.store(false, Ordering::Relaxed);
        }
        if !GOT_PLAN.load(Ordering::Relaxed) {
            send(tx, &hello()).await;
        }
        let status = Status {
            temp_dc: i16::MIN,
            hop_errors: PLAN_ERRORS.load(Ordering::Relaxed),
            overruns: OVERRUNS.load(Ordering::Relaxed),
            gain_index: GAIN_AUTO,
        };
        send(tx, &Message::Status(status)).await;
    }
}

/// One fitted receiver: its RSSI input and its select line.
struct Receiver {
    rssi: AnyAdcChannel<'static, ADC1>,
    select: Output<'static>,
}

/// A receiver that a plan has put on a seat.
#[derive(Clone, Copy)]
struct Seat {
    seat: u8,
    n: u8,
    /// Node time of the first sample in `batch`.
    t0: u64,
    flags: SampleFlags,
    batch: [i16; board::BATCH],
}

/// Tune the receivers to the plan, then sample every seat at 1 kHz and send batches.
#[embassy_executor::task]
async fn sample_task(
    mut adc: Adc<'static, ADC1>,
    receivers: &'static mut [Option<Receiver>; board::SLOTS],
    mut bus: rx5808::Bus,
    tx: &'static Tx,
) {
    // Wait for the modules' supply and RSSI outputs to settle, then see which slots are
    // fitted: an empty slot's input is pulled to ground on the board.
    Timer::after_millis(300).await;
    let mut fitted = 0u16;
    for slot in receivers.iter_mut() {
        let present = match slot {
            Some(r) => {
                let mut sum = 0u32;
                for _ in 0..8 {
                    sum += adc.read(&mut r.rssi, board::SAMPLE_TIME).await as u32;
                }
                sum / 8 >= board::PRESENT_MIN_RAW
            }
            None => false,
        };
        if present {
            fitted += 1;
        } else {
            *slot = None;
        }
    }
    RECEIVERS.store(fitted, Ordering::Relaxed);

    let mut plan = PLAN.wait().await;
    loop {
        // --- Apply the plan: its k-th seat goes to the k-th fitted receiver.
        let mut seats: [Option<Seat>; board::SLOTS] = [None; board::SLOTS];
        let mut entries = plan.seats().iter();
        for (slot, state) in receivers.iter_mut().zip(seats.iter_mut()) {
            let Some(r) = slot else { continue };
            let Some(&(seat, freq_mhz)) = entries.next() else { break };
            match rx5808::synth_word(freq_mhz) {
                Some(word) => {
                    bus.write(&mut r.select, rx5808::REG_SYNTH_B, word).await;
                    *state = Some(Seat { seat, n: 0, t0: 0, flags: SampleFlags::POST_HOP, batch: [0; board::BATCH] });
                }
                None => {
                    PLAN_ERRORS.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        PLAN_ERRORS.fetch_add(entries.count() as u16, Ordering::Relaxed);
        Timer::after_millis(board::SETTLE_MS).await;

        // --- Sample until the next plan.
        let mut tick = Ticker::every(Duration::from_micros(board::DT_US as u64));
        let mut due = Instant::now();
        loop {
            if let Some(p) = PLAN.try_take() {
                plan = p;
                break;
            }
            tick.next().await;
            due += Duration::from_micros(board::DT_US as u64);
            let t = now_us();
            // Running late (a long flash of other work): restart the batches rather than
            // send samples with the wrong times.
            if Instant::now() > due + Duration::from_micros(board::DT_US as u64) {
                OVERRUNS.fetch_add(1, Ordering::Relaxed);
                for s in seats.iter_mut().flatten() {
                    s.n = 0;
                }
                tick.reset();
                due = Instant::now();
                continue;
            }
            for (slot, state) in receivers.iter_mut().zip(seats.iter_mut()) {
                let (Some(r), Some(s)) = (slot, state) else { continue };
                let raw = adc.read(&mut r.rssi, board::SAMPLE_TIME).await;
                if s.n == 0 {
                    s.t0 = t;
                }
                if raw >= board::CLIP_RAW {
                    s.flags = SampleFlags(s.flags.0 | SampleFlags::CLIPPED.0);
                }
                s.batch[s.n as usize] = board::raw_to_ddb(raw);
                s.n += 1;
            }
            for s in seats.iter_mut().flatten() {
                if (s.n as usize) < board::BATCH {
                    continue;
                }
                let mut m = Samples::new(s.seat, s.t0, board::DT_US, board::NO_FLOOR, s.flags);
                for &p in &s.batch {
                    let _ = m.push(p);
                }
                s.n = 0;
                s.flags = SampleFlags::empty();
                send(tx, &Message::Samples(m)).await;
            }
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_stm32::init(board::clocks(Config::default()));

    // --- The Pi's UART: USART1 on PA9 (TX) and PA10 (RX).
    static TX_BUF: StaticCell<[u8; 1024]> = StaticCell::new();
    static RX_BUF: StaticCell<[u8; 256]> = StaticCell::new();
    let mut ucfg = UartConfig::default();
    ucfg.baudrate = board::BAUD;
    let uart = BufferedUart::new(p.USART1, p.PA10, p.PA9, TX_BUF.init([0; 1024]), RX_BUF.init([0; 256]), Irqs, ucfg).unwrap();
    let (uart_tx, uart_rx) = uart.split();
    static TX: StaticCell<Tx> = StaticCell::new();
    let tx: &'static Tx = TX.init(Mutex::new(uart_tx));

    // --- Receivers: RSSI on PA0-PA7, select on PB6-PB9 and PB12-PB15, shared data and clock.
    let sel = |pin: Peri<'static, AnyPin>| Output::new(pin, Level::High, Speed::Low);
    static RECEIVER_SLOTS: StaticCell<[Option<Receiver>; board::SLOTS]> = StaticCell::new();
    let receivers = RECEIVER_SLOTS.init([
        Some(Receiver { rssi: p.PA0.degrade_adc(), select: sel(p.PB6.into()) }),
        Some(Receiver { rssi: p.PA1.degrade_adc(), select: sel(p.PB7.into()) }),
        Some(Receiver { rssi: p.PA2.degrade_adc(), select: sel(p.PB8.into()) }),
        Some(Receiver { rssi: p.PA3.degrade_adc(), select: sel(p.PB9.into()) }),
        Some(Receiver { rssi: p.PA4.degrade_adc(), select: sel(p.PB12.into()) }),
        Some(Receiver { rssi: p.PA5.degrade_adc(), select: sel(p.PB13.into()) }),
        Some(Receiver { rssi: p.PA6.degrade_adc(), select: sel(p.PB14.into()) }),
        Some(Receiver { rssi: p.PA7.degrade_adc(), select: sel(p.PB15.into()) }),
    ]);
    let bus = rx5808::Bus::new(Output::new(p.PB3, Level::Low, Speed::Low), Output::new(p.PB4, Level::Low, Speed::Low));
    let adc = Adc::new(p.ADC1);
    let led = Output::new(p.PC13, Level::High, Speed::Low);

    spawner.spawn(rx_task(uart_rx, tx).unwrap());
    spawner.spawn(housekeeping_task(tx, led).unwrap());
    spawner.spawn(sample_task(adc, receivers, bus, tx).unwrap());
}
