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

/// 帧时间与负载需求率闭环控制器
///
/// # demand 语义
///
/// 本函数接收的 `demand` 由调用方通过 `cpu_cycles_reader` 计算，
/// 语义为**负载需求率**：
///
/// ```text
/// demand = cycles_delta / (duration * cluster_max_freq)
/// ```
///
/// 分母是该集群**最高频**，而非当前频率。含义是：
/// “若 CPU 跑满最高频，当前负载需要占用多少比例的周期”。
///
/// 与频率利用率（分母为当前频率）的区别：
/// - 频率利用率会因降频而虚高，不适合做降频依据
/// - 负载需求率只反映负载本身，不受当前频率影响
/// - `demand < 1.0` 说明当前负载不需要跑满最高频，有余量可降
///
/// 本函数在入口处将该值 clamp 到 `[0.0, 1.0]`，保证阈值比较可预测。
pub fn calculate_control(
    buffer: &Buffer,
    config: &mut Config,
    mode: Mode,
    controller_state: &mut ControllerState,
    target_fps_offset_thermal: f64,
    demand: f64,
) -> Option<(isize, bool)> {
    if unlikely(buffer.frametime_state.frametimes.len() < 60) {
        return None;
    }

    let demand = demand.clamp(0.0, 1.0);

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
        debug!("demand (load-based, clamped): {demand:.4}");
    }

    // 原 PID 输出
    let raw_control = calculate_control_inner(
        controller_state,
        adjusted_last_frame,
        target_frametime,
        demand,
    );

    // ===== 负载需求率闭环 =====
    // 所有阈值/步长从 params 读取，可在 webui 中调整。
    let demand_low = controller_state.params.demand_low;
    let demand_high = controller_state.params.demand_high;
    let step_base = controller_state.params.demand_step_base;
    let step_scale = controller_state.params.demand_step_scale;
    let up_max_ratio = controller_state.params.demand_up_max;
    let residency_ms = controller_state.params.mode_residency_ms;
    let fps_ok_margin = controller_state.params.fps_ok_margin;
    let fps_ok_recover_margin = controller_state.params.fps_ok_recover_margin;

    let frametime_miss = adjusted_last_frame > target_frametime;
    let current_fps = buffer.frametime_state.current_fps_long;

    // fps_ok 双阈值滞回。
    //
    // 多档位（60/90/120）时 current_fps_long 会在目标附近抖动，
    // 单阈值判定会导致 fps_ok 每帧翻转，控制模式反复横跳，
    // 帧率卡在中间档位上不去。改为滞回：
    //   - 已达标：掉到 target - fps_ok_margin 以下才认为不达标
    //   - 未达标：爬到 target - fps_ok_recover_margin 以上才认为达标
    let fps_ok = if controller_state.was_fps_ok {
        current_fps >= target_fps - fps_ok_margin
    } else {
        current_fps >= target_fps - fps_ok_recover_margin
    };
    controller_state.was_fps_ok = fps_ok;

    // 模式切换最小驻留时间。
    //
    // 即使 fps_ok 已稳定，frametime_miss 也可能因单帧尖峰翻转。
    // 加入驻留时间，切换后至少在当前模式待满才允许切回。
    let want_demand_mode = !frametime_miss && fps_ok;
    let now = Instant::now();
    let can_switch = want_demand_mode == controller_state.last_control_mode
        || controller_state.last_mode_switch.elapsed() >= Duration::from_millis(residency_ms);
    let use_demand_mode = if can_switch {
        if want_demand_mode != controller_state.last_control_mode {
            controller_state.last_mode_switch = now;
        }
        controller_state.last_control_mode = want_demand_mode;
        want_demand_mode
    } else {
        controller_state.last_control_mode
    };

    #[cfg(debug_assertions)]
    debug!(
        "frametime_miss: {frametime_miss}, fps_ok: {fps_ok}, \
         current_fps: {current_fps:.2}, target_fps: {target_fps:.2}, \
         use_demand_mode: {use_demand_mode}"
    );

    let control = if use_demand_mode {
        if demand < demand_low {
            // 需求率低 → 主动降频（不依赖 raw_control）
            // deficit 越大，降频步长越大
            let deficit = (demand_low - demand) / demand_low;
            let step = (controller_state.max_freq as f64 * (step_base + deficit * step_scale)) as isize;
            -step
        } else if demand > demand_high {
            // 需求率高 → 保守升频
            let step = (controller_state.max_freq as f64 * up_max_ratio) as isize;
            raw_control.min(step)
        } else {
            // 维持
            0
        }
    } else {
        // 帧时间未达标或帧率未达标 → 走原 PID
        raw_control
    };
    // ===== 负载需求率闭环结束 =====

    #[cfg(debug_assertions)]
    debug!("raw_control: {raw_control}, control after demand-loop: {control}");

    Some((
        control,
        buffer.frametime_state.current_fps_long < target_fps - fps_ok_recover_margin,
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
    demand: f64,
) -> isize {
    let error_p = (current_frametime.as_nanos() as f64 - target_frametime.as_nanos() as f64)
        * controller_state.params.kp;

    #[cfg(debug_assertions)]
    debug!("error_p {error_p}");

    let mut control = error_p;

    // 仅对正向控制（升频）进行需求率衰减。
    //
    // demand 已是负载需求率（分母为集群最高频），比频率利用率
    // 更准确地反映“CPU 是否真的需要更多周期”。当需求率低于
    // util_decay_threshold 时，说明瓶颈不在 CPU，按比例抑制升频。
    if control > 0.0 {
        let threshold = controller_state.params.util_decay_threshold;
        let util_factor = (demand / threshold).clamp(0.0, 1.0);
        control *= util_factor;
    }

    // 限制单次控制量幅度，防止频率剧烈跳变
    let max_step = controller_state.max_freq as f64 * controller_state.params.max_step_ratio;
    control = control.clamp(-max_step, max_step);

    control as isize
}
