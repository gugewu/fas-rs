// Copyright 2025-2025, shadow3, shadow3aaa
//
// This file is part of fas-rs.
//
// fas-rs is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free
// Software Foundation, either version 3 of the License, or (at your option)
// any later version.
//
// fas-rs is distributed in the hope that it will be useful, but WITHOUT ANY
// WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS
// FOR A PARTICULAR PURPOSE. See the GNU General Public License for more
// details.
//
// You should have received a copy of the GNU General Public License along
// with fas-rs. If not, see <https://www.gnu.org/licenses/>.
use std::{
    cmp,
    collections::{HashMap, hash_map::Entry},
    fs,
    time::{Duration, Instant},
};

use anyhow::Result;
use libc::{_SC_CLK_TCK, sysconf};

#[derive(Debug, Clone, Copy)]
struct UsageTracker {
    pid: i32,
    tid: i32,
    last_cputime: u64,
    read_timer: Instant,
    current_usage: f64,
}

impl UsageTracker {
    fn new(pid: i32, tid: i32) -> Result<Self> {
        Ok(Self {
            pid,
            tid,
            last_cputime: get_thread_cpu_time(pid, tid)?,
            read_timer: Instant::now(),
            current_usage: 0.0,
        })
    }

    fn try_calculate(&mut self) -> Result<f64> {
        let tick_per_sec = unsafe { sysconf(_SC_CLK_TCK) };
        let new_cputime = get_thread_cpu_time(self.pid, self.tid)?;
        let elapsed_ticks = self.read_timer.elapsed().as_secs_f64() * tick_per_sec as f64;
        self.read_timer = Instant::now();
        let cputime_slice = new_cputime - self.last_cputime;
        self.last_cputime = new_cputime;
        self.current_usage = cputime_slice as f64 / elapsed_ticks;
        Ok(self.current_usage)
    }
}

/// 单线程 schedstat 的一次差分快照。
///
/// `exec_delta_ns`：采样窗口内该线程在 CPU 上执行的时间。
/// `delay_delta_ns`：采样窗口内该线程在 runqueue 里等 CPU 的时间。
/// `window_ns`：两次采样的时间差。
///
/// 一个线程在同一时刻只能处于三种状态之一：在 CPU 上跑、在 runqueue 里等、
/// 睡眠/阻塞。所以 `exec + delay <= window`，差值就是“看不见的等待”
/// （锁等待、IO 等待、GPU fence 等待）。
#[derive(Debug, Clone, Copy)]
pub struct SchedstatSnapshot {
    pub tid: i32,
    pub exec_delta_ns: u64,
    pub delay_delta_ns: u64,
    pub window_ns: u64,
}

impl SchedstatSnapshot {
    /// 看不见的等待占采样窗口的比例（0.0 ~ 1.0）。
    pub fn unseen_ratio(&self) -> f64 {
        if self.window_ns == 0 {
            return 0.0;
        }
        let covered = self.exec_delta_ns.saturating_add(self.delay_delta_ns);
        let ratio = covered as f64 / self.window_ns as f64;
        (1.0 - ratio).clamp(0.0, 1.0)
    }
}

#[derive(Debug)]
pub struct ProcessMonitor {
    current_pid: Option<i32>,
    all_trackers: HashMap<i32, UsageTracker>,
    top_trackers: HashMap<i32, UsageTracker>,
    last_full_update: Instant,
    last_update: Instant,
    /// 上一次 schedstat 快照：(tid, exec_ns, delay_ns, 采样时刻)
    last_schedstat: Option<(i32, u64, u64, Instant)>,
}

impl ProcessMonitor {
    pub fn new() -> Self {
        Self {
            current_pid: None,
            all_trackers: HashMap::new(),
            top_trackers: HashMap::new(),
            last_full_update: Instant::now(),
            last_update: Instant::now(),
            last_schedstat: None,
        }
    }

    pub fn set_pid(&mut self, pid: Option<i32>) {
        if self.current_pid != pid {
            self.current_pid = pid;
            self.all_trackers.clear();
            self.top_trackers.clear();
            self.last_full_update = Instant::now();
            self.last_update = Instant::now();
            self.last_schedstat = None;
        }
    }

    pub fn update(&mut self) -> Option<f64> {
        if self.last_update.elapsed() < Duration::from_millis(300) {
            return None;
        }

        self.last_update = Instant::now();
        let pid = self.current_pid?;

        if self.last_full_update.elapsed() >= Duration::from_secs(1) {
            self.update_thread_list(pid);
            self.last_full_update = Instant::now();
        }

        let mut util_max: f64 = 0.0;
        for tracker in self.top_trackers.values_mut() {
            if let Ok(usage) = tracker.try_calculate() {
                util_max = util_max.max(usage);
            }
        }

        Some(util_max)
    }

    fn update_thread_list(&mut self, pid: i32) {
        if let Ok(threads) = get_thread_ids(pid) {
            self.all_trackers = threads
                .iter()
                .copied()
                .filter_map(|tid| {
                    Some((
                        tid,
                        match self.all_trackers.entry(tid) {
                            Entry::Occupied(o) => o.remove(),
                            Entry::Vacant(_) => UsageTracker::new(pid, tid).ok()?,
                        },
                    ))
                })
                .collect();

            let mut top_threads: Vec<_> = self
                .all_trackers
                .iter()
                .filter_map(|(tid, tracker)| Some((*tid, tracker.clone().try_calculate().ok()?)))
                .collect();

            top_threads.sort_by(|(_, a), (_, b)| b.partial_cmp(a).unwrap_or(cmp::Ordering::Equal));
            top_threads.truncate(8);

            self.top_trackers = top_threads
                .into_iter()
                .filter_map(|(tid, _)| match self.top_trackers.entry(tid) {
                    Entry::Occupied(o) => Some((tid, o.remove())),
                    Entry::Vacant(_) => Some((tid, UsageTracker::new(pid, tid).ok()?)),
                })
                .collect();
        }
    }

    pub fn top_threads(&self) -> impl Iterator<Item = i32> {
        self.top_trackers.keys().copied()
    }

    /// 返回当前正在监控的进程 pid。无游戏时为 `None`。
    pub fn current_pid(&self) -> Option<i32> {
        self.current_pid
    }

    /// 返回 top 8 线程最近一次采样的 CPU 占用率（tid -> usage）。
    ///
    /// `current_usage` 由 `update()` 里的 `try_calculate()` 更新，
    /// 每 300ms 一次。上层若在 `update_util_max()` 之后调用，
    /// 拿到的就是最近一个采样窗口的数据。
    pub fn top_thread_usage(&self) -> HashMap<i32, f64> {
        self.top_trackers
            .iter()
            .map(|(&tid, tracker)| (tid, tracker.current_usage))
            .collect()
    }

    /// 对“当前 usage 最高的 top 线程”做一次 schedstat 差分。
    ///
    /// 返回 `None` 的情况：
    /// - 没有游戏 pid
    /// - top_trackers 为空
    /// - 首次采样（没有上一次的值可比）
    /// - top 线程切换了（tid 变了，差分无意义）
    /// - schedstat 读取失败
    ///
    /// 返回值里的 `unseen_ratio()` 就是“这个线程在采样窗口内，
    /// 有多少比例的时间既不在 CPU 上跑、也不在 runqueue 里等”。
    /// 这部分时间对应锁等待、IO 等待、GPU fence 等待。
    pub fn schedstat_delta(&mut self) -> Option<SchedstatSnapshot> {
        let pid = self.current_pid?;
        let tid = self
            .top_trackers
            .iter()
            .max_by(|a, b| {
                a.1.current_usage
                    .partial_cmp(&b.1.current_usage)
                    .unwrap_or(cmp::Ordering::Equal)
            })
            .map(|(tid, _)| *tid)?;

        let path = format!("/proc/{pid}/task/{tid}/schedstat");
        let content = fs::read_to_string(&path).ok()?;
        let mut parts = content.split_whitespace();
        let exec_ns = parts.next()?.parse::<u64>().ok()?;
        let delay_ns = parts.next()?.parse::<u64>().ok()?;

        let now = Instant::now();
        let result = match self.last_schedstat {
            Some((last_tid, last_exec, last_delay, last_time)) if last_tid == tid => {
                let window_ns = now.duration_since(last_time).as_nanos() as u64;
                if window_ns == 0 {
                    None
                } else {
                    Some(SchedstatSnapshot {
                        tid,
                        exec_delta_ns: exec_ns.saturating_sub(last_exec),
                        delay_delta_ns: delay_ns.saturating_sub(last_delay),
                        window_ns,
                    })
                }
            }
            // 首次采样，或 top 线程切换了，不产生差分
            _ => None,
        };

        self.last_schedstat = Some((tid, exec_ns, delay_ns, now));
        result
    }
}

fn get_thread_ids(pid: i32) -> Result<Vec<i32>> {
    let proc_path = format!("/proc/{pid}/task");
    Ok(fs::read_dir(proc_path)?
        .filter_map(|entry| {
            entry
                .ok()
                .and_then(|e| e.file_name().to_string_lossy().parse::<i32>().ok())
        })
        .collect())
}

fn get_thread_cpu_time(pid: i32, tid: i32) -> Result<u64> {
    let stat_path = format!("/proc/{pid}/task/{tid}/stat");
    let stat_content = fs::read_to_string(stat_path)?;
    let parts: Vec<&str> = stat_content.split_whitespace().collect();
    let utime = parts[13].parse::<u64>().unwrap_or(0);
    let stime = parts[14].parse::<u64>().unwrap_or(0);
    Ok(utime + stime)
}
