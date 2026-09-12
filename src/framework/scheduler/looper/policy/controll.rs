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

use std::time::{Duration, Instant};

use likely_stable::unlikely;
#[cfg(debug_assertions)]
use log::debug;

use super::super::buffer::Buffer;
use crate::framework::{
    config::MarginFps,
    prelude::*,
    scheduler::looper::ControllerState,
};

pub fn calculate_control(
    buffer: &Buffer,
    config: &mut Config,
    mode: Mode,
    controller_state: &mut ControllerState,
    target_fps_offset_thermal: f64,
    cpu_util: f64, // 新增：CPU利用率
) -> Option<(isize, bool)> {
    // control, is_janked
    if unlikely(buffer.frametime_state.frametimes.len() < 60) {
        return None;
    }

    let target_fps = f64::from(buffer.target_fps_state.target_fps?);
    let margin_fps: f64 = match &config.mode_config(mode).margin_fps {
        MarginFps::BaseOnly(base) => target_fps / 60.0 * f64::from(*base),
        MarginFps::Advanced { base, overrides } => overrides
            .get(&target_fps.to_string())
            .copied()
            .map_or_else(|| target_fps / 60.0 * f64::from(*base), f64::from),
    };
    assert!(margin_fps.is_sign_positive(), "margin_fps must be positive");

    let target_fps = (target_fps + target_fps_offset_thermal).clamp(0.0, target_fps);
    let adjusted_target_fps = adjust_target_fps(target_fps, controller_state) - margin_fps;
    let adjusted_last_frame = get_normalized_last_frame(buffer, adjusted_target_fps);
    let target_frametime = Duration::from_secs(1);

    #[cfg(debug_assertions)]
    {
        debug!("adjusted_target_fps: {adjusted_target_fps}");
        debug!("adjusted_last_frame: {adjusted_last_frame:?}");
        debug!("target_frametime: {target_frametime:?}");
        debug!("cpu_util: {cpu_util:.4}");
    }

    Some((
        calculate_control_inner(
            controller_state,
            adjusted_last_frame,
            target_frametime,
            cpu_util,
        ),
        buffer.frametime_state.current_fps_long < target_fps - 2.0,
    ))
}

fn get_normalized_last_frame(buffer: &Buffer, target_fps: f64) -> Duration {
    let last_frame = buffer
        .frametime_state
        .frametimes
        .front()
        .copied()
        .unwrap_or_default();

    if buffer.frametime_state.additional_frametime == Duration::ZERO {
        last_frame
    } else {
        buffer.frametime_state.additional_frametime.max(last_frame)
    }
    .mul_f64(target_fps)
}

fn adjust_target_fps(target_fps: f64, controller_state: &mut ControllerState) -> f64 {
    if controller_state.usage_sample_timer.elapsed() >= Duration::from_secs(1) {
        controller_state.usage_sample_timer = Instant::now();
        let util = controller_state.controller.util_max();

        if util <= 0.1 {
            controller_state.target_fps_offset = 0.0;
        } else if util <= 0.55 {
            controller_state.target_fps_offset -= 0.1;
        } else if util >= 0.65 {
            controller_state.target_fps_offset += 0.1;
        }
    }

    controller_state.target_fps_offset = controller_state.target_fps_offset.clamp(-3.0, 0.0);
    target_fps + controller_state.target_fps_offset
}

fn calculate_control_inner(
    controller_state: &ControllerState,
    current_frametime: Duration,
    target_frametime: Duration,
    cpu_util: f64, // 新增：CPU利用率
) -> isize {
    let error_p = (current_frametime.as_nanos() as f64 - target_frametime.as_nanos() as f64)
        * controller_state.params.kp;

    #[cfg(debug_assertions)]
    debug!("error_p {error_p}");

    let mut control = error_p;

    // 仅对正向控制（升频）进行利用率衰减
    if control > 0.0 {
        let threshold = controller_state.params.util_decay_threshold;
        let mut util_factor = (cpu_util / threshold).clamp(0.0, 1.0);

        // 安全下限：帧时间严重超标（>2倍目标）且非卡顿时，保留至少0.5的升频系数
        let severe_miss =
            current_frametime.as_nanos() as f64 > target_frametime.as_nanos() as f64 * 2.0;
        if severe_miss && !controller_state.is_janked {
            util_factor = util_factor.max(0.5);
        }

        control *= util_factor;
    }

    // 限制单次控制量幅度
    let max_step = controller_state.max_freq as f64 * controller_state.params.max_step_ratio;
    control = control.clamp(-max_step, max_step);

    control as isize
}
