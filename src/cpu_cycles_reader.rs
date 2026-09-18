//! 本地实现的 CPU 硬件周期读取器。
//!
//! 用 `perf_event_open` 直接读取每个核心的 `PERF_COUNT_HW_CPU_CYCLES`
//! 硬件计数器，语义为“该核心在采样窗口内实际执行的周期数”。
//!
//! 替代 sysinfo 的时间片占用率，避免降频后时间片占比虚高的问题。

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use libc;

/// 一段周期数。包装 u64，提供减法和利用率换算。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cycles(pub u64);

impl Cycles {
    /// 从频率（kHz）构造一个“满负载参考周期数”。
    ///
    /// 语义：如果 CPU 以该频率跑满整个采样窗口，应执行的周期数。
    pub fn from_khz(khz: u64) -> Self {
        Cycles(khz)
    }

    /// 计算利用率：实际周期数 / 满负载参考周期数。
    ///
    /// - `duration`：采样窗口长度
    /// - `freq_cycles`：由 `Cycles::from_khz` 构造的参考值
    ///
    /// 返回值为比值，可能大于 1.0（实际频率高于参考频率时）。
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

/// 每个 CPU 核心一个 perf fd。
pub struct CyclesReader {
    fds: HashMap<i32, i32>,
}

impl CyclesReader {
    /// 为指定核心列表打开硬件周期计数器。
    pub fn new(cpus: &[usize]) -> Result<Self> {
        let mut fds = HashMap::with_capacity(cpus.len());

        for &cpu in cpus {
            let mut attr: libc::perf_event_attr = unsafe { std::mem::zeroed() };
            attr.type_ = libc::PERF_TYPE_HARDWARE;
            attr.size = std::mem::size_of::<libc::perf_event_attr>() as u32;
            attr.config = libc::PERF_COUNT_HW_CPU_CYCLES;
            attr.disabled = 1;
            attr.inherit = 1;
            attr.exclude_kernel = 0;
            attr.exclude_hv = 1;

            let fd = unsafe {
                libc::syscall(
                    libc::SYS_perf_event_open,
                    &attr as *const libc::perf_event_attr,
                    -1i32,          // pid: -1 = 监控所有进程
                    cpu as i32,     // cpu
                    -1i32,          // group_fd
                    0u64,           // flags
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
                libc::ioctl(fd, libc::PERF_EVENT_IOC_RESET, 0);
                libc::ioctl(fd, libc::PERF_EVENT_IOC_ENABLE, 0);
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
