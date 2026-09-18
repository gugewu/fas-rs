// Copyright 2024-2025, dependabot[bot], reigadegr, shadow3aaa
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
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use cpu_cycles_reader::{Cycles, CyclesReader};
use log::warn;
use nix::sched::CpuSet;

use super::IGNORE_MAP;
use crate::file_handler::FileHandler;

#[derive(Debug)]
pub struct Info {
    pub policy: i32,
    path: PathBuf,
    affected_cpus: Vec<usize>,
    pub cur_fas_freq: isize,
    pub freqs: Vec<isize>,
    verify_freq: Option<isize>,
    verify_timer: Instant,

    // === 硬件周期读取器（替代 sysinfo） ===
    cycles_reader: CyclesReader,

    // === 采样基准 ===
    last_cycles: Vec<Option<Cycles>>,
    last_instant: Instant,
    last_freq_khz: Vec<u64>,

    // === 复用缓冲区 ===
    freq_buf: Vec<u64>,

    // [MODIFIED] 负载需求率 EMA 平滑状态
    demand_smoothed: f64,
    last_demand_update: Instant,
}

impl Info {
    pub fn new<P>(path: P) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref().to_path_buf();
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .context("Invalid file name")?;
        let policy_str = file_name.get(6..).context("Invalid policy format")?;
        let policy = policy_str
            .parse()
            .context("Failed to parse policy")?;

        let freqs_content = fs::read_to_string(path.join("scaling_available_frequencies"))
            .context("Failed to read frequencies")?;
        let mut freqs: Vec<isize> = freqs_content
            .split_whitespace()
            .map(|f| f.parse().context("Failed to parse frequency"))
            .collect::<Result<Vec<isize>>>()?;
        freqs.sort_unstable();

        let affected_cpus: Vec<usize> = fs::read_to_string(path.join("affected_cpus"))
            .context("Failed to read affected_cpus")?
            .split_whitespace()
            .map(|core| {
                core.parse()
                    .context("Failed to parse core")
                    .unwrap()
            })
            .collect();

        // === 初始化 CyclesReader（替代 sysinfo） ===
        let reader = CyclesReader::new(&affected_cpus)
            .context("Failed to create CyclesReader")?;
        reader.enable();

        let n = affected_cpus.len();

        Ok(Self {
            policy,
            path,
            affected_cpus,
            cur_fas_freq: *freqs.last().context("No frequencies available")?,
            freqs,
            verify_freq: None,
            verify_timer: Instant::now(),
            cycles_reader: reader,
            last_cycles: vec![None; n],
            last_instant: Instant::now(),
            last_freq_khz: vec![0; n],
            freq_buf: Vec::with_capacity(n),
            // [MODIFIED]
            demand_smoothed: 0.0,
            last_demand_update: Instant::now(),
        })
    }

    /// 读取指定核心的当前频率（kHz）
    fn read_freq_khz(cpu_id: usize) -> Result<u64> {
        let path = format!(
            "/sys/devices/system/cpu/cpu{}/cpufreq/scaling_cur_freq",
            cpu_id
        );
        let content = fs::read_to_string(&path).context("Failed to read scaling_cur_freq")?;
        Ok(content.trim().parse::<u64>()?)
    }

    /// 集群最高频（kHz）
    ///
    /// [MODIFIED] 新增：`freqs` 已在 `new()` 中升序排序，last 即最高频。
    pub fn max_freq(&self) -> isize {
        *self.freqs.last().unwrap_or(&0)
    }

    /// 批量采样，返回集群级别的频率利用率（0.0 ~ N.0）
    ///
    /// 分母为**当前频率**，反映“当前频率被用掉多少”。
    /// 该值会因降频而虚高，仅适合诊断，不适合做降频依据。
    pub fn cluster_usage(&mut self) -> Result<f64> {
        let now = Instant::now();
        let duration = now.duration_since(self.last_instant);

        let cycles_map = self
            .cycles_reader
            .read()
            .context("Failed to read cycles")?;

        self.freq_buf.clear();
        for &cpu_id in &self.affected_cpus {
            self.freq_buf.push(Self::read_freq_khz(cpu_id)?);
        }

        let mut total_usage = 0.0f64;
        let mut valid_count = 0usize;

        for (i, &cpu_id) in self.affected_cpus.iter().enumerate() {
            let now_cycles = cycles_map
                .get(&(cpu_id as i32))
                .copied()
                .context("CPU id not found in cycles map")?;

            if let Some(last) = self.last_cycles[i] {
                let diff = now_cycles - last;
                let freq_cycles = Cycles::from_khz(self.freq_buf[i]);
                if let Ok(usage) = diff.as_usage(duration, freq_cycles) {
                    total_usage += usage;
                    valid_count += 1;
                }
            }
            self.last_cycles[i] = Some(now_cycles);
        }

        self.last_instant = now;

        if valid_count == 0 {
            return Ok(0.0);
        }
        Ok(total_usage / valid_count as f64)
    }

    /// 集群级别的**负载需求率**（0.0 ~ N.0）
    ///
    /// [MODIFIED] 新增。
    ///
    /// 分母为**该集群最高频**，语义为：
    /// “如果 CPU 跑满最高频，当前负载需要占用多少比例的周期”。
    ///
    /// 与 `cluster_usage()` 的区别：
    /// - `cluster_usage` 用当前频率做分母 → 结果受当前频率影响，降频后会虚高
    /// - `load_demand_rate` 用最高频做分母 → 结果只反映负载本身，不受频率影响
    ///
    /// 用途：作为降频决策的输入。`demand < 1.0` 说明当前负载
    /// 不需要跑满最高频，有余量可降。
    ///
    /// 注意：本方法与 `cluster_usage()` 共享采样基准（`last_cycles` /
    /// `last_instant`），一次控制周期内只应调用其中一个，避免重复采样
    /// 导致基准被消耗两次。
    pub fn load_demand_rate(&mut self) -> Result<f64> {
        let now = Instant::now();
        let duration = now.duration_since(self.last_instant);

        let cycles_map = self
            .cycles_reader
            .read()
            .context("Failed to read cycles")?;

        self.freq_buf.clear();
        for &cpu_id in &self.affected_cpus {
            self.freq_buf.push(Self::read_freq_khz(cpu_id)?);
        }

        // 用该集群最高频作为“满负载”参考
        let max_freq_khz = self.max_freq().max(0) as u64;
        let max_freq_cycles = Cycles::from_khz(max_freq_khz);

        let mut total_demand = 0.0f64;
        let mut valid_count = 0usize;

        for (i, &cpu_id) in self.affected_cpus.iter().enumerate() {
            let now_cycles = cycles_map
                .get(&(cpu_id as i32))
                .copied()
                .context("CPU id not found in cycles map")?;

            if let Some(last) = self.last_cycles[i] {
                let diff = now_cycles - last;
                if let Ok(demand) = diff.as_usage(duration, max_freq_cycles) {
                    total_demand += demand;
                    valid_count += 1;
                }
            }
            self.last_cycles[i] = Some(now_cycles);
        }

        self.last_instant = now;

        if valid_count == 0 {
            return Ok(0.0);
        }
        Ok(total_demand / valid_count as f64)
    }

    /// 负载需求率的 EMA 平滑版本
    ///
    /// [MODIFIED] 新增。
    ///
    /// 硬件周期采样短时波动较大，直接用于调频会引起频率抖动。
    /// 用指数移动平均（α = 0.25）平滑，让调频决策基于趋势而非瞬时值。
    ///
    /// 超过 100ms 未更新时直接用原始值重置，避免长时间空窗后 EMA
    /// 被旧值拖慢。
    pub fn load_demand_rate_smoothed(&mut self) -> Result<f64> {
        let raw = self.load_demand_rate()?;

        const EMA_ALPHA: f64 = 0.25;
        const RESET_WINDOW: Duration = Duration::from_millis(100);

        if self.last_demand_update.elapsed() > RESET_WINDOW {
            self.demand_smoothed = raw;
        } else {
            self.demand_smoothed = EMA_ALPHA * raw + (1.0 - EMA_ALPHA) * self.demand_smoothed;
        }
        self.last_demand_update = Instant::now();

        Ok(self.demand_smoothed)
    }

    /// 兼容旧接口：返回 0.0 ~ 1.0 的归一化值
    ///
    /// 内部调用 `cluster_usage()` 后做 clamp，保持与旧版
    /// `cpu_usage()` 的量纲一致（0.0 ~ 1.0）。
    pub fn cpu_usage(&mut self) -> Result<f64> {
        let usage = self.cluster_usage()?;
        Ok(usage.min(1.0))
    }

    /// 首次采样（建立 CyclesReader 基准，不参与调度决策）
    pub fn init_sample(&mut self) -> Result<()> {
        let _ = self.cluster_usage()?;
        Ok(())
    }

    /// 刷新 CPU 利用率数据（旧接口兼容，现在由 cluster_usage 内部处理）
    pub fn refresh_usage(&mut self) {
        // no-op：采样基准已在 cluster_usage 内部更新
    }

    /// 重置采样基准和 EMA 状态。
    ///
    /// [MODIFIED] 新增：`cpu_common/mod.rs` 会调用此方法，
    /// 在游戏切换、模式切换或长时间无帧后重置采样基准，
    /// 避免旧的周期基准和 EMA 值污染新一段采样。
    pub fn reset(&mut self, _file_handler: &mut FileHandler) -> Result<()> {
        let n = self.affected_cpus.len();
        self.last_cycles = vec![None; n];
        self.last_instant = Instant::now();
        self.last_freq_khz = vec![0; n];
        self.demand_smoothed = 0.0;
        self.last_demand_update = Instant::now();
        Ok(())
    }

    pub fn cur_freq(&self) -> isize {
        self.cur_fas_freq
    }

    pub fn set_cur_freq(&mut self, freq: isize) {
        self.cur_fas_freq = freq;
    }

    pub fn verify_freq(&mut self, freq: isize) -> bool {
        if let Some(verify) = self.verify_freq {
            if verify == freq && self.verify_timer.elapsed().as_millis() < 500 {
                return true;
            }
        }
        self.verify_freq = Some(freq);
        self.verify_timer = Instant::now();
        false
    }

    pub fn affected_cpus(&self) -> &[usize] {
        &self.affected_cpus
    }
}
