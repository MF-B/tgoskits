//! Small rk3x register-block transport, following the existing PMIC polling
//! flow. Kept separate from the qualified I2C0 DVFS path to avoid changing it.
//! The rdrive device borrow owns the whole physical bus for each operation;
//! callers must use that same device for every slave on this controller.

use core::time::Duration;

use mmio_api::MmioRaw;
use rdrive::DriverGeneric;

const CON: usize = 0x00;
const CLKDIV: usize = 0x04;
const MRXADDR: usize = 0x08;
const MRXRADDR: usize = 0x0c;
const MTXCNT: usize = 0x10;
const MRXCNT: usize = 0x14;
const IEN: usize = 0x18;
const IPD: usize = 0x1c;
const TX: usize = 0x100;
const RX: usize = 0x200;
const EN: u32 = 1;
const REGISTER_TX: u32 = 1 << 1;
const START: u32 = 1 << 3;
const STOP: u32 = 1 << 4;
const LASTACK: u32 = 1 << 5;
const ACTACK: u32 = 1 << 6;
const MBTF: u32 = 1 << 2;
const MBRF: u32 = 1 << 3;
const START_DONE: u32 = 1 << 4;
const STOP_DONE: u32 = 1 << 5;
const NACK: u32 = 1 << 6;
const ALL: u32 = 0x7f;
const VALID: u32 = 1 << 24;
const POLL_US: u32 = 100_000;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Error {
    Nack,
    Timeout,
    Invalid,
}

pub(crate) struct Rk3xI2c {
    mmio: MmioRaw,
    tuning: u32,
}

impl DriverGeneric for Rk3xI2c {
    fn name(&self) -> &str {
        "rk3588-i2c-polled"
    }
}

impl Rk3xI2c {
    /// Configure v1 timing from the actual functional clock. The divider and
    /// SDA/START/STOP tuning equations are documented in Linux v6.6
    /// drivers/i2c/busses/i2c-rk3x.c. No controller or slave reset is asserted.
    pub(crate) fn new(
        mmio: MmioRaw,
        input_hz: u64,
        bus_hz: u32,
        rise_ns: u32,
        fall_ns: u32,
    ) -> Result<Self, Error> {
        if input_hz == 0 || input_hz > 1_000_000_000 || !(1_000..=400_000).contains(&bus_hz) {
            return Err(Error::Invalid);
        }
        let (low_ns, high_ns, start_ns, stop_ns, hold_ns, setup_ns) = if bus_hz <= 100_000 {
            (4700u64, 4000u64, 4700u64, 4000u64, 3450u64, 250u64)
        } else {
            (1300u64, 600u64, 600u64, 600u64, 900u64, 100u64)
        };
        let mut low = (input_hz * (low_ns + u64::from(fall_ns)))
            .div_ceil(8_000_000_000)
            .max(2);
        let mut high = (input_hz * (high_ns + u64::from(rise_ns)))
            .div_ceil(8_000_000_000)
            .max(2);
        let total = input_hz.div_ceil(u64::from(bus_hz) * 8);
        if total > low + high {
            let extra = total - low - high;
            let extra_low = (low * extra).div_ceil(low + high);
            low += extra_low;
            high += extra - extra_low;
        }
        if low > 65_536 || high > 65_536 {
            return Err(Error::Invalid);
        }
        let sda = (1..=3u64)
            .rev()
            .find(|s| {
                (low * s + 1) * 1_000_000_000 < hold_ns * input_hz
                    && ((8 - s) * low + 1) * 1_000_000_000 > setup_ns * input_hz
            })
            .ok_or(Error::Invalid)?;
        let start = (input_hz * (start_ns + u64::from(rise_ns)))
            .saturating_sub(1_000_000_000)
            .div_ceil(8_000_000_000 * high)
            .max(1);
        let stop = (input_hz * (stop_ns + u64::from(rise_ns)))
            .saturating_sub(1_000_000_000)
            .div_ceil(8_000_000_000 * high)
            .max(1);
        if start > 4 || stop > 4 {
            return Err(Error::Invalid);
        }
        let tuning = (((sda - 1) << 8) | ((start - 1) << 12) | ((stop - 1) << 14)) as u32;
        let bus = Self { mmio, tuning };
        bus.write(IEN, 0);
        bus.write(CON, tuning);
        bus.write(IPD, ALL);
        bus.write(CLKDIV, (((high - 1) << 16) | (low - 1)) as u32);
        log::info!(
            "rk3588-i2c: input={input_hz} Hz bus={bus_hz} Hz CLKDIV={:#x} tuning={tuning:#x}",
            bus.read(CLKDIV)
        );
        Ok(bus)
    }

    fn read(&self, offset: usize) -> u32 {
        self.mmio.read::<u32>(offset)
    }

    fn write(&self, offset: usize, value: u32) {
        self.mmio.write::<u32>(offset, value);
    }

    fn wait(&self, event: u32) -> Result<(), Error> {
        for _ in 0..POLL_US {
            let pending = self.read(IPD);
            if pending & NACK != 0 {
                self.write(IPD, NACK);
                return Err(Error::Nack);
            }
            if pending & event != 0 {
                self.write(IPD, event);
                return Ok(());
            }
            axklib::time::busy_wait(Duration::from_micros(1));
        }
        Err(Error::Timeout)
    }

    fn start(&self, mode: u32) -> Result<(), Error> {
        self.write(IPD, ALL);
        // Poll raw status with hardware IRQ delivery disabled throughout.
        self.write(CON, self.tuning | EN | ACTACK | START | mode);
        self.wait(START_DONE)?;
        self.write(CON, self.tuning | EN | ACTACK | mode);
        Ok(())
    }

    fn finish(&self, result: Result<(), Error>) -> Result<(), Error> {
        self.write(IPD, ALL);
        self.write(CON, self.tuning | EN | STOP);
        let stopped = self.wait(STOP_DONE);
        self.write(CON, self.tuning);
        self.write(IEN, 0);
        self.write(IPD, ALL);
        result.and(stopped)
    }

    /// One combined register-address + repeated-START read. At most one FIFO,
    /// allowing the seven-byte RTC calendar to be latched coherently.
    pub(crate) fn read_regs(&mut self, chip: u8, reg: u8, data: &mut [u8]) -> Result<(), Error> {
        if chip > 0x7f || data.is_empty() || data.len() > 32 {
            return Err(Error::Invalid);
        }
        self.write(MRXADDR, (u32::from(chip) << 1) | 1 | VALID);
        self.write(MRXRADDR, u32::from(reg) | VALID);
        let result = (|| {
            self.start(REGISTER_TX)?;
            self.write(CON, self.tuning | EN | ACTACK | REGISTER_TX | LASTACK);
            self.write(MRXCNT, data.len() as u32);
            self.wait(MBRF)?;
            for (index, chunk) in data.chunks_mut(4).enumerate() {
                let word = self.read(RX + index * 4).to_le_bytes();
                chunk.copy_from_slice(&word[..chunk.len()]);
            }
            Ok(())
        })();
        self.finish(result)
    }

    /// Single FIFO write: slave address, register address, then payload.
    pub(crate) fn write_regs(&mut self, chip: u8, reg: u8, data: &[u8]) -> Result<(), Error> {
        if chip > 0x7f || data.is_empty() || data.len() > 30 {
            return Err(Error::Invalid);
        }
        let mut packet = [0u8; 32];
        packet[0] = chip << 1;
        packet[1] = reg;
        packet[2..2 + data.len()].copy_from_slice(data);
        for (index, word) in packet[..(data.len() + 2).next_multiple_of(4)]
            .chunks_exact(4)
            .enumerate()
        {
            self.write(
                TX + index * 4,
                u32::from_le_bytes([word[0], word[1], word[2], word[3]]),
            );
        }
        let result = (|| {
            self.start(0)?;
            self.write(MTXCNT, (data.len() + 2) as u32);
            self.wait(MBTF)
        })();
        self.finish(result)
    }
}
