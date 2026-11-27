use core::{any::Any, ffi::c_int};

use ax_driver::time::{RtcDateTime, RtcError, read_hardware_rtc, set_hardware_rtc};
use axfs_ng_vfs::{DeviceId, NodeFlags, VfsError, VfsResult};
use chrono::{Datelike, Timelike};
use linux_raw_sys::ioctl::{RTC_RD_TIME, RTC_SET_TIME};

use crate::{mm::{VmMutPtr, VmPtr}, pseudofs::DeviceOps};

/// The device ID for /dev/rtc0
pub const RTC0_DEVICE_ID: DeviceId = DeviceId::new(250, 0);

#[repr(C)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, bytemuck::NoUninit)]
#[allow(non_camel_case_types, dead_code)]
struct rtc_time {
    tm_sec: c_int,
    tm_min: c_int,
    tm_hour: c_int,
    tm_mday: c_int,
    tm_mon: c_int,
    tm_year: c_int,
    tm_wday: c_int,
    tm_yday: c_int,
    tm_isdst: c_int,
}

/// RTC device
pub struct Rtc;

impl DeviceOps for Rtc {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Ok(0)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Ok(0)
    }

    fn ioctl(&self, current: &crate::task::UserTaskRef, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            RTC_RD_TIME => {
                let wall = match read_hardware_rtc().map_err(rtc_error)? {
                    Some(seconds) => chrono::DateTime::from_timestamp(seconds as i64, 0)
                        .ok_or(VfsError::InvalidInput)?,
                    None => chrono::DateTime::from_timestamp_nanos(
                        ax_runtime::hal::time::wall_time_nanos() as _,
                    ),
                };
                (arg as *mut rtc_time)
                    .vm_write(
                        current,
                        rtc_time {
                            tm_sec: wall.second() as _,
                            tm_min: wall.minute() as _,
                            tm_hour: wall.hour() as _,
                            tm_mday: wall.day() as _,
                            tm_mon: wall.month0() as _,
                            tm_year: (wall.year() - 1900) as _,
                            tm_wday: wall.weekday().num_days_from_sunday() as _,
                            tm_yday: wall.ordinal0() as _,
                            tm_isdst: 0,
                        },
                    )
                    .map_err(|error| VfsError::from(crate::StarryError::from(error)))?;
            }
            RTC_SET_TIME => {
                let thread = current.as_thread();
                // This changes a global, battery-backed device. A capability
                // obtained in a child user namespace cannot authorize it.
                let namespace = thread.proc_data.namespace_snapshot().user_ns.clone();
                if namespace.lock_irqsave().level() != 0
                    || !thread.cred().has_cap_sys_time()
                {
                    // Linux rtc_dev_ioctl checks CAP_SYS_TIME before copying
                    // the input and reports EACCES for this ioctl.
                    return Err(VfsError::PermissionDenied);
                }
                let time = (arg as *const rtc_time).vm_read(current)
                    .map_err(|error| VfsError::from(crate::StarryError::from(error)))?;
                let positive = |value| u32::try_from(value).map_err(|_| VfsError::InvalidInput);
                set_hardware_rtc(RtcDateTime {
                    year: time.tm_year.checked_add(1900).ok_or(VfsError::InvalidInput)?,
                    month: positive(time.tm_mon.checked_add(1).ok_or(VfsError::InvalidInput)?)?,
                    day: positive(time.tm_mday)?,
                    hour: positive(time.tm_hour)?,
                    minute: positive(time.tm_min)?,
                    second: positive(time.tm_sec)?,
                }).map_err(rtc_error)?;
            }
            _ => return Err(VfsError::NotATty),
        }
        Ok(0)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM
    }
}

fn rtc_error(error: RtcError) -> VfsError {
    match error {
        RtcError::NoDevice => VfsError::NoSuchDevice,
        RtcError::Busy => VfsError::ResourceBusy,
        RtcError::Io => VfsError::Io,
        RtcError::Timeout => VfsError::TimedOut,
        RtcError::InvalidTime | RtcError::VoltageLow | RtcError::ClockStopped => VfsError::InvalidInput,
    }
}
