use crate::global::{AppResult, RecordingConfig, TARGET_SAMPLE_RATE};
use nnnoiseless::DenoiseState;
#[cfg(feature = "webrtc_apm")]
use webrtc_audio_processing::{
    Config as WebRtcConfig, EchoCancellation, EchoCancellationSuppressionLevel, GainControl, GainControlMode,
    InitializationConfig, NoiseSuppression, NoiseSuppressionLevel, Processor as WebRtcProcessor, SampleRate,
    NUM_SAMPLES_PER_FRAME,
};

pub(crate) struct DspProcessor {
    #[cfg(feature = "webrtc_apm")]
    webrtc: Option<WebRtcProcessor>,
    rnnoise: Option<Box<DenoiseState>>,
    simple_aec_agc: Option<SimpleAecAgc>,
    frame_size: usize,
    // 标记当前是否启用 AEC（WebRTC 或纯 Rust），决定是否走简易回声抑制兜底
    aec_active: bool,
    // WebRTC 处理所需的固定帧缓冲
    webrtc_render_buffer: Vec<f32>,
    webrtc_capture_buffer: Vec<f32>,
    #[cfg(feature = "webrtc_apm")]
    webrtc_config: WebRtcConfig,
}

impl DspProcessor {
    pub(crate) fn new(config: RecordingConfig) -> AppResult<Self> {
        #[cfg(feature = "webrtc_apm")]
        let frame_size = {
            // 同时满足 WebRTC 与 RNNoise 的最小帧长
            DenoiseState::FRAME_SIZE.max(NUM_SAMPLES_PER_FRAME as usize)
        };
        #[cfg(not(feature = "webrtc_apm"))]
        let frame_size = DenoiseState::FRAME_SIZE;
        let mut processor = Self {
            #[cfg(feature = "webrtc_apm")]
            webrtc: None,
            rnnoise: None,
            simple_aec_agc: None,
            frame_size,
            aec_active: false,
            webrtc_render_buffer: vec![0.0; frame_size],
            webrtc_capture_buffer: vec![0.0; frame_size],
            #[cfg(feature = "webrtc_apm")]
            webrtc_config: WebRtcConfig::default(),
        };
        processor.reconfigure(config)?;
        Ok(processor)
    }

    pub(crate) fn frame_size(&self) -> usize {
        self.frame_size
    }

    pub(crate) fn webrtc_aec_active(&self) -> bool {
        self.aec_active
    }

    pub(crate) fn reconfigure(&mut self, config: RecordingConfig) -> AppResult<()> {
        let want_aec = config.enable_webrtc_aec;
        let want_agc = config.enable_webrtc_agc;
        #[cfg(feature = "webrtc_apm")]
        {
            if config.enable_webrtc_aec || config.enable_webrtc_ns || config.enable_webrtc_agc {
                // 启用 WebRTC APM 时固定为 48kHz/单声道处理
                // 默认配置建议：
                // AEC=High：适合大多数桌面回放场景；过度抑制可改为 Moderate
                // NS=High：噪声更低但易有“水声”，可改为 Moderate/Low
                // AGC=AdaptiveDigital：目标电平 3dBFS、压缩增益 9dB；过响可降低增益或提高目标电平
                let init = InitializationConfig {
                    sample_rate: SampleRate::Hz48000,
                    num_capture_channels: 1,
                    num_render_channels: 1,
                };
                let mut processor = WebRtcProcessor::new(&init)?;
                let mut webrtc_config = WebRtcConfig::default();
                webrtc_config.echo_cancellation = EchoCancellation {
                    enabled: config.enable_webrtc_aec,
                    suppression_level: EchoCancellationSuppressionLevel::High,
                };
                webrtc_config.noise_suppression = NoiseSuppression {
                    enabled: config.enable_webrtc_ns,
                    level: NoiseSuppressionLevel::High,
                };
                webrtc_config.gain_control = GainControl {
                    enabled: config.enable_webrtc_agc,
                    mode: GainControlMode::AdaptiveDigital,
                    target_level_dbfs: 3,
                    compression_gain_db: 9,
                    enable_limiter: true,
                };
                processor.set_config(&webrtc_config)?;
                self.webrtc = Some(processor);
                self.webrtc_config = webrtc_config;
                self.frame_size = NUM_SAMPLES_PER_FRAME as usize;
            } else {
                self.webrtc = None;
            }
        }

        #[cfg(feature = "webrtc_apm")]
        {
            self.aec_active = self.webrtc.is_some() && config.enable_webrtc_aec;
        }
        #[cfg(not(feature = "webrtc_apm"))]
        {
            self.aec_active = false;
        }

        // 纯 Rust AEC/AGC：当 WebRTC APM 不可用或未启用时，提供 NLMS AEC + RMS AGC 兜底
        #[cfg(feature = "webrtc_apm")]
        let webrtc_available = self.webrtc.is_some();
        #[cfg(not(feature = "webrtc_apm"))]
        let webrtc_available = false;

        if !webrtc_available && (want_aec || want_agc) {
            self.simple_aec_agc = Some(SimpleAecAgc::new(TARGET_SAMPLE_RATE, self.frame_size));
            if want_aec {
                self.aec_active = true;
            }
        } else {
            self.simple_aec_agc = None;
        }

        self.rnnoise = if config.enable_rnnoise {
            Some(DenoiseState::new())
        } else {
            None
        };

        // 确保处理帧长与缓冲区长度一致
        if self.frame_size < DenoiseState::FRAME_SIZE {
            self.frame_size = DenoiseState::FRAME_SIZE;
        }
        if self.webrtc_render_buffer.len() != self.frame_size {
            self.webrtc_render_buffer.resize(self.frame_size, 0.0);
        }
        if self.webrtc_capture_buffer.len() != self.frame_size {
            self.webrtc_capture_buffer.resize(self.frame_size, 0.0);
        }
        Ok(())
    }

    pub(crate) fn process_mic_frame(&mut self, mic_mono: &mut [f32], spk_mono: &[f32], config: &RecordingConfig) -> AppResult<()> {
        #[cfg(feature = "webrtc_apm")]
        let mut used_webrtc = false;
        #[cfg(not(feature = "webrtc_apm"))]
        let used_webrtc = false;
        #[cfg(feature = "webrtc_apm")]
        if let Some(processor) = &mut self.webrtc {
            // WebRTC 处理需要与渲染端对齐的固定帧长
            if mic_mono.len() == self.frame_size && spk_mono.len() == self.frame_size {
                self.webrtc_render_buffer.copy_from_slice(spk_mono);
                self.webrtc_capture_buffer.copy_from_slice(mic_mono);
                processor.process_render_frame(&mut self.webrtc_render_buffer)?;
                processor.process_capture_frame(&mut self.webrtc_capture_buffer)?;
                mic_mono.copy_from_slice(&self.webrtc_capture_buffer);
                used_webrtc = true;
            }
        }

        if !used_webrtc {
            if let Some(simple) = &mut self.simple_aec_agc {
                simple.process_frame(mic_mono, spk_mono, config.enable_webrtc_aec, config.enable_webrtc_agc);
            }
        }

        if config.enable_rnnoise {
            if let Some(denoise) = &mut self.rnnoise {
                // RNNoise 仅支持固定帧长，长度不匹配时跳过以避免失真
                if mic_mono.len() != DenoiseState::FRAME_SIZE {
                    return Ok(());
                }
                let mut output = vec![0.0f32; DenoiseState::FRAME_SIZE];
                let mut input = vec![0.0f32; DenoiseState::FRAME_SIZE];
                let scale = i16::MAX as f32;
                for i in 0..DenoiseState::FRAME_SIZE {
                    let sample = mic_mono[i].clamp(-1.0, 1.0) * scale;
                    input[i] = sample;
                }
                denoise.process_frame(&mut output, &input);
                for i in 0..DenoiseState::FRAME_SIZE {
                    mic_mono[i] = (output[i] / scale).clamp(-1.0, 1.0);
                }
            }
        }
        Ok(())
    }
}

struct SimpleAecAgc {
    sample_rate: usize,
    frame_size: usize,
    filter_len: usize,
    mu: f32,
    leakage: f32,
    eps: f32,
    ref_buf: Vec<f32>,
    ref_pos: usize,
    weights: Vec<f32>,
    dt_corr_threshold: f32,
    dt_energy_ratio: f32,
    adapt_min_ref_rms: f32,
    postfilter_threshold_ratio: f32,
    postfilter_max_reduction: f32,
    agc_target_rms: f32,
    agc_attack: f32,
    agc_release: f32,
    agc_gain: f32,
    agc_min_gain: f32,
    agc_max_gain: f32,
    noise_rms: f32,
    ns_attack: f32,
    ns_release: f32,
    ns_threshold_ratio: f32,
    ns_max_reduction: f32,
}

impl SimpleAecAgc {
    fn new(sample_rate: usize, frame_size: usize) -> Self {
        let filter_len = 2048;
        Self {
            sample_rate,
            frame_size,
            filter_len,
            mu: 0.1, // NLMS 自适应步长，越大收敛更快但易失稳
            leakage: 0.999, // 权重泄漏因子，抑制漂移
            eps: 1e-6, // 防止归一化分母为 0
            ref_buf: vec![0.0; filter_len], // 参考信号环形缓冲
            ref_pos: 0, // 环形缓冲读写指针
            weights: vec![0.0; filter_len], // 自适应滤波器权重
            dt_corr_threshold: 0.2, // 双讲检测相关性阈值，越低越保守
            dt_energy_ratio: 1.5, // 双讲检测能量比例阈值
            adapt_min_ref_rms: 0.005, // 允许自适应的最小参考能量
            postfilter_threshold_ratio: 0.5, // 残余回声抑制的参考门限比例
            postfilter_max_reduction: 0.6, // 残余回声最大衰减比例
            agc_target_rms: 0.2, // AGC 目标 RMS
            agc_attack: 0.05, // AGC 攻击时间系数
            agc_release: 0.02, // AGC 释放时间系数
            agc_gain: 1.0, // 当前 AGC 增益
            agc_min_gain: 0.5, // AGC 最小增益
            agc_max_gain: 4.0, // AGC 最大增益
            noise_rms: 0.01, // 噪声地板初始估计
            ns_attack: 0.1, // 噪声估计攻击系数
            ns_release: 0.02, // 噪声估计释放系数
            ns_threshold_ratio: 1.5, // 噪声门限比例
            ns_max_reduction: 0.6, // 噪声最大衰减比例
        }
    }

    // 纯 Rust AEC/AGC 单帧处理入口：以扬声器参考信号抑制回声并进行自动增益
    fn process_frame(&mut self, mic: &mut [f32], spk: &[f32], enable_aec: bool, enable_agc: bool) {
        let frame_duration = self.frame_size as f32 / self.sample_rate as f32;
        let adapt_ref_floor = self.adapt_min_ref_rms * (frame_duration / 0.01).clamp(0.5, 2.0);
        let mut x2 = 0.0f32;
        let mut m2 = 0.0f32;
        let mut cross = 0.0f32;
        for i in 0..mic.len() {
            let sx = if i < spk.len() { spk[i] } else { 0.0 };
            x2 += sx * sx;
            m2 += mic[i] * mic[i];
            cross += sx * mic[i];
        }
        let denom = mic.len().max(1) as f32;
        let ref_rms = (x2 / denom).sqrt();
        let mic_rms_pre = (m2 / denom).sqrt();
        let coh = cross / ((x2 * m2).sqrt() + self.eps);
        let double_talk = coh < self.dt_corr_threshold && mic_rms_pre > self.dt_energy_ratio * ref_rms;
        let allow_adapt = !double_talk && ref_rms > adapt_ref_floor;
        // AEC：NLMS 自适应滤波器，根据扬声器参考信号估计回声并从麦克风中抵消
        if enable_aec {
            for i in 0..mic.len() {
                let pos = self.ref_pos;
                // 更新参考缓冲（单样本推进）
                let spk_sample = if i < spk.len() { spk[i] } else { 0.0 };
                self.ref_buf[pos] = spk_sample;

                // 预测回声 y 与参考能量，用于归一化步长
                let mut y = 0.0f32;
                let mut x_energy = 0.0f32;
                for k in 0..self.filter_len {
                    let idx = if pos >= k { pos - k } else { pos + self.filter_len - k };
                    let x = self.ref_buf[idx];
                    y += self.weights[k] * x;
                    x_energy += x * x;
                }

                // 误差信号 e = mic - y，并用 NLMS 更新滤波器权重
                let e = mic[i] - y;
                let step = if allow_adapt { self.mu * e / (self.eps + x_energy) } else { 0.0 };
                for k in 0..self.filter_len {
                    let idx = if pos >= k { pos - k } else { pos + self.filter_len - k };
                    let x = self.ref_buf[idx];
                    self.weights[k] = self.weights[k] * self.leakage + step * x;
                }
                let pf_th = self.postfilter_threshold_ratio * ref_rms;
                let over = ((y.abs() - pf_th).max(0.0)) / (1.0 - pf_th).max(1e-6);
                let duck = 1.0 - over.clamp(0.0, 1.0) * self.postfilter_max_reduction;
                mic[i] = e * duck;
                self.ref_pos = (pos + 1) % self.filter_len;
            }
        }

        // AGC：RMS 估计后以攻击/释放速率平滑调节增益并限幅
        if enable_agc {
            let mut rms_acc = 0.0f32;
            for &s in mic.iter() {
                rms_acc += s * s;
            }
            let denom = mic.len().max(1) as f32;
            let rms = (rms_acc / denom).sqrt();
            let ns_slope = if rms < self.noise_rms { self.ns_attack } else { self.ns_release };
            self.noise_rms += ns_slope * (rms - self.noise_rms);
            let ns_threshold = self.noise_rms * self.ns_threshold_ratio;
            let ns_ratio = if rms <= ns_threshold && ns_threshold > 1e-6 {
                (rms / ns_threshold).clamp(0.0, 1.0)
            } else {
                1.0
            };
            let ns_gain = 1.0 - (1.0 - ns_ratio) * self.ns_max_reduction;
            let target_gain = if rms > 1e-6 { self.agc_target_rms / rms } else { 1.0 };
            let s = if target_gain > self.agc_gain { self.agc_attack } else { self.agc_release };
            self.agc_gain += s * (target_gain - self.agc_gain);
            self.agc_gain = self.agc_gain.clamp(self.agc_min_gain, self.agc_max_gain);
            let total_gain = ns_gain * self.agc_gain;
            for s in mic.iter_mut() {
                *s = (*s * total_gain).clamp(-1.0, 1.0);
            }
        }
    }
}
