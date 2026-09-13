// src/framework/scheduler/looper/buffer/calculate.rs

use std::time::Duration;
use likely_stable::unlikely;
#[cfg(debug_assertions)]
use log::debug;
use super::Buffer;
use crate::{Extension, api::trigger_target_fps_change, framework::config::TargetFps};

// ========== TargetFpsState 结构体（需修改） ==========
#[derive(Debug, Clone)]
pub struct TargetFpsState {
    pub target_fps_config: TargetFps,
    pub target_fps: Option<u32>,
    // 新增字段
    pub last_target: Option<u32>,
    pub upgrade_attempts: u32,
    pub cooldown_remaining: u32,
}

impl Default for TargetFpsState {
    fn default() -> Self {
        Self {
            target_fps_config: TargetFps::Value(60),
            target_fps: None,
            last_target: None,
            upgrade_attempts: 0,
            cooldown_remaining: 0,
        }
    }
}

// ========== impl Buffer ==========
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

        let avg_time_short = self
            .calculate_average_frametime(self.target_fps().map(|target_fps| target_fps as usize));
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

    /// 修复后的 target_fps：固定下限 15、从高到低遍历、贴顶升档、冷却回退
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

        // 从高到低遍历
        for &target_fps in target_fpses.iter().rev() {
            let target = f64::from(target_fps);

            // 贴顶区间：尝试升档
            if current_fps >= target - MARGIN && current_fps <= target + MARGIN {
                // 已是最高档，直接返回
                if target_fps == *target_fpses.last().unwrap() {
                    self.target_fps_state.upgrade_attempts = 0;
                    self.target_fps_state.last_target = Some(target_fps);
                    return Some(target_fps);
                }

                // 贴顶但未到上限，递增尝试计数
                self.target_fps_state.upgrade_attempts += 1;
                if self.target_fps_state.upgrade_attempts >= UPGRADE_ATTEMPTS {
                    // 尝试升到下一档
                    if let Some(pos) = target_fpses.iter().position(|&x| x == target_fps) {
                        if let Some(&next) = target_fpses.get(pos + 1) {
                            self.target_fps_state.upgrade_attempts = 0;
                            self.target_fps_state.last_target = Some(next);
                            return Some(next);
                        }
                    }
                }
                self.target_fps_state.last_target = Some(target_fps);
                return Some(target_fps);
            }

            // 正常匹配：current_fps 低于该档 + margin
            if current_fps <= target + MARGIN {
                self.target_fps_state.upgrade_attempts = 0;
                self.target_fps_state.last_target = Some(target_fps);
                return Some(target_fps);
            }
        }

        // 高于所有档位：升档失败，进入冷却
        self.target_fps_state.upgrade_attempts = 0;
        self.target_fps_state.cooldown_remaining = COOLDOWN_FRAMES;
        target_fpses.last().copied()
    }
}
