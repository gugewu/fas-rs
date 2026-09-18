//! 本地实现的 CPU 硬件周期读取器。
//!
//! 用 `perf_event_open` 直接读取每个核心的 `PERF_COUNT_HW_CPU_CYCLES`
//! 硬件计数器，语义为“该核心在采样窗口内实际执行的周期数”。
//!
//! 替代 sysinfo 的时间片占用率，避免降频后时间片占比虚高的问题。

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;

/// 一段周期数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cycles(pub u64);

impl Cycles {
    /// 从频率（kHz）构造一个“满负载参考周期数”。
    pub fn from_khz(khz: u64) -> Self {
        Cycles(khz)
    }

    /// 计算利用率：实际周期数 / 满负载参考周期数。
    pub fn as_usage(&self, duration: Duration, freq_cycles: Cycles) -> Result<f64> {
        let seconds = duration.as_secs_f64();
        let freq_hz = freq_cycles.0 as f64 * 1000.0; // kHz -> Hz
        let capacity = seconds * freq_hz;
        if capacity <= 0.0 {
            return Ok(0.0);
        }
        Ok(self.0 as f64 / capacity)
    }
}

impl std::ops::Sub for Cycles {
    type Output = Cycles;
    fn sub(self, other: Cycles) -> Cycles {
        Cycles(self.0.saturating_sub(other.0))
    }
}

// ===== perf_event_open 相关定义 =====
//
// `libc` crate 在 Android 目标上没有导出 `perf_event_attr` 和 PERF_* 常量，
// 这里手动定义最小可用集合。

const PERF_TYPE_HARDWARE: u32 = 0;
const PERF_COUNT_HW_CPU_CYCLES: u64 = 0;

// _IO('$', 0) 和 _IO('$', 3)，见 linux/perf_event.h
const IOC_ENABLE: libc::c_ulong = 0x2400;
const IOC_RESET: libc::c_ulong = 0x2403;

/// 精简版 `perf_event_attr`。
///
/// 只声明前 64 字节的字段（PERF_ATTR_SIZE_VER0），内核只会读
/// `attr.size` 指定的字节数，因此把 size 固定为 64 即可。
#[repr(C)]
struct PerfEventAttr {
    type_: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    /// 位域：disabled:1, inherit:1, pinned:1, exclusive:1,
    /// exclude_user:1, exclude_kernel:1, exclude_hv:1, ...
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
}

const PERF_FLAG_DISABLED: u64 = 1 << 0;
const PERF_FLAG_INHERIT: u64 = 1 << 1;
const PERF_FLAG_EXCLUDE_HV: u64 = 1 << 6;

impl PerfEventAttr {
    fn new() -> Self {
        Self {
            type_: PERF_TYPE_HARDWARE,
            size: std::mem::size_of::<Self>() as u32,
            config: PERF_COUNT_HW_CPU_CYCLES,
            sample_period: 0,
            sample_type: 0,
            read_format: 0,
            flags: PERF_FLAG_DISABLED | PERF_FLAG_INHERIT | PERF_FLAG_EXCLUDE_HV,
            wakeup_events: 0,
            bp_type: 0,
            config1: 0,
        }
    }
}

/// 每个 CPU 核心一个 perf fd。
#[derive(Debug)]
pub struct CyclesReader {
    fds: HashMap<i32, i32>,
}

impl CyclesReader {
    /// 为指定核心列表打开硬件周期计数器。
    pub fn new(cpus: &[usize]) -> Result<Self> {
        let mut fds = HashMap::with_capacity(cpus.len());

        for &cpu in cpus {
            let attr = PerfEventAttr::new();

            let fd = unsafe {
                libc::syscall(
                    libc::SYS_perf_event_open,
                    &attr as *const PerfEventAttr,
                    -1i32,      // pid: -1 = 监控所有进程
                    cpu as i32, // cpu
                    -1i32,      // group_fd
                    0u64,       // flags
                )
            };

            if fd < 0 {
                anyhow::bail!(
                    "perf_event_open failed for cpu {cpu}: {}",
                    std::io::Error::last_os_error()
                );
            }
            fds.insert(cpu as i32, fd as i32);
        }

        Ok(Self { fds })
    }

    /// 启用所有计数器。
    pub fn enable(&self) {
        for &fd in self.fds.values() {
            unsafe {
                libc::ioctl(fd, IOC_RESET, 0);
                libc::ioctl(fd, IOC_ENABLE, 0);
            }
        }
    }

    /// 读取所有核心的当前周期数。
    pub fn read(&self) -> Result<HashMap<i32, Cycles>> {
        let mut map = HashMap::with_capacity(self.fds.len());
        for (&cpu, &fd) in &self.fds {
            let mut value: u64 = 0;
            let n = unsafe {
                libc::read(
                    fd,
                    &mut value as *mut u64 as *mut libc::c_void,
                    std::mem::size_of::<u64>(),
                )
            };
            if n < 0 {
                anyhow::bail!(
                    "read perf fd failed for cpu {cpu}: {}",
                    std::io::Error::last_os_error()
                );
            }
            map.insert(cpu, Cycles(value));
        }
        Ok(map)
    }
}

impl Drop for CyclesReader {
    fn drop(&mut self) {
        for &fd in self.fds.values() {
            unsafe {
                libc::close(fd);
            }
        }
    }
}
