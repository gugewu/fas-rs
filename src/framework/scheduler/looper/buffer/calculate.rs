// src/framework/scheduler/looper/buffer/calculate.rs

use std::time::Duration;
#[cfg(debug_assertions)]
use log::debug;
use super::Buffer;
use crate::{Extension, api::trigger_target_fps_change, framework::config::TargetFps};

impl Buffer {
    pub fn calculate_current_fps(&mut self) {
        let avg_time_long = self.calculate_average_frametime(None);
        #[cfg(debug_assertions)]
        debug!("avg_time_long: {avg_time_long:?}");
        self.frametime_state.avg_time_long = avg_time_long;
        let current_fps_long = 1.0 / avg_time_long.as_secs_f64();
        #[cfg(debug_assertions)]
        debug!("current_fps_long: {current_fps_long:.2}");
        self.frametime_state.current_fps_long = current_fps_long;

        // ===== 修复 E0502：先算出 target_fps（可变借用在此结束），再传 Option<usize> 给 &self 方法 =====
        let target_fps_usize = self.target_fps().map(|t| t as usize);
        let avg_time_short = self.calculate_average_frametime(target_fps_usize);
        #[cfg(debug_assertions)]
        debug!("avg_time_short: {avg_time_short:?}");
        self.frametime_state.avg_time_short = avg_time_short;
        let current_fps_short = 1.0 / avg_time_short.as_secs_f64();
        #[cfg(debug_assertions)]
        debug!("current_fps_short: {current_fps_short:.2}");
        self.frametime_state.current_fps_short = current_fps_short;
    }

    fn calculate_average_frametime(&self, it_takes: Option<usize>) -> Duration {
        let total_time: Duration = self
            .frametime_state
            .frametimes
            .iter()
            .take(it_takes.unwrap_or(self.frametime_state.frametimes.len()))
            .sum::<Duration>()
            .saturating_add(self.frametime_state.additional_frametime);

        total_time
            .checked_div(
                it_takes
                    .unwrap_or(self.frametime_state.frametimes.len())
                    .min(self.frametime_state.frametimes.len())
                    .try_into()
                    .unwrap(),
            )
            .unwrap_or_default()
    }

    pub fn calculate_target_fps(&mut self, extension: &Extension) {
        let new_target_fps = self.target_fps();
        if self.target_fps_state.target_fps != new_target_fps || new_target_fps.is_none() {
            self.reset_frametime_state();
            if let Some(target_fps) = new_target_fps {
                self.trigger_target_fps_change(extension, target_fps);
            }
            self.target_fps_state.target_fps = new_target_fps;
            self.unusable();
        }
    }

    fn reset_frametime_state(&mut self) {
        self.frametime_state.frametimes.clear();
    }

    fn trigger_target_fps_change(&self, extension: &Extension, target_fps: u32) {
        trigger_target_fps_change(extension, target_fps, self.package_info.pkg.clone());
    }

    /// target_fps：
    /// 1. 固定下限 15
    /// 2. 匹配规则：选择"不超过 current_fps + MARGIN 的最大候选"
    ///    （原逻辑是"从高到低第一个 fps <= target + MARGIN 的候选"，
    ///     对 fps=98, MARGIN=3 会错误命中 120，因为 98 <= 123）
    /// 3. 贴顶升档 + 连续尝试计数
    /// 4. fps 高于最高档上限 + MARGIN 时进入冷却，冷却期返回上次目标
    fn target_fps(&mut self) -> Option<u32> {
        const MIN_FPS: f64 = 15.0;
        const MARGIN: f64 = 3.0;
        const UPGRADE_ATTEMPTS: u32 = 3;
        const COOLDOWN_FRAMES: u32 = 30;

        let current_fps = self.frametime_state.current_fps_long;

        // 冷却期优先处理
        if self.target_fps_state.cooldown_remaining > 0 {
            self.target_fps_state.cooldown_remaining -= 1;
            return self.target_fps_state.last_target;
        }

        // 固定下限 15
        if current_fps < MIN_FPS {
            self.target_fps_state.upgrade_attempts = 0;
            return None;
        }

        let mut target_fpses = match &self.target_fps_state.target_fps_config {
            TargetFps::Value(t) => vec![*t],
            TargetFps::Array(arr) => arr.clone(),
        };
        target_fpses.sort_unstable();
        target_fpses.dedup();

        let max_target = *target_fpses.last().unwrap();

        // fps 高于最高档上限 + MARGIN：升档失败，进入冷却
        if current_fps > f64::from(max_target) + MARGIN {
            self.target_fps_state.upgrade_attempts = 0;
            self.target_fps_state.cooldown_remaining = COOLDOWN_FRAMES;
            return Some(max_target);
        }

        // 匹配：取"不超过 current_fps + MARGIN"的最大候选
        // 对 [30, 60, 90, 120]，fps=98 → 只有 30/60/90 满足 t <= 101，取最大者 90
        // 对 fps=118 → 全部满足 t <= 121，取最大者 120
        let matched = target_fpses
            .iter()
            .rev()
            .copied()
            .find(|&t| f64::from(t) <= current_fps + MARGIN)
            .unwrap_or(*target_fpses.first().unwrap());

        let target = f64::from(matched);

        // 贴顶区间：尝试升档
        if current_fps >= target - MARGIN && current_fps <= target + MARGIN {
            // 已是最高档，直接返回
            if matched == max_target {
                self.target_fps_state.upgrade_attempts = 0;
                self.target_fps_state.last_target = Some(matched);
                return Some(matched);
            }

            // 贴顶但未到上限，递增尝试计数
            self.target_fps_state.upgrade_attempts += 1;
            if self.target_fps_state.upgrade_attempts >= UPGRADE_ATTEMPTS {
                if let Some(pos) = target_fpses.iter().position(|&x| x == matched) {
                    if let Some(&next) = target_fpses.get(pos + 1) {
                        self.target_fps_state.upgrade_attempts = 0;
                        self.target_fps_state.last_target = Some(next);
                        return Some(next);
                    }
                }
            }
            self.target_fps_state.last_target = Some(matched);
            return Some(matched);
        }

        // 正常匹配
        self.target_fps_state.upgrade_attempts = 0;
        self.target_fps_state.last_target = Some(matched);
        Some(matched)
    }
}
