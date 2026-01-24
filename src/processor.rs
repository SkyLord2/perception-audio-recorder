// src/processor.rs

// 修正引入路径：使用 HeapRb (堆分配环形缓冲)
use ringbuf::{HeapRb, Producer, Consumer}; 
use rubato::{Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};
use std::sync::Arc;
use crate::global::{TARGET_CHANNELS, TARGET_SAMPLE_RATE, RESAMPLER_CHUNK_SIZE, AppResult, INTERNAL_PROCESSING_ENABLED};
use std::sync::atomic::Ordering;

pub struct AudioProcessor {
    resampler: Option<SincFixedIn<f32>>,
    
    // 修正：在 ringbuf 0.3 中，split() 返回的 Producer/Consumer 需要知道底层的容器类型。
    // HeapRb<f32> 默认会被 wrap 进 Arc，所以这里泛型参数是 Arc<HeapRb<f32>>
    producer: Producer<f32, Arc<HeapRb<f32>>>,
    consumer: Consumer<f32, Arc<HeapRb<f32>>>,
    
    is_mic: bool,
    use_resampler: bool,
    input_channels: usize,
    scratch_input: Vec<Vec<f32>>, 
    scratch_output: Vec<Vec<f32>>,
    hp_prev_x: Vec<f32>,
    hp_prev_y: Vec<f32>,
    hp_alpha: f32,
    agc_gain: f32,
    agc_target_rms: f32,
    agc_attack: f32,
    agc_release: f32,
    agc_min_gain: f32,
    agc_max_gain: f32,
    ns_noise_rms: f32,
    ns_attack: f32,
    ns_release: f32,
    ns_threshold_ratio: f32,
    ns_max_reduction: f32,
    soft_k: f32,
}

impl AudioProcessor {
    pub fn new(source_sample_rate: usize, input_channels: usize, is_mic: bool) -> AppResult<Self> {
        // --- Rubato 配置 (保持不变) ---
        let params = SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: WindowFunction::BlackmanHarris2,
        };

        let ratio = TARGET_SAMPLE_RATE as f64 / source_sample_rate as f64;
        let use_resampler = source_sample_rate != TARGET_SAMPLE_RATE;
        // 当源采样率已匹配目标采样率时，绕过重采样以降低延迟与失真
        let resampler = if use_resampler {
            Some(SincFixedIn::<f32>::new(
                ratio,
                2.0,
                params,
                RESAMPLER_CHUNK_SIZE, 
                input_channels,
            )?)
        } else {
            None
        };

        // --- Ringbuf 0.3.3 修正初始化 ---
        let buffer_len = RESAMPLER_CHUNK_SIZE * input_channels * 4; 
        
        // 1. 创建堆分配的 RingBuffer
        let rb = HeapRb::<f32>::new(buffer_len);
        
        // 2. split() 会自动将 rb 包装在 Arc 中，并返回两个拥有者
        let (producer, consumer) = rb.split();

        let scratch_input = vec![vec![0.0; RESAMPLER_CHUNK_SIZE]; input_channels];
        let output_frames_max = (RESAMPLER_CHUNK_SIZE as f64 * ratio).ceil() as usize + 10;
        let scratch_output = vec![vec![0.0; output_frames_max]; input_channels];
        let hp_prev_x = vec![0.0; input_channels];
        let hp_prev_y = vec![0.0; input_channels];
        let fc = 80.0;
        let dt = 1.0 / TARGET_SAMPLE_RATE as f32;
        let rc = 1.0 / (2.0 * std::f32::consts::PI * fc);
        let hp_alpha = rc / (rc + dt);

        // 内置处理默认值与调参建议（仅在 enable_internal_processing=true 时生效）：
        // 高通 fc=80Hz：提升人声清晰度；低频保留更多可降到 60Hz
        // AGC target_rms=0.2：提高会更响但易泵动，降低更自然
        // AGC attack=0.05 / release=0.02：attack 大更跟手，release 大更平滑
        // AGC min/max=0.5~4.0：max 越大噪声越易被抬升
        // NS noise_rms=0.01：初始噪声地板估计，嘶声明显可略增
        // NS attack=0.1 / release=0.02：attack 大更快贴合噪声，release 大更稳
        // NS threshold_ratio=1.5 / max_reduction=0.6：比值越低抑制越强，过低易“水声”
        // soft_k=2.0：软限幅强度，数值小更易削顶，数值大更柔和
        Ok(Self {
            resampler,
            producer,
            consumer,
            is_mic,
            use_resampler,
            input_channels,
            scratch_input,
            scratch_output,
            hp_prev_x,
            hp_prev_y,
            hp_alpha,
            agc_gain: 1.0,
            agc_target_rms: 0.2,
            agc_attack: 0.05,
            agc_release: 0.02,
            agc_min_gain: 0.5,
            agc_max_gain: 4.0,
            ns_noise_rms: 0.01,
            ns_attack: 0.1,
            ns_release: 0.02,
            ns_threshold_ratio: 1.5,
            ns_max_reduction: 0.6,
            soft_k: 2.0,
        })
    }

    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        // 修正：0.3.3 中推荐使用 push_slice 提高性能，如果版本不支持可以使用循环 push
        // 这里为了最稳妥的兼容性，我们使用循环 push
        for &sample in input {
            let _ = self.producer.push(sample);
        }

        let mut final_output = Vec::new();
        let needed_samples = RESAMPLER_CHUNK_SIZE * self.input_channels;

        // len() 是 0.3.3 的标准方法
        while self.consumer.len() >= needed_samples {
            // A. Interleaved -> Planar
            for i in 0..RESAMPLER_CHUNK_SIZE {
                for ch in 0..self.input_channels {
                    // pop() 返回 Option<T>
                    let sample = self.consumer.pop().unwrap();
                    self.scratch_input[ch][i] = sample;
                }
            }

            let out_len = if self.use_resampler {
                // B. 重采样：统一到目标采样率
                let (_, out_len) = self.resampler.as_mut().unwrap().process_into_buffer(
                    &self.scratch_input,
                    &mut self.scratch_output,
                    None
                ).expect("Resampling internal error");
                out_len
            } else {
                // B. 采样率一致时直接拷贝，避免不必要的插值失真
                for ch in 0..self.input_channels {
                    self.scratch_output[ch][..RESAMPLER_CHUNK_SIZE].copy_from_slice(&self.scratch_input[ch]);
                }
                RESAMPLER_CHUNK_SIZE
            };

            if self.is_mic {
                // C. 仅对麦克风执行高通，降低低频轰鸣并提升语音清晰度
                let mut rms_acc = 0.0f32;
                for ch in 0..self.input_channels {
                    for i in 0..out_len {
                        let x = self.scratch_output[ch][i];
                        let y = self.hp_alpha * (self.hp_prev_y[ch] + x - self.hp_prev_x[ch]);
                        self.hp_prev_x[ch] = x;
                        self.hp_prev_y[ch] = y;
                        self.scratch_output[ch][i] = y;
                    }
                }

                if INTERNAL_PROCESSING_ENABLED.load(Ordering::SeqCst) {
                    // 内置降噪 + AGC：当未启用 WebRTC/RNNoise 时作为轻量兜底
                    for ch in 0..self.input_channels {
                        for i in 0..out_len {
                            let y = self.scratch_output[ch][i];
                            rms_acc += y * y;
                        }
                    }
                    let denom = (out_len * self.input_channels).max(1) as f32;
                    let rms = (rms_acc / denom).sqrt();

                    // 自适应噪声估计：低电平时更快贴合噪声地板
                    let ns_slope = if rms < self.ns_noise_rms { self.ns_attack } else { self.ns_release };
                    self.ns_noise_rms += ns_slope * (rms - self.ns_noise_rms);
                    let ns_threshold = self.ns_noise_rms * self.ns_threshold_ratio;
                    let ns_ratio = if rms <= ns_threshold && ns_threshold > 1e-6 {
                        (rms / ns_threshold).clamp(0.0, 1.0)
                    } else {
                        1.0
                    };
                    let ns_gain = 1.0 - (1.0 - ns_ratio) * self.ns_max_reduction;

                    // 自动增益控制：将 RMS 拉向目标区间，限制最大增益避免噪声放大
                    let target_gain = if rms > 1e-6 { self.agc_target_rms / rms } else { 1.0 };
                    let s = if target_gain > self.agc_gain { self.agc_attack } else { self.agc_release };
                    self.agc_gain += s * (target_gain - self.agc_gain);
                    self.agc_gain = self.agc_gain.clamp(self.agc_min_gain, self.agc_max_gain);
                    let total_gain = ns_gain * self.agc_gain;

                    for ch in 0..self.input_channels {
                        for i in 0..out_len {
                            self.scratch_output[ch][i] *= total_gain;
                        }
                    }
                }
            }

            // C. Planar -> Interleaved & Channel Mapping
            let start_idx = final_output.len();
            final_output.resize(start_idx + out_len * TARGET_CHANNELS, 0.0);
            
            for i in 0..out_len {
                let left_sample;
                let right_sample;

                if self.input_channels == 1 {
                    let s = self.scratch_output[0][i];
                    left_sample = s;
                    right_sample = s;
                } else {
                    left_sample = self.scratch_output[0][i];
                    right_sample = if self.input_channels > 1 { self.scratch_output[1][i] } else { left_sample };
                }

                // 仅对麦克风通道进行软限幅，避免削波失真
                let (l, r) = if self.is_mic && INTERNAL_PROCESSING_ENABLED.load(Ordering::SeqCst) {
                    let l = (self.soft_k * left_sample).tanh() / self.soft_k.tanh();
                    let r = (self.soft_k * right_sample).tanh() / self.soft_k.tanh();
                    (l, r)
                } else {
                    (left_sample, right_sample)
                };
                final_output[start_idx + i * 2] = l;
                final_output[start_idx + i * 2 + 1] = r;
            }
        }

        final_output
    }
}
