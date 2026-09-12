pub mod controll;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ControllerParams {
    pub kp: f64,

    /// 单次 control 调整的最大幅度（相对于 max_freq 的比例）
    /// 默认 0.15，即单次最多调整 15% 的最大频率
    #[serde(default = "default_max_step_ratio")]
    pub max_step_ratio: f64,

    /// 利用率衰减阈值：CPU 利用率低于此值时，正向 control 线性衰减
    /// 默认 0.3，即利用率 30% 以下开始衰减
    #[serde(default = "default_util_decay_threshold")]
    pub util_decay_threshold: f64,
}

fn default_max_step_ratio() -> f64 {
    0.15
}

fn default_util_decay_threshold() -> f64 {
    0.3
}

impl Default for ControllerParams {
    fn default() -> Self {
        Self {
            kp: 0.000_3,
            max_step_ratio: default_max_step_ratio(),
            util_decay_threshold: default_util_decay_threshold(),
        }
    }
}
