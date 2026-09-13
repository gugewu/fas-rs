// Copyright 2024-2025, dependabot[bot], reigadegr, shadow3aaa
//
// This file is part of fas-rs.
//
// fas-rs is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free
// Software Foundation, either version 3 of the License, or (at your
// option) any later version.
//
// fas-rs is distributed in the hope that it will be useful, but WITHOUT ANY
// WARRANTY; without even the implied warranty of MERCHANTABILITY or
// FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for
// more details.
//
// You should have received a copy of the GNU General Public License along
// with fas-rs. If not, see <https://www.gnu.org/licenses/>.

use std::{
    fs,
    path::{Path, PathBuf},
    time::Instant,
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

    /// 批量采样，返回集群级别的频率利用率（0.0 ~ N.0）
    ///
    /// 与旧版 sysinfo 的 `cpu_usage()` 不同，此方法基于硬件性能计数器
    /// 读取真实 CPU 周期数，结合当前频率计算“频率利用率”，
    /// 消除了降频导致时间片占用率虚高的问题。
    pub fn cluster_usage(&mut self) -> Result<f64> {
        let now = Instant::now();
        let duration = now.duration_since(self.last_instant);

        // 一次性读取所有核心的周期数
        let cycles_map = self
            .cycles_reader
            .read()
            .context("Failed to read cycles")?;

        // 读取所有核心的频率
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
