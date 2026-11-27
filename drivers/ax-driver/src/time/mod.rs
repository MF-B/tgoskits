use log::{debug, info};
use rdrive::probe::OnProbeError;

#[cfg(target_arch = "x86_64")]
mod cmos;
#[cfg(any(test, target_arch = "x86_64"))]
mod cmos_decode;
#[cfg(any(
    test,
    all(target_arch = "aarch64", feature = "rockchip-soc"),
    target_arch = "loongarch64",
    target_arch = "riscv64",
    target_arch = "x86_64"
))]
mod datetime;
#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
mod fdt;
#[cfg(target_arch = "riscv64")]
mod goldfish;
#[cfg(any(test, all(target_arch = "aarch64", feature = "rockchip-soc")))]
mod hym8563;
#[cfg(target_arch = "loongarch64")]
mod loongson;
#[cfg(any(test, target_arch = "loongarch64"))]
mod loongson_decode;
#[cfg(target_arch = "aarch64")]
mod pl031;
#[cfg(target_arch = "riscv64")]
mod starfive;
#[cfg(any(test, target_arch = "riscv64"))]
mod starfive_decode;

fn init_epoch_offset(node_name: &str, unix_timestamp: u64) -> Result<(), OnProbeError> {
    if unix_timestamp == 0 {
        return Err(OnProbeError::other(alloc::format!(
            "[{node_name}] returned zero unix timestamp"
        )));
    }

    let epoch_time_nanos = unix_timestamp * 1_000_000_000;
    if axklib::time::try_init_epoch_offset(epoch_time_nanos) {
        info!("Initialized wall clock from {node_name}");
    } else {
        debug!("Skipping RTC {node_name} because epoch offset is already initialized",);
    }

    Ok(())
}

/// UTC calendar supplied to a registered hardware RTC.
#[derive(Clone, Copy, Debug)]
pub struct RtcDateTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// Hardware errors are kept distinct from an absent RTC and software wall time.
#[derive(Clone, Copy, Debug)]
pub enum RtcError {
    NoDevice,
    Busy,
    Io,
    Timeout,
    InvalidTime,
    VoltageLow,
    ClockStopped,
}

/// Read UTC Unix seconds. `None` means no hardware RTC was registered;
/// an invalid battery-backed calendar is an error, never a wall-clock fallback.
pub fn read_hardware_rtc() -> Result<Option<u64>, RtcError> {
    #[cfg(all(target_arch = "aarch64", feature = "rockchip-soc"))]
    {
        let Some(device) = rdrive::get_one::<hym8563::Hym8563>() else {
            return Ok(None);
        };
        let mut rtc = device.try_lock().map_err(|_| RtcError::Busy)?;
        rtc.read_time().map(Some)
    }
    #[cfg(not(all(target_arch = "aarch64", feature = "rockchip-soc")))]
    {
        Ok(None)
    }
}

/// Set the battery-backed UTC calendar. This does not change the software clock.
/// The OS adapter must check the caller's time-setting privilege first.
pub fn set_hardware_rtc(time: RtcDateTime) -> Result<(), RtcError> {
    #[cfg(all(target_arch = "aarch64", feature = "rockchip-soc"))]
    {
        let device = rdrive::get_one::<hym8563::Hym8563>().ok_or(RtcError::NoDevice)?;
        let mut rtc = device.try_lock().map_err(|_| RtcError::Busy)?;
        rtc.set_time(time)
    }
    #[cfg(not(all(target_arch = "aarch64", feature = "rockchip-soc")))]
    {
        let _ = time;
        Err(RtcError::NoDevice)
    }
}
