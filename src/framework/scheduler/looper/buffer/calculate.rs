// Copyright 2024-2025, shadow3aaa
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
        // 检测当前游戏运行的档位。检测失败（无法判断）时保持现有 target 不变。
        let Some(detected) = self.detect_target_fps() else {
            return;
        };

        // 通过防抖器决定是否真正切换
        let new_target_fps = self.fps_switcher.update(detected);

        // 只在真正切换时才触发副作用（清空帧窗口、通知外部）
        if self.target_fps_state.target_fps != Some(new_target_fps) {
            self.reset_frametime_state();
            self.trigger_target_fps_change(extension, new_target_fps);
            self.target_fps_state.target_fps = Some(new_target_fps);
            self.unusable();
        }
    }

    fn reset_frametime_state(&mut self) {
        self.frametime_state.frametimes.clear();
    }

    fn trigger_target_fps_change(&self, extension: &Extension, target_fps: u32) {
        trigger_target_fps_change(extension, target_fps, self.package_info.pkg.clone());
    }

    /// 返回当前已确认的 target_fps。
    ///
    /// 与旧版语义不同：旧版每次重新从配置派生；现在直接读取已确认状态，
    /// 由 `detect_target_fps` + `FpsSwitcher` 更新。
    fn target_fps(&self) -> Option<u32> {
        self.target_fps_state.target_fps
    }

    /// 取最近 n 帧的中位数，抗瞬时尖刺。
    ///
    /// `frametimes` 使用 `push_front` 推入新帧，所以 `iter().take(n)`
    /// 取到的就是"最近的 n 帧"。
    fn recent_frametime_median(&self, n: usize) -> Option<Duration> {
        let mut recent: Vec<Duration> = self
            .frametime_state
            .frametimes
            .iter()
            .take(n)
            .copied()
            .collect();

        if recent.is_empty() {
            return None;
        }

        recent.sort_unstable();
        Some(recent[recent.len() / 2])
    }

    /// 从帧时间分布反推当前游戏运行的档位。
    ///
    /// 关键设计：
    /// 1. 用最近 20 帧的**中位数**而非全量平均，切档后 ~200ms 就能反映新档位，
    ///    且瞬时卡顿不会把中位数拉偏；
    /// 2. 加入**迟滞**：如果当前已有 target，且最近帧率不低于当前 target 的
    ///    80%，保持当前 target 不降级。避免"游戏想跑 120 但只能跑到 100"被
    ///    认输到 90；
    /// 3. 在候选集（`TargetFps::Array`）里找**最接近**的档位；
    /// 4. 差异超过 15% 容差时返回 `None`，让调用方保持现有 target 不动作。
    ///
    /// 单档位场景（`TargetFps::Value`）直接返回，不做检测。
    fn detect_target_fps(&self) -> Option<u32> {
        let target_fpses = match &self.target_fps_state.target_fps_config {
            TargetFps::Value(t) => return Some(*t),
            TargetFps::Array(arr) => arr,
        };

        if target_fpses.is_empty() {
            return None;
        }

        // 窗口：最近 20 帧。30fps 下约 660ms，120fps 下约 166ms。
        const WINDOW: usize = 20;
        // 至少需要这么多帧才做判断，否则中位数不可靠
        const MIN_SAMPLES: usize = 10;

        // 数据太少（比如刚切换清空过），暂不判断
        let available = self.frametime_state.frametimes.len().min(WINDOW);
        if available < MIN_SAMPLES {
            return None;
        }

        let recent = self.recent_frametime_median(WINDOW)?;
        if recent.is_zero() {
            return None;
        }

        let recent_fps = 1.0 / recent.as_secs_f64();

        // 迟滞：只在"可能降级"时生效，防止因游戏跑不满而认输到低档位
        if let Some(current) = self.target_fps_state.target_fps {
            if recent_fps < f64::from(current) && recent_fps >= f64::from(current) * 0.8 {
                #[cfg(debug_assertions)]
                debug!(
                    "detect_target_fps: hysteresis, recent_fps={recent_fps:.2}, \
                     keeping current={current}"
                );
                return Some(current);
            }
        }

        // 在候选集里找最接近的档位
        let mut best = target_fpses[0];
        let mut best_dist = f64::MAX;
        for &fps in target_fpses {
            let dist = (recent_fps - f64::from(fps)).abs();
            if dist < best_dist {
                best_dist = dist;
                best = fps;
            }
        }

        // 容差 15%：比如 120fps 容差 ±18fps，60fps 容差 ±9fps。
        // 超出容差说明最近帧率不靠近任何档位（过渡期或卡顿），
        // 返回 None 让调用方保持现状。
        let tolerance = f64::from(best) * 0.15;
        if best_dist > tolerance {
            #[cfg(debug_assertions)]
            debug!(
                "detect_target_fps: recent_fps={recent_fps:.2}, \
                 best={best}, dist={best_dist:.2} > tolerance={tolerance:.2}, \
                 keep current"
            );
            return None;
        }

        #[cfg(debug_assertions)]
        debug!(
            "detect_target_fps: recent_fps={recent_fps:.2}, \
             matched={best}, dist={best_dist:.2}"
        );

        Some(best)
    }
}
