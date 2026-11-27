//! HYM8563 RTC on RK3588. Calendar layout and the STOP/write/restart sequence
//! follow Linux v6.6 drivers/rtc/rtc-hym8563.c.

use alloc::vec::Vec;

#[cfg(all(target_arch = "aarch64", feature = "rockchip-soc"))]
pub(super) use driver::Hym8563;
use rdrive::probe::{
    OnProbeError,
    fdt::{Fdt, NodeType, Phandle},
};

struct I2cResources {
    address: u8,
    base: usize,
    size: usize,
    resets: Vec<ResetRegister>,
}

#[derive(Debug, PartialEq, Eq)]
struct ResetRegister {
    base: usize,
    size: usize,
    offset: usize,
    mask: u32,
}

fn mmio_window(node: NodeType<'_>, min_size: usize) -> Result<(usize, usize), OnProbeError> {
    let invalid = || {
        OnProbeError::other(alloc::format!(
            "hym8563: invalid MMIO reg on {}",
            node.name()
        ))
    };
    let reg = node.regs().into_iter().next().ok_or_else(invalid)?;
    let base = usize::try_from(reg.address).map_err(|_| invalid())?;
    let size = reg
        .size
        .and_then(|size| usize::try_from(size).ok())
        .ok_or_else(invalid)?;
    if !base.is_multiple_of(4) || size < min_size || base.checked_add(size).is_none() {
        return Err(invalid());
    }
    Ok((base, size))
}

const I2C_COMPATIBLES: &[&str] = &["rockchip,rk3588-i2c", "rockchip,rk3399-i2c"];

fn i2c_resources(fdt: &Fdt, rtc: NodeType<'_>) -> Result<I2cResources, OnProbeError> {
    let address =
        rtc.as_node()
            .get_property("reg")
            .and_then(|reg| reg.get_u32())
            .filter(|address| (0x03..=0x77).contains(address))
            .ok_or_else(|| OnProbeError::other("hym8563: invalid I2C slave reg"))? as u8;
    let bus = rtc
        .parent()
        .filter(|bus| {
            bus.as_node()
                .compatibles()
                .any(|value| I2C_COMPATIBLES.contains(&value))
        })
        .ok_or_else(|| OnProbeError::other("hym8563: missing Rockchip I2C parent"))?;
    // Include all eight 32-bit words in the RX FIFO at 0x200.
    let (base, size) = mmio_window(bus, 0x220)?;
    Ok(I2cResources {
        address,
        base,
        size,
        resets: reset_registers(fdt, bus)?,
    })
}

fn reset_registers(fdt: &Fdt, bus: NodeType<'_>) -> Result<Vec<ResetRegister>, OnProbeError> {
    let Some(property) = bus.as_node().get_property("resets") else {
        return Ok(Vec::new());
    };
    if !property.data.len().is_multiple_of(8) {
        return Err(OnProbeError::other(
            "hym8563: incomplete I2C reset specifier",
        ));
    }
    let mut resets = Vec::new();
    let mut cells = property.get_u32_iter();
    while let (Some(phandle), Some(id)) = (cells.next(), cells.next()) {
        let cru = fdt
            .get_by_phandle(Phandle::from(phandle))
            .filter(|node| {
                node.as_node()
                    .compatibles()
                    .any(|value| value == "rockchip,rk3588-cru")
            })
            .ok_or_else(|| {
                OnProbeError::other("hym8563: I2C reset provider is not an RK3588 CRU")
            })?;
        if cru
            .as_node()
            .get_property("#reset-cells")
            .and_then(|cells| cells.get_u32())
            != Some(1)
        {
            return Err(OnProbeError::other(
                "hym8563: RK3588 CRU must use one reset cell",
            ));
        }
        // BSP IDs encode 16 reset bits per 32-bit SOFTRST_CON register.
        let offset = 0xa00 + (id as usize / 16) * 4;
        let mask = 1 << (id % 16);
        let (base, size) = mmio_window(cru, offset + 4)?;
        resets.push(ResetRegister {
            base,
            size,
            offset,
            mask,
        });
    }
    Ok(resets)
}

#[cfg(all(target_arch = "aarch64", feature = "rockchip-soc"))]
mod driver {
    use core::time::Duration;

    use log::{info, warn};
    use mmio_api::{MmioAddr, MmioRaw};
    use rdif_pinctrl::PinctrlDevice;
    use rdrive::{
        Device, DriverGeneric,
        probe::{OnProbeError, fdt::NodeType},
        register::ProbeFdt,
    };

    use super::{I2C_COMPATIBLES, ResetRegister, i2c_resources};
    use crate::{
        soc::rockchip::polled_i2c::{Error as I2cError, Rk3xI2c},
        time::{RtcDateTime, RtcError, datetime::datetime_to_unix_timestamp, init_epoch_offset},
    };

    const STOP: u8 = 1 << 5;
    const VOLTAGE_LOW: u8 = 1 << 7;

    crate::model_register!(
        name: "Rockchip HYM8563 RTC",
        level: ProbeLevel::PostKernel,
        priority: ProbePriority::DEFAULT,
        probe_kinds: &[ProbeKind::Fdt {
            compatibles: I2C_COMPATIBLES,
            on_probe: probe
        }],
    );

    /// The RTC refers to the registered physical bus, so another slave must borrow
    /// this same Rk3xI2c device. Neither borrow disables preemption during a poll.
    pub(in crate::time) struct Hym8563 {
        bus: Device<Rk3xI2c>,
        address: u8,
    }

    impl DriverGeneric for Hym8563 {
        fn name(&self) -> &str {
            "hym8563"
        }
    }

    fn i2c_error(error: I2cError) -> RtcError {
        match error {
            I2cError::Nack => RtcError::Io,
            I2cError::Timeout => RtcError::Timeout,
            I2cError::Invalid => RtcError::InvalidTime,
        }
    }

    fn decode_bcd(value: u8) -> Result<u32, RtcError> {
        let low = value & 0x0f;
        let high = value >> 4;
        if low > 9 || high > 9 {
            return Err(RtcError::InvalidTime);
        }
        Ok(u32::from(high) * 10 + u32::from(low))
    }

    fn decode_time(registers: &[u8; 9]) -> Result<u64, RtcError> {
        if registers[2] & VOLTAGE_LOW != 0 {
            return Err(RtcError::VoltageLow);
        }
        // A stopped or test-mode oscillator is not a usable source of wall time.
        if registers[0] & (STOP | (1 << 7) | (1 << 3)) != 0 {
            return Err(RtcError::ClockStopped);
        }
        if registers[6] & 7 > 6 {
            return Err(RtcError::InvalidTime);
        }
        // Match Linux's 2000..2099 range; HYM8563's century bit is unreliable.
        datetime_to_unix_timestamp(
            2000 + decode_bcd(registers[8])? as i32,
            decode_bcd(registers[7] & 0x1f)?,
            decode_bcd(registers[5] & 0x3f)?,
            decode_bcd(registers[4] & 0x3f)?,
            decode_bcd(registers[3] & 0x7f)?,
            decode_bcd(registers[2] & 0x7f)?,
        )
        .ok_or(RtcError::InvalidTime)
    }

    fn encode_bcd(value: u32) -> u8 {
        (((value / 10) << 4) | (value % 10)) as u8
    }

    impl Hym8563 {
        pub(in crate::time) fn read_time(&mut self) -> Result<u64, RtcError> {
            let mut bus = self.bus.try_lock().map_err(|_| RtcError::Busy)?;
            // One burst includes all seven calendar bytes: no torn date at a tick.
            let mut registers = [0u8; 9];
            bus.read_regs(self.address, 0, &mut registers)
                .map_err(i2c_error)?;
            decode_time(&registers)
        }

        pub(in crate::time) fn set_time(&mut self, time: RtcDateTime) -> Result<(), RtcError> {
            if !(2000..=2099).contains(&time.year) {
                return Err(RtcError::InvalidTime);
            }
            let timestamp = datetime_to_unix_timestamp(
                time.year,
                time.month,
                time.day,
                time.hour,
                time.minute,
                time.second,
            )
            .ok_or(RtcError::InvalidTime)?;
            // Linux rtc_set_time also recomputes weekday rather than trusting it.
            let weekday = ((timestamp / 86_400 + 4) % 7) as u32;
            let calendar = [
                encode_bcd(time.second),
                encode_bcd(time.minute),
                encode_bcd(time.hour),
                encode_bcd(time.day),
                encode_bcd(weekday),
                encode_bcd(time.month),
                encode_bcd((time.year - 2000) as u32),
            ];
            let mut bus = self.bus.try_lock().map_err(|_| RtcError::Busy)?;
            // Retain bus ownership across STOP, the whole calendar, and restart.
            // Even an error may have reached the chip, so always try to restart it.
            let written = bus
                .write_regs(self.address, 0, &[STOP])
                .and_then(|()| bus.write_regs(self.address, 2, &calendar));
            let restarted = bus.write_regs(self.address, 0, &[0]);
            if restarted.is_err() {
                warn!(
                    "hym8563: failed to restart oscillator after RTC_SET_TIME; calendar may be \
                     stopped"
                );
            }
            written.and(restarted).map_err(i2c_error)
        }
    }

    fn enabled(node: NodeType<'_>) -> bool {
        node.as_node()
            .get_property("status")
            .is_none_or(|property| {
                property
                    .as_str_iter()
                    .any(|status| status == "okay" || status == "ok")
            })
    }

    fn map_mmio(base: usize, size: usize) -> Result<MmioRaw, OnProbeError> {
        axklib::mmio::ioremap_raw(MmioAddr::from(base as u64), size).map_err(|error| {
            OnProbeError::other(alloc::format!(
                "hym8563: MMIO map at {base:#x} failed: {error:?}"
            ))
        })
    }

    fn release_resets(resets: &[ResetRegister]) -> Result<(), OnProbeError> {
        for reset in resets {
            let cru = map_mmio(reset.base, reset.size)?;
            // Upper-half write enables select only this reset bit, which is cleared.
            cru.write::<u32>(reset.offset, reset.mask << 16);
            if cru.read::<u32>(reset.offset) & reset.mask != 0 {
                return Err(OnProbeError::other("hym8563: I2C reset remains asserted"));
            }
        }
        if !resets.is_empty() {
            axklib::time::busy_wait(Duration::from_micros(10));
        }
        Ok(())
    }

    fn probe(probe: ProbeFdt<'_>) -> Result<(), OnProbeError> {
        let (info, platform) = probe.into_parts();
        if !enabled(info.node) || rdrive::get_one::<Hym8563>().is_some() {
            return Ok(());
        }
        let Some(child) = info.available_children().into_iter().find(|child| {
            let node = child.node();
            enabled(node)
                && node
                    .as_node()
                    .compatibles()
                    .any(|value| value == "haoyu,hym8563")
        }) else {
            return Ok(());
        };
        let fdt =
            rdrive::fdt_ref().ok_or_else(|| OnProbeError::other("hym8563: FDT unavailable"))?;
        // The BSP and mainline use the same CRU compatible but different reset IDs.
        if !fdt.get_by_path("/").is_some_and(|root| {
            root.as_node()
                .compatibles()
                .any(|value| value == "rockchip,rk3588-orangepi-5-plus")
        }) {
            return Ok(());
        }
        let resources = i2c_resources(fdt, child.node())?;
        let address = resources.address;
        let clock = info
            .find_clock_line_by_name("i2c")?
            .ok_or_else(|| OnProbeError::other("hym8563: I2C functional clock missing"))?;
        let pclk = info
            .find_clock_line_by_name("pclk")?
            .ok_or_else(|| OnProbeError::other("hym8563: I2C peripheral clock missing"))?;
        pclk.enable()?;
        clock.enable()?;
        let input_hz = clock.rate()?;
        release_resets(&resources.resets)?;
        // Rockchip pinctrl is registered at PreKernel/CLK. This PostKernel/DEFAULT
        // probe also runs after the CRU's PostKernel/CLK registration.
        let pinctrl = rdrive::get_one::<PinctrlDevice>()
            .ok_or_else(|| OnProbeError::other("hym8563: pinctrl provider unavailable"))?;
        let mut pinctrl = pinctrl
            .try_lock()
            .map_err(|_| OnProbeError::other("hym8563: pinctrl provider busy"))?;
        pinctrl
            .apply_fdt_default_state(fdt, info.node.as_node())
            .map_err(|error| {
                OnProbeError::other(alloc::format!("hym8563: I2C pinctrl failed: {error:?}"))
            })?;
        drop(pinctrl);
        let property_u32 = |name: &str, default| {
            info.node
                .as_node()
                .get_property(name)
                .and_then(|property| property.get_u32())
                .unwrap_or(default)
        };
        let bus_hz = property_u32("clock-frequency", 100_000);
        let rise_ns = property_u32(
            "i2c-scl-rising-time-ns",
            if bus_hz <= 100_000 { 1000 } else { 300 },
        );
        let fall_ns = property_u32("i2c-scl-falling-time-ns", 300);
        let mmio = map_mmio(resources.base, resources.size)?;
        let mut controller =
            Rk3xI2c::new(mmio, input_hz, bus_hz, rise_ns, fall_ns).map_err(|error| {
                OnProbeError::other(alloc::format!("hym8563: I2C timing failed: {error:?}"))
            })?;
        let mut registers = [0u8; 9];
        controller
            .read_regs(address, 0, &mut registers)
            .map_err(|error| {
                OnProbeError::other(alloc::format!(
                    "hym8563: I2C address {address:#x} read failed: {error:?}"
                ))
            })?;
        // A low-voltage/invalid calendar still proves the DT-described chip is
        // reachable and must remain available for privileged RTC_SET_TIME repair.
        let initial_time = decode_time(&registers);
        platform.register(controller);
        let bus = rdrive::get::<Rk3xI2c>(platform.descriptor().device_id()).map_err(|error| {
            OnProbeError::other(alloc::format!(
                "hym8563: I2C registration unavailable: {error}"
            ))
        })?;
        platform
            .register_fdt_child(child, Hym8563 { bus, address })
            .map_err(|error| {
                OnProbeError::other(alloc::format!(
                    "hym8563: RTC registration failed: {error:?}"
                ))
            })?;
        info!(
            "hym8563: registered hardware RTC on I2C@{:#x}, address {address:#x}",
            resources.base
        );
        match initial_time {
            Ok(timestamp) => init_epoch_offset("hym8563", timestamp)?,
            Err(error) => warn!(
                "hym8563: RTC calendar unusable ({error:?}); wall clock not initialized; use \
                 privileged RTC_SET_TIME"
            ),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOARD_DTB: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../os/StarryOS/configs/board/orangepi-5-plus.dtb"
    ));
    const BUS_PATH: &str = "/i2c@fec80000";
    const RTC_PATH: &str = "/i2c@fec80000/hym8563@51";

    #[test]
    fn parses_orange_pi_5_plus_resources() {
        let fdt = Fdt::from_bytes(BOARD_DTB).unwrap();
        let resources = i2c_resources(&fdt, fdt.get_by_path(RTC_PATH).unwrap()).unwrap();
        assert_eq!(resources.address, 0x51);
        assert_eq!((resources.base, resources.size), (0xfec8_0000, 0x1000));
        assert_eq!(
            resources.resets,
            [
                ResetRegister {
                    base: 0xfd7c_0000,
                    size: 0x5c000,
                    offset: 0xa2c,
                    mask: 1 << 5
                },
                ResetRegister {
                    base: 0xfd7c_0000,
                    size: 0x5c000,
                    offset: 0xa28,
                    mask: 1 << 13
                },
            ]
        );
    }

    #[test]
    fn accepts_changed_address_and_missing_resets_but_rejects_reserved_address() {
        let mut fdt = Fdt::from_bytes(BOARD_DTB).unwrap();
        let bus_id = fdt.get_by_path(BUS_PATH).unwrap().id();
        fdt.node_mut(bus_id).unwrap().remove_property("resets");
        let rtc_id = fdt.get_by_path(RTC_PATH).unwrap().id();
        let set_address = |fdt: &mut Fdt, address| {
            fdt.node_mut(rtc_id)
                .unwrap()
                .get_property_mut("reg")
                .unwrap()
                .set_u32_ls(&[address]);
        };
        set_address(&mut fdt, 0x77);
        let resources = i2c_resources(&fdt, fdt.get_by_path(RTC_PATH).unwrap()).unwrap();
        assert_eq!(resources.address, 0x77);
        assert!(resources.resets.is_empty());
        set_address(&mut fdt, 0x78);
        assert!(i2c_resources(&fdt, fdt.get_by_path(RTC_PATH).unwrap()).is_err());
    }

    #[test]
    fn rejects_reset_register_outside_cru_window() {
        let mut fdt = Fdt::from_bytes(BOARD_DTB).unwrap();
        let bus_id = fdt.get_by_path(BUS_PATH).unwrap().id();
        let resets = fdt
            .node_mut(bus_id)
            .unwrap()
            .get_property_mut("resets")
            .unwrap();
        let phandle = resets.get_u32_iter().next().unwrap();
        resets.set_u32_ls(&[phandle, u32::MAX]);
        assert!(i2c_resources(&fdt, fdt.get_by_path(RTC_PATH).unwrap()).is_err());
    }
}
