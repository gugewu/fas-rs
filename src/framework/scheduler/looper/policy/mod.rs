#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerParams {
    /// 比例系数
    #[serde(default = "ControllerParams::default_kp")]
    pub kp: f64,

    /// 积分系数
    #[serde(default = "ControllerParams::default_ki")]
    pub ki: f64,

    /// 微分系数
    #[serde(default = "ControllerParams::default_kd")]
    pub kd: f64,

    /// 单次 control 最大幅度占 max_freq 的比例（0.0 ~ 1.0）
    #[serde(default = "ControllerParams::default_max_step_ratio")]
    pub max_step_ratio: f64,

    /// CPU 利用率低于此值时，正向 control 线性衰减（0.0 ~ 1.0）
    #[serde(default = "ControllerParams::default_util_decay_threshold")]
    pub util_decay_threshold: f64,

    // ===== 负载需求率闭环参数 =====
    /// 负载需求率下限：低于此值主动降频
    #[serde(default = "ControllerParams::default_demand_low")]
    pub demand_low: f64,

    /// 负载需求率上限：高于此值保守升频
    #[serde(default = "ControllerParams::default_demand_high")]
    pub demand_high: f64,

    /// 降频基础步长占 max_freq 比例（deficit = 0 时）
    #[serde(default = "ControllerParams::default_demand_step_base")]
    pub demand_step_base: f64,

    /// 降频步长随 deficit 额外放大的比例（deficit = 1 时再叠加这么多）
    #[serde(default = "ControllerParams::default_demand_step_scale")]
    pub demand_step_scale: f64,

    /// 保守升频最大步长占 max_freq 比例
    #[serde(default = "ControllerParams::default_demand_up_max")]
    pub demand_up_max: f64,

    /// 控制模式最小驻留时间（毫秒）
    #[serde(default = "ControllerParams::default_mode_residency_ms")]
    pub mode_residency_ms: u64,

    /// fps_ok 滞回：已达标时，current_fps 掉到此 margin 以下才判不达标
    #[serde(default = "ControllerParams::default_fps_ok_margin")]
    pub fps_ok_margin: f64,

    /// fps_ok 滞回：未达标时，current_fps 升到此 margin 以上才判达标
    #[serde(default = "ControllerParams::default_fps_ok_recover_margin")]
    pub fps_ok_recover_margin: f64,
}

impl ControllerParams {
    const fn default_kp() -> f64 {
        0.0003
    }

    const fn default_ki() -> f64 {
        0.0
    }

    const fn default_kd() -> f64 {
        0.0
    }

    const fn default_max_step_ratio() -> f64 {
        0.15
    }

    const fn default_util_decay_threshold() -> f64 {
        0.3
    }

    const fn default_demand_low() -> f64 {
        0.60
    }

    const fn default_demand_high() -> f64 {
        0.85
    }

    const fn default_demand_step_base() -> f64 {
        0.05
    }

    const fn default_demand_step_scale() -> f64 {
        0.15
    }

    const fn default_demand_up_max() -> f64 {
        0.02
    }

    const fn default_mode_residency_ms() -> u64 {
        500
    }

    const fn default_fps_ok_margin() -> f64 {
        5.0
    }

    const fn default_fps_ok_recover_margin() -> f64 {
        2.0
    }
}

impl Default for ControllerParams {
    fn default() -> Self {
        Self {
            kp: Self::default_kp(),
            ki: Self::default_ki(),
            kd: Self::default_kd(),
            max_step_ratio: Self::default_max_step_ratio(),
            util_decay_threshold: Self::default_util_decay_threshold(),
            demand_low: Self::default_demand_low(),
            demand_high: Self::default_demand_high(),
            demand_step_base: Self::default_demand_step_base(),
            demand_step_scale: Self::default_demand_step_scale(),
            demand_up_max: Self::default_demand_up_max(),
            mode_residency_ms: Self::default_mode_residency_ms(),
            fps_ok_margin: Self::default_fps_ok_margin(),
            fps_ok_recover_margin: Self::default_fps_ok_recover_margin(),
        }
    }
}
