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
    cpu_util: f64,
) -> Option<(isize, bool)> {
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

    // ---- 原 PID 输出 ----
    let raw_control = calculate_control_inner(
        controller_state,
        adjusted_last_frame,
        target_frametime,
        cpu_util,
    );

    // ===== 利用率闭环（新增） =====
    // 目标区间：80% - 90%
    // 帧时间未达标 / 帧率未达标时，走原 PID（不掉帧优先）
    // 帧时间达标且帧率达标时，用利用率控制频率：
    //   利用率 < 80%  → 降频（频率给多了）
    //   利用率 > 90%  → 保守升频（留余量）
    //   80-90%        → 维持
    const UTIL_LOW: f64 = 0.80;
    const UTIL_HIGH: f64 = 0.90;

    let frametime_miss = adjusted_last_frame > target_frametime;
    let fps_ok = buffer.frametime_state.current_fps_long >= target_fps;

    let control = if !frametime_miss && fps_ok {
        if cpu_util < UTIL_LOW {
            // 利用率偏低：频率给多了，主动降频
            // deficit 越大，降频步长越大（2% ~ 10% max_freq）
            let deficit = (UTIL_LOW - cpu_util) / UTIL_LOW;
            let step = (controller_state.max_freq as f64 * (0.02 + deficit * 0.08)) as isize;
            (-step).max(raw_control)
        } else if cpu_util > UTIL_HIGH {
            // 利用率偏高：留余量，保守升频（最多 2% max_freq）
            let step = (controller_state.max_freq as f64 * 0.02) as isize;
            step.min(raw_control)
        } else {
            // 落在 80-90% 区间，维持现状
            0
        }
    } else {
        // 帧时间未达标或帧率未达标：走原 PID 控制
        raw_control
    };
    // ===== 利用率闭环结束 =====

    #[cfg(debug_assertions)]
    debug!("raw_control: {raw_control}, control after util-loop: {control}");

    Some((
        control,
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
    cpu_util: f64,
) -> isize {
    let error_p = (current_frametime.as_nanos() as f64 - target_frametime.as_nanos() as f64)
        * controller_state.params.kp;

    #[cfg(debug_assertions)]
    debug!("error_p {error_p}");

    let mut control = error_p;

    // 仅对正向控制（升频）进行利用率衰减
    if control > 0.0 {
        let threshold = controller_state.params.util_decay_threshold;
        let util_factor = (cpu_util / threshold).clamp(0.0, 1.0);
        // 【已删除 severe_miss 强制下限】
        // 原因：帧时间严重超标但 CPU 利用率低时，说明瓶颈不在 CPU，
        //       强制升频只会导致频率空转。
        control *= util_factor;
    }

    // 限制单次控制量幅度，防止频率剧烈跳变
    let max_step = controller_state.max_freq as f64 * controller_state.params.max_step_ratio;
    control = control.clamp(-max_step, max_step);

    control as isize
}
