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
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use log::warn;
use nix::sched::CpuSet;

use super::IGNORE_MAP;
use crate::{
    cpu_cycles_reader::{Cycles, CyclesReader},
    file_handler::FileHandler,
};

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
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .context("Invalid file name")?;
        let policy_str = file_name.get(6..).context("Invalid policy format")?;
        let policy = policy_str
            .parse::<i32>()
            .context("Failed to parse policy")?;

        let freqs_content = fs::read_to_string(path.join("scaling_available_frequencies"))
            .context("Failed to read frequencies")?;
        let mut freqs: Vec<isize> = freqs_content
            .split_whitespace()
            .map(|f| f.parse::<isize>().context("Failed to parse frequency"))
            .collect::<Result<_>>()?;
        freqs.sort_unstable();

        let affected_cpus: Vec<usize> = fs::read_to_string(path.join("affected_cpus"))
            .context("Failed to read affected_cpus")?
            .split_whitespace()
            .map(|core| {
                core.parse::<usize>()
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

    // ===== 以下为原版保留：频率写入与校验 =====

    fn verify_freq(&mut self, write_freq: isize) {
        if self.verify_timer.elapsed() >= Duration::from_secs(3) {
            self.verify_timer = Instant::now();

            if let Some(verify_freq) = self.verify_freq {
                let current_freq = self.read_freq();
                let min_acceptable_freq = self
                    .freqs
                    .iter()
                    .take_while(|freq| **freq <= verify_freq)
                    .last()
                    .copied()
                    .unwrap_or(verify_freq);
                let max_acceptable_freq = self
                    .freqs
                    .iter()
                    .find(|freq| **freq >= verify_freq)
                    .copied()
                    .unwrap_or(verify_freq);
                if !(min_acceptable_freq..=max_acceptable_freq).contains(&current_freq) {
                    warn!(
                        "CPU Policy{}: Frequency control does not meet expectations! Expected: {}-{}, Actual: {}",
                        self.policy, min_acceptable_freq, max_acceptable_freq, current_freq
                    );
                }
            }
        }

        self.verify_freq = Some(write_freq);
    }

    fn ignore_write(&self) -> Result<bool> {
        Ok(IGNORE_MAP
            .get()
            .context("IGNORE_MAP not initialized")?
            .get(&self.policy)
            .context("Policy ignore flag not found")?
            .load(Ordering::Acquire))
    }

    fn critical_policy(&self, top_used_cores: CpuSet) -> bool {
        self.affected_cpus
            .iter()
            .any(|core| top_used_cores.is_set(*core).unwrap())
    }

    pub fn write_freq(
        &mut self,
        top_used_cores: CpuSet,
        freq: isize,
        file_handler: &mut FileHandler,
    ) -> Result<()> {
        let min_freq = *self.freqs.first().context("No frequencies available")?;
        let max_freq = *self.freqs.last().context("No frequencies available")?;

        let adjusted_freq = freq.clamp(min_freq, max_freq);
        self.cur_fas_freq = adjusted_freq;

        if !self.ignore_write()? {
            if self.critical_policy(top_used_cores) {
                self.verify_freq(adjusted_freq);
                let adjusted_freq = adjusted_freq.to_string();
                file_handler.write_with_workround(self.max_freq_path(), &adjusted_freq)?;
                file_handler.write_with_workround(self.min_freq_path(), &adjusted_freq)?;
            } else {
                let adjusted_freq = adjusted_freq.to_string();
                let min_freq = self
                    .freqs
                    .first()
                    .context("No frequencies available")?
                    .to_string();
                file_handler.write_with_workround(self.min_freq_path(), &min_freq)?;
                file_handler.write_with_workround(self.max_freq_path(), &adjusted_freq)?;
            }
        }

        Ok(())
    }

    pub fn reset(&mut self, file_handler: &mut FileHandler) -> Result<()> {
        let min_freq = self
            .freqs
            .first()
            .context("No frequencies available")?
            .to_string();
        let max_freq = self
            .freqs
            .last()
            .context("No frequencies available")?
            .to_string();
        self.verify_freq = None;

        file_handler.write_with_workround(self.max_freq_path(), &max_freq)?;
        file_handler.write_with_workround(self.min_freq_path(), &min_freq)?;

        // [MODIFIED] 同时重置采样基准和 EMA，避免旧值污染下一段采样
        let n = self.affected_cpus.len();
        self.last_cycles = vec![None; n];
        self.last_instant = Instant::now();
        self.last_freq_khz = vec![0; n];
        self.demand_smoothed = 0.0;
        self.last_demand_update = Instant::now();

        Ok(())
    }

    pub fn read_freq(&self) -> isize {
        fs::read_to_string(self.path.join("scaling_cur_freq"))
            .context("Failed to read scaling_cur_freq")
            .unwrap()
            .trim()
            .parse::<isize>()
            .context("Failed to parse scaling_cur_freq")
            .unwrap()
    }

    fn max_freq_path(&self) -> PathBuf {
        self.path.join("scaling_max_freq")
    }

    fn min_freq_path(&self) -> PathBuf {
        self.path.join("scaling_min_freq")
    }

    // ===== 以下为硬件周期采样：频率利用率 + 负载需求率 =====

    /// 读取指定核心的当前频率（kHz），用于硬件周期换算
    fn read_freq_khz(cpu_id: usize) -> Result<u64> {
        let path = format!(
            "/sys/devices/system/cpu/cpu{}/cpufreq/scaling_cur_freq",
            cpu_id
        );
        let content = fs::read_to_string(&path).context("Failed to read scaling_cur_freq")?;
        Ok(content.trim().parse::<u64>()?)
    }

    /// 集群最高频（kHz）
    pub fn max_freq(&self) -> isize {
        *self.freqs.last().unwrap_or(&0)
    }

    /// 集群级别的频率利用率（0.0 ~ N.0）
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
    /// 分母为**该集群最高频**，语义为：
    /// “如果 CPU 跑满最高频，当前负载需要占用多少比例的周期”。
    /// 该值只反映负载本身，不受当前频率影响，适合做降频依据。
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

    /// 负载需求率的 EMA 平滑版本（α = 0.25）
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
    pub fn cpu_usage(&mut self) -> Result<f64> {
        let usage = self.cluster_usage()?;
        Ok(usage.min(1.0))
    }

    /// 首次采样（建立 CyclesReader 基准，不参与调度决策）
    pub fn init_sample(&mut self) -> Result<()> {
        let _ = self.cluster_usage()?;
        Ok(())
    }

    /// 刷新 CPU 利用率数据（旧接口兼容，采样基准已在 cluster_usage 内部更新）
    pub fn refresh_usage(&mut self) {
        // no-op
    }

    pub fn cur_freq(&self) -> isize {
        self.cur_fas_freq
    }

    pub fn set_cur_freq(&mut self, freq: isize) {
        self.cur_fas_freq = freq;
    }

    pub fn affected_cpus(&self) -> &[usize] {
        &self.affected_cpus
    }
}
