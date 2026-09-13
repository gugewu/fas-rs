// Copyright 2024-2025, shadow3aaa
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

use std::collections::HashMap;

use anyhow::Result;

use super::cpu_info::Info;

/// 频率利用率平滑器
///
/// 对每个 policy 的原始频率利用率做指数移动平均（EMA），
/// 避免单次采样噪声导致调度器抖动。
struct FrequencyUsageCalculator {
    last_usage: HashMap<i32, f64>,
    smoothing: f64,
}

impl FrequencyUsageCalculator {
    fn new(smoothing: f64) -> Self {
        Self {
            last_usage: HashMap::new(),
            smoothing,
        }
    }

    fn update(&mut self, policy: i32, raw_usage: f64) -> f64 {
        let smoothed = match self.last_usage.get(&policy) {
            Some(&last) => self.smoothing * last + (1.0 - self.smoothing) * raw_usage,
            None => raw_usage,
        };
        self.last_usage.insert(policy, smoothed);
        smoothed
    }
}

#[derive(Debug)]
pub struct WeightedCalculator {
    /// policy -> 平滑后的频率利用率
    cache: HashMap<i32, f64>,

    /// 频率利用率计算器
    freq_calc: FrequencyUsageCalculator,

    /// 是否已完成首次采样（建立 CyclesReader 基准）
    first_sample_done: bool,
}

impl WeightedCalculator {
    pub fn new(policys: &[Info]) -> Self {
        let mut cache = HashMap::new();
        for info in policys {
            cache.insert(info.policy, 0.0);
        }

        Self {
            cache,
            freq_calc: FrequencyUsageCalculator::new(0.3),
            first_sample_done: false,
        }
    }

    /// 更新权重（频率利用率）
    ///
    /// 返回 policy -> 频率利用率（0.0 ~ N.0，通常 >1.0 是正常的）
    pub fn update(&mut self, infos: &mut [Info]) -> Result<&HashMap<i32, f64>> {
        // 首次采样仅用于建立 CyclesReader 基准，不参与调度决策
        if !self.first_sample_done {
            for info in infos.iter_mut() {
                let _ = info.cluster_usage()?;
            }
            self.first_sample_done = true;
            self.cache.clear();
            for info in infos {
                self.cache.insert(info.policy, 0.0);
            }
            return Ok(&self.cache);
        }

        for info in infos.iter_mut() {
            let raw_usage = info.cluster_usage()?;
            let smoothed = self.freq_calc.update(info.policy, raw_usage);
            self.cache.insert(info.policy, smoothed);
        }

        Ok(&self.cache)
    }

    /// 获取指定 policy 的当前频率利用率
    pub fn get(&self, policy: i32) -> Option<f64> {
        self.cache.get(&policy).copied()
    }

    /// 重置所有状态
    pub fn reset(&mut self) {
        self.freq_calc.last_usage.clear();
        self.first_sample_done = false;
    }
}
