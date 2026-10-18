// Copyright 2023-2025, dependabot[bot], shadow3, shadow3aaa
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

mod cpu_info;
pub mod extra_policy;
mod process_monitor;

use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::{OnceLock, atomic::AtomicBool},
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
#[cfg(debug_assertions)]
use log::debug;
use log::warn;
use nix::{
    sched::{CpuSet, sched_getaffinity},
    unistd::Pid,
};
use parking_lot::Mutex;
use process_monitor::ProcessMonitor;

use crate::{
    Extension,
    api::{trigger_init_cpu_freq, trigger_reset_cpu_freq},
    file_handler::FileHandler,
};
use cpu_info::Info;
use extra_policy::ExtraPolicy;

pub static EXTRA_POLICY_MAP: OnceLock<HashMap<i32, Mutex<ExtraPolicy>>> = OnceLock::new();
pub static IGNORE_MAP: OnceLock<HashMap<i32, AtomicBool>> = OnceLock::new();

/// EMA 平滑系数。越小越稳、越大越跟当前负载。
const WEIGHT_EMA_ALPHA: f64 = 0.3;

/// 每个集群权重下限，防止某个集群被完全饿死。
const MIN_CLUSTER_WEIGHT: f64 = 0.15;

#[derive(Debug)]
pub struct Controller {
    max_freq: isize,
    cpu_infos: Vec<Info>,
    file_handler: FileHandler,
    process_monitor: ProcessMonitor,
    util_max: Option<f64>,
    /// CPU ID -> policy 映射，启动时构建一次。
    cpu_to_policy: HashMap<i32, i32>,
    /// 平滑后的各 policy 权重，跨控制周期保留。
    smoothed_weights: HashMap<i32, f64>,
}

impl Controller {
    pub fn new() -> Result<Self> {
        let mut cpu_infos = Self::load_cpu_infos()?;
        cpu_infos.sort_by_key(|cpu| cpu.policy);

        EXTRA_POLICY_MAP.get_or_init(|| {
            cpu_infos
                .iter()
                .map(|cpu| (cpu.policy, Mutex::new(ExtraPolicy::None)))
                .collect()
        });
        IGNORE_MAP.get_or_init(|| {
            cpu_infos
                .iter()
                .map(|cpu| (cpu.policy, AtomicBool::new(false)))
                .collect()
        });

        #[cfg(debug_assertions)]
        debug!("cpu infos: {cpu_infos:?}");

        let max_freq = cpu_infos
            .iter()
            .flat_map(|info| info.freqs.iter())
            .max()
            .copied()
            .unwrap_or(0);

        let cpu_to_policy = Self::build_cpu_to_policy();

        #[cfg(debug_assertions)]
        debug!("cpu_to_policy: {cpu_to_policy:?}");

        Ok(Self {
            max_freq,
            cpu_infos,
            file_handler: FileHandler::new(),
            process_monitor: ProcessMonitor::new(),
            util_max: None,
            cpu_to_policy,
            smoothed_weights: HashMap::new(),
        })
    }

    fn load_cpu_infos() -> Result<Vec<Info>> {
        let mut cpu_infos = Vec::new();

        for entry in fs::read_dir("/sys/devices/system/cpu/cpufreq")? {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(e) => {
                    warn!("Failed to read entry: {e:?}");
                    continue;
                }
            };

            if !path.is_dir() {
                continue;
            }

            let Some(filename) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };

            if !filename.starts_with("policy") {
                continue;
            }

            cpu_infos.push(Self::retry_load_info(&path));
        }

        Ok(cpu_infos)
    }

    fn retry_load_info(path: &Path) -> Info {
        loop {
            match Info::new(path) {
                Ok(info) => return info,
                Err(e) => {
                    warn!(
                        "Failed to read cpu info from: {}, reason: {e:?}",
                        path.display()
                    );
                    warn!("Retrying...");
                    thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }

    /// 从 sysfs 构建 CPU ID -> policy 映射。
    ///
    /// 数据源：`/sys/devices/system/cpu/cpufreq/policy<N>/affected_cpus`，
    /// 内容形如 `"0 1 2 3"`，即该 policy 管理的 CPU 列表。
    ///
    /// 若文件不存在或解析失败，跳过该 policy。全部失败时返回空表，
    /// 上层 `cluster_weights` 会检测到空表并回退等权。
    fn build_cpu_to_policy() -> HashMap<i32, i32> {
        let mut map = HashMap::new();
        let Ok(entries) = fs::read_dir("/sys/devices/system/cpu/cpufreq") else {
            return map;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !name.starts_with("policy") {
                continue;
            }
            let Ok(policy) = name["policy".len()..].parse::<i32>() else {
                continue;
            };
            let Ok(content) = fs::read_to_string(path.join("affected_cpus")) else {
                continue;
            };
            for cpu_str in content.split_whitespace() {
                if let Ok(cpu) = cpu_str.parse::<i32>() {
                    map.insert(cpu, policy);
                }
            }
        }
        map
    }

    pub fn init_game(&mut self, pid: i32, extension: &Extension) {
        trigger_init_cpu_freq(extension);
        self.reset_all_cpu_freq();
        self.process_monitor.set_pid(Some(pid));
        self.util_max = None;
        self.smoothed_weights.clear();
    }

    pub fn init_default(&mut self, extension: &Extension) {
        trigger_reset_cpu_freq(extension);
        self.reset_all_cpu_freq();
        self.process_monitor.set_pid(None);
        self.util_max = None;
        self.smoothed_weights.clear();
    }

    pub fn fas_update_freq(&mut self, control: isize, is_janked: bool) {
        #[cfg(debug_assertions)]
        debug!("change freq: {control}");

        let fas_freqs = self.compute_target_frequencies(control, is_janked);
        let sorted_policies = self.sort_policies_topologically();
        let fas_freqs = Self::apply_absolute_constraints(fas_freqs, &sorted_policies);
        let fas_freqs = Self::apply_relative_constraints(fas_freqs, &sorted_policies);
        let top_used_cores = self.top_used_cores().unwrap_or_else(|| {
            let mut all_cores = CpuSet::new();
            for core in 0..num_cpus::get() {
                all_cores.set(core).unwrap();
            }
            all_cores
        });

        if no_extra_policy() {
            let fas_freq_max = fas_freqs.values().max().copied().unwrap();
            for cpu in &mut self.cpu_infos {
                if let Some(freq) = fas_freqs.get(&cpu.policy).copied() {
                    let freq = freq.clamp(
                        fas_freq_max.saturating_sub(100_000),
                        fas_freq_max.saturating_add(100_000),
                    );
                    let _ = cpu.write_freq(top_used_cores, freq, &mut self.file_handler);
                }
            }
        } else {
            for cpu in &mut self.cpu_infos {
                if let Some(freq) = fas_freqs.get(&cpu.policy).copied() {
                    let _ = cpu.write_freq(top_used_cores, freq, &mut self.file_handler);
                }
            }
        }
    }

    fn update_util_max(&mut self) {
        if let Some(util_max) = self.process_monitor.update() {
            self.util_max = Some(util_max);
        }
    }

    /// 计算各 policy 的归一化权重（sum ≈ 1.0）。
    ///
    /// 算法：
    /// 1. 从 `process_monitor` 拿 top 8 线程的 CPU 占用率；
    /// 2. 逐个读线程所在 CPU，映射到 policy，按 policy 累加 usage；
    /// 3. 归一化得到 raw_weights；
    /// 4. 加 `MIN_CLUSTER_WEIGHT` 保底，再重新归一化；
    /// 5. EMA 平滑（`WEIGHT_EMA_ALPHA`），跨控制周期保留；
    /// 6. 平滑后再归一化一次，保证 sum = 1.0。
    ///
    /// 所有失败路径（无 pid、top 线程为空、usage 全 0、`cpu_to_policy`
    /// 为空、最终总和为 0）统一回退等权，保证不会比改造前更差。
    fn cluster_weights(&mut self) -> HashMap<i32, f64> {
        let n = self.cpu_infos.len() as f64;
        let policies: Vec<i32> = self.cpu_infos.iter().map(|c| c.policy).collect();

        if n <= 0.0 || self.cpu_to_policy.is_empty() {
            return policies.iter().map(|&p| (p, 1.0 / n.max(1.0))).collect();
        }

        let equal_weights = || -> HashMap<i32, f64> {
            policies.iter().map(|&p| (p, 1.0 / n)).collect()
        };

        let Some(pid) = self.process_monitor.current_pid() else {
            return equal_weights();
        };

        let thread_usage = self.process_monitor.top_thread_usage();
        if thread_usage.is_empty() {
            return equal_weights();
        }

        // 按 policy 聚合 usage
        let mut raw: HashMap<i32, f64> = HashMap::new();
        for (tid, usage) in thread_usage {
            if usage <= 0.0 {
                continue;
            }
            let Some(cpu) = read_thread_cpu(pid, tid) else {
                continue;
            };
            let Some(&policy) = self.cpu_to_policy.get(&cpu) else {
                continue;
            };
            *raw.entry(policy).or_insert(0.0) += usage;
        }

        let total: f64 = raw.values().sum();
        if total <= 0.0 {
            return equal_weights();
        }

        // 归一化 + 保底 + 重新归一化
        let mut raw_weights: HashMap<i32, f64> = policies
            .iter()
            .map(|&p| (p, raw.get(&p).copied().unwrap_or(0.0) / total))
            .collect();

        for w in raw_weights.values_mut() {
            *w = w.max(MIN_CLUSTER_WEIGHT);
        }

        let sum: f64 = raw_weights.values().sum();
        for w in raw_weights.values_mut() {
            *w /= sum;
        }

        // EMA 平滑（先构造新表，避免借用冲突）
        let mut new_smoothed: HashMap<i32, f64> = HashMap::new();
        for &p in &policies {
            let new_w = raw_weights.get(&p).copied().unwrap_or(1.0 / n);
            let old_w = self
                .smoothed_weights
                .get(&p)
                .copied()
                .unwrap_or(1.0 / n);
            let smoothed = WEIGHT_EMA_ALPHA * new_w + (1.0 - WEIGHT_EMA_ALPHA) * old_w;
            new_smoothed.insert(p, smoothed);
        }

        // 平滑后再归一化
        let sum: f64 = new_smoothed.values().sum();
        if sum <= 0.0 {
            return equal_weights();
        }
        for w in new_smoothed.values_mut() {
            *w /= sum;
        }

        self.smoothed_weights = new_smoothed.clone();

        #[cfg(debug_assertions)]
        debug!("cluster_weights: {new_smoothed:?}");

        new_smoothed
    }

    /// 把全局 `control` 按权重拆到各 policy。
    ///
    /// 乘 `n` 保证：权重相等时 `control × (1/n) × n = control`，
    /// 平均增量与改造前完全一致。
    fn distribute_to_policy(
        control: isize,
        weights: &HashMap<i32, f64>,
        n: f64,
    ) -> HashMap<i32, isize> {
        weights
            .iter()
            .map(|(&policy, &w)| (policy, (control as f64 * w * n) as isize))
            .collect()
    }

    fn compute_target_frequencies(
        &mut self,
        control: isize,
        is_janked: bool,
    ) -> HashMap<i32, isize> {
        let cur_fas_freq_max = self
            .cpu_infos
            .iter()
            .map(|cpu| cpu.cur_fas_freq)
            .max()
            .unwrap_or_default();
        let cur_freq_max = self
            .cpu_infos
            .iter()
            .map(cpu_info::Info::read_freq)
            .max()
            .unwrap_or_default();

        if is_janked {
            self.util_max = None;
        } else {
            self.update_util_max();
        }

        // 先算权重（内部会更新 smoothed_weights），再按权重分配增量
        let weights = self.cluster_weights();
        let n = self.cpu_infos.len() as f64;
        let per_policy_control = Self::distribute_to_policy(control, &weights, n);

        self.cpu_infos
            .iter()
            .map(|cpu| {
                let cluster_control = per_policy_control
                    .get(&cpu.policy)
                    .copied()
                    .unwrap_or(control);

                (
                    cpu.policy,
                    if is_janked || self.util_max.is_none() {
                        cur_fas_freq_max
                            .saturating_add(cluster_control)
                            .clamp(0, self.max_freq)
                    } else {
                        let util_tracking_sugg_freq =
                            (cur_freq_max as f64 * self.util_max.unwrap() / 0.5) as isize; // min_util: 50%
                        #[cfg(debug_assertions)]
                        debug!(
                            "util: {}, cur_freq_max: {}, util_tracking_sugg_freq: {}",
                            self.util_max.unwrap(),
                            cur_freq_max,
                            util_tracking_sugg_freq
                        );
                        cur_fas_freq_max
                            .saturating_add(cluster_control)
                            .min(util_tracking_sugg_freq)
                            .clamp(0, self.max_freq)
                    },
                )
            })
            .collect()
    }

    fn sort_policies_topologically(&self) -> Vec<i32> {
        let mut graph: HashMap<_, Vec<_>> = HashMap::new();
        let mut indegree: HashMap<_, _> = HashMap::new();

        for cpu in &self.cpu_infos {
            let policy = cpu.policy;

            if let ExtraPolicy::RelRangeBound(ref rel_bound) = *EXTRA_POLICY_MAP
                .get()
                .context("EXTRA_POLICY_MAP not initialized")
                .unwrap()
                .get(&policy)
                .context("CPU Policy not found")
                .unwrap()
                .lock()
            {
                graph.entry(rel_bound.rel_to).or_default().push(policy);
                *indegree.entry(policy).or_insert(0) += 1;
            }

            indegree.entry(policy).or_insert(0);
        }

        let mut queue: Vec<_> = indegree
            .iter()
            .filter(|&(_, &deg)| deg == 0)
            .map(|(&policy, _)| policy)
            .collect();
        let mut sorted_policies = Vec::new();

        while let Some(policy) = queue.pop() {
            sorted_policies.push(policy);
            if let Some(dependents) = graph.get(&policy) {
                for &dependent in dependents {
                    if let Some(deg) = indegree.get_mut(&dependent) {
                        *deg -= 1;
                        if *deg == 0 {
                            queue.push(dependent);
                        }
                    }
                }
            }
        }

        assert!(
            (sorted_policies.len() >= indegree.len()),
            "Circular dependency detected in CPU policies"
        );

        sorted_policies
    }

    fn top_used_cores(&self) -> Option<CpuSet> {
        let top_threads_cpu_sets: Vec<_> = self
            .process_monitor
            .top_threads()
            .filter_map(|tid| sched_getaffinity(Pid::from_raw(tid)).ok())
            .collect();

        let mut counts = HashMap::new();
        for cpu_set in top_threads_cpu_sets.iter().copied() {
            *counts.entry(cpu_set).or_insert(0) += 1;
        }

        if counts.len() <= 1 {
            let mut top_used_cores = CpuSet::new();
            for cpuset in top_threads_cpu_sets {
                for core in 0..num_cpus::get() {
                    if cpuset.is_set(core).unwrap() {
                        top_used_cores.set(core).unwrap();
                    }
                }
            }

            return Some(top_used_cores);
        }

        let (mode, _) = counts.into_iter().max_by_key(|&(_num, count)| count)?;

        let mut top_used_cores = CpuSet::new();
        for cpuset in top_threads_cpu_sets
            .into_iter()
            .filter(|cpu_set| *cpu_set != mode)
        {
            for core in 0..num_cpus::get() {
                if cpuset.is_set(core).unwrap() {
                    top_used_cores.set(core).unwrap();
                }
            }
        }

        Some(top_used_cores)
    }

    fn apply_absolute_constraints(
        mut fas_freqs: HashMap<i32, isize>,
        sorted_policies: &[i32],
    ) -> HashMap<i32, isize> {
        for policy in sorted_policies {
            if let Some(freq) = fas_freqs.get(policy).copied() {
                if let ExtraPolicy::AbsRangeBound(ref abs_bound) = *EXTRA_POLICY_MAP
                    .get()
                    .context("EXTRA_POLICY_MAP not initialized")
                    .unwrap()
                    .get(policy)
                    .context("CPU Policy not found")
                    .unwrap()
                    .lock()
                {
                    let clamped_freq = freq.clamp(
                        abs_bound.min.unwrap_or(0),
                        abs_bound.max.unwrap_or(isize::MAX),
                    );
                    fas_freqs.insert(*policy, clamped_freq);
                }
            }
        }

        fas_freqs
    }

    fn apply_relative_constraints(
        mut fas_freqs: HashMap<i32, isize>,
        sorted_policies: &[i32],
    ) -> HashMap<i32, isize> {
        for policy in sorted_policies {
            if let Some(freq) = fas_freqs.get(policy).copied() {
                let adjusted_freq = match *EXTRA_POLICY_MAP
                    .get()
                    .context("EXTRA_POLICY_MAP not initialized")
                    .unwrap()
                    .get(policy)
                    .context("CPU Policy not found")
                    .unwrap()
                    .lock()
                {
                    ExtraPolicy::RelRangeBound(ref rel_bound) => {
                        let rel_to_freq = fas_freqs.get(&rel_bound.rel_to).copied().unwrap_or(0);

                        #[cfg(debug_assertions)]
                        debug!("policy{policy} rel_to {rel_to_freq}");

                        freq.clamp(
                            rel_to_freq + rel_bound.min.unwrap_or(isize::MIN),
                            rel_to_freq + rel_bound.max.unwrap_or(isize::MAX),
                        )
                    }
                    _ => freq,
                };

                #[cfg(debug_assertions)]
                debug!("policy{policy} freq after relative bound: {adjusted_freq}");

                fas_freqs.insert(*policy, adjusted_freq);
            }
        }

        fas_freqs
    }

    fn reset_all_cpu_freq(&mut self) {
        for cpu in &mut self.cpu_infos {
            let _ = cpu.reset(&mut self.file_handler);
        }
    }

    pub fn util_max(&self) -> f64 {
        self.util_max.unwrap_or_default()
    }
}

fn no_extra_policy() -> bool {
    EXTRA_POLICY_MAP
        .get()
        .context("EXTRA_POLICY_MAP not initialized")
        .unwrap()
        .values()
        .all(|policy| *policy.lock() == ExtraPolicy::None)
}

/// 读线程“上次运行的 CPU ID”。
///
/// 数据源：`/proc/[pid]/task/[tid]/stat`。
/// stat 行格式：`pid (comm) state ppid ... processor ...`。
/// comm 可能含空格/括号，用 `rfind(')')` 定位 comm 结束。
/// `processor` 是第 39 个字段，从 comm 之后算起索引为 36。
fn read_thread_cpu(pid: i32, tid: i32) -> Option<i32> {
    let path = format!("/proc/{pid}/task/{tid}/stat");
    let content = fs::read_to_string(path).ok()?;
    let after_comm = content.rfind(')')?;
    let rest = &content[after_comm + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    fields.get(36)?.parse().ok()
}
