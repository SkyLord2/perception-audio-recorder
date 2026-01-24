#![deny(clippy::all)]
mod global;
mod processor;
mod capture;

use napi_derive::napi;
use napi::threadsafe_function::{ThreadsafeFunction};
use napi::{ Env, Status };

use std::sync::atomic::Ordering;
use std::ffi::c_void;
use std::sync::{Mutex};
use std::time::{Instant, Duration};
use std::thread;

use windows::{
    Win32::Foundation::{WPARAM, LPARAM},
    Win32::System::Threading::GetCurrentThreadId,
    Win32::UI::WindowsAndMessaging::{ 
        PostThreadMessageW, WM_QUIT
    },
};

use crate::global::{
    AppResult, GLOBAL_LOG, GLOBAL_REPORT, IS_RECORDING, MONITOR_THREAD_ID, SOME_EVENT, SomeInfo, TARGET_CHANNELS, 
    TARGET_SAMPLE_RATE, get_current_format_time, report_func, CHANNEL_QUEUE_MAX_SECONDS, PACKET_QUEUE_MAX, 
    ECHO_SUPPRESS_THRESHOLD, ECHO_SUPPRESS_MAX_REDUCTION, FLUSH_INTERVAL_SECS, OUTPUT_BITS_PER_SAMPLE, DITHER_LEVEL,
    RecordingConfig, MixMode, get_recording_config, update_recording_config
};

use cpal::traits::{DeviceTrait, HostTrait};
use crossbeam_channel::bounded;
use hound::WavSpec;
use std::collections::VecDeque;
use nnnoiseless::DenoiseState;
#[cfg(feature = "webrtc_apm")]
use webrtc_audio_processing::{
    Config as WebRtcConfig, EchoCancellation, EchoCancellationSuppressionLevel, GainControl, GainControlMode,
    InitializationConfig, NoiseSuppression, NoiseSuppressionLevel, Processor as WebRtcProcessor, SampleRate,
    NUM_SAMPLES_PER_FRAME,
};

// 【新增】定义清理回调函数
// 这个函数会在 Node.js 环境销毁（Electron 退出）时自动执行
unsafe extern "C" fn cleanup_monitor_thread(_arg: *mut c_void) {
    let thread_id = MONITOR_THREAD_ID.load(Ordering::SeqCst);
    if thread_id != 0 {
        // 向后台线程发送 WM_QUIT，打破它的死循环
        let _ = unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        println!("Cleanup hook triggered: Sent WM_QUIT to monitor thread.");
    }
}

#[napi]
pub fn do_initialize(mut report: ThreadsafeFunction<Vec<SomeInfo>>, mut log: ThreadsafeFunction<String>, env: Env) -> napi::Result<()> {
    #[allow(deprecated)]
    report.unref(&env)?;
    #[allow(deprecated)]
    log.unref(&env)?;
    
    GLOBAL_REPORT.set(report).map_err(|_| napi::Error::new(Status::GenericFailure, "Global report listener already registered"))?;
    GLOBAL_LOG.set(log).map_err(|_| napi::Error::new(Status::GenericFailure, "Global log listener already registered"))?;

    SOME_EVENT.get_or_init(|| Mutex::new((String::from("Ready"), Instant::now() - Duration::from_secs(100))));
    update_recording_config(RecordingConfig::default());


    if cfg!(debug_assertions) {
        report_info_log!("[Debug] 当前正处于开发模式运行，开启详细日志...");
    } else {
        report_info_log!("[Release] 生产模式运行");
    }

    env.add_env_cleanup_hook(
        std::ptr::null_mut(), 
        |arg| unsafe { cleanup_monitor_thread(arg) }
    )?;

    let thread_id = unsafe {
        GetCurrentThreadId()    
    };
    MONITOR_THREAD_ID.store(thread_id, Ordering::SeqCst);

    report_func(vec![SomeInfo {
        pname: "".to_string(),
        pid: 0,
        title: "".to_string()
    }]);

    Ok(())
}

#[napi]
pub fn set_recording_config(config: RecordingConfig) -> napi::Result<()> {
    update_recording_config(config);
    Ok(())
}
#[napi]
pub fn start_record() -> napi::Result<()> {
    if IS_RECORDING.load(Ordering::SeqCst) {
        return Err(napi::Error::new(Status::GenericFailure, "录制已在进行中"));
    }
    IS_RECORDING.store(true, Ordering::SeqCst);
    thread::spawn(move || {
        report_info_log!("开始录制...");
        if let Err(e) = start_record_impl() {
            report_error_log!("录制过程中出错: {:?}", e);
        }
        report_info_log!("录制已结束...");
        IS_RECORDING.store(false, Ordering::SeqCst);
    });
    Ok(())
}

fn start_record_impl() -> AppResult<()> {
    let host = cpal::default_host();
    
    // 这里的 ? 会自动将 &str 转为 Box<dyn Error>，在 AppResult 中是合法的
    let mic_device = host.default_input_device()
        .ok_or("未找到默认麦克风输入设备")?;
        
    let spk_device = host.default_output_device()
        .ok_or("未找到默认扬声器输出设备")?;

    let mic_desc = mic_device
        .description()
        .map(|desc| desc.to_string())
        .unwrap_or_else(|_| "Unknown".to_string());
    let spk_desc = spk_device
        .description()
        .map(|desc| desc.to_string())
        .unwrap_or_else(|_| "Unknown".to_string());

    report_info_log!("麦克风设备: {}", mic_desc);
    report_info_log!("扬声器设备: {}", spk_desc);

    // 使用有界队列避免录制回调阻塞导致积压
    let (tx, rx) = bounded(PACKET_QUEUE_MAX);

    let _mic_stream = capture::start_stream(&mic_device, tx.clone(), true)?;
    let _spk_stream = capture::start_stream(&spk_device, tx.clone(), false)?;

    report_info_log!("录制系统已启动 (注意：这将阻塞 Node 主线程)...");

    let spec = WavSpec {
        channels: TARGET_CHANNELS as u16,
        sample_rate: TARGET_SAMPLE_RATE as u32,
        bits_per_sample: OUTPUT_BITS_PER_SAMPLE,
        sample_format: hound::SampleFormat::Int,
    };

    let filename = format!("output_{}.wav", get_current_format_time("%Y-%m-%d_%H-%M-%S_%3f"));
    let mut writer = hound::WavWriter::create(filename, spec)?;

    let max_queue_samples = TARGET_SAMPLE_RATE * TARGET_CHANNELS * CHANNEL_QUEUE_MAX_SECONDS;
    let mut mic_queue: VecDeque<f32> = VecDeque::with_capacity(max_queue_samples);
    let mut spk_queue: VecDeque<f32> = VecDeque::with_capacity(max_queue_samples);
    let mut last_flush = Instant::now();
    let mut dither_state: u32 = 0x1234_5678;
    let mut dsp = DspProcessor::new(get_recording_config())?;
    let mut last_config = get_recording_config();

    // 信号流图（采集 -> 处理 -> 写盘）
    // Mic/Spk 设备采集
    //   -> AudioProcessor（重采样/高通/内置兜底 NS/AGC）
    //   -> 有界队列聚合
    //   -> DspProcessor（WebRTC AEC/NS/AGC + RNNoise）
    //   -> 简易回声抑制兜底（仅 AEC 关闭时）
    //   -> 混音/分轨与增益
    //   -> 抖动量化（16-bit PCM）
    //   -> WAV Writer

    // 【关键修改】循环条件改为检测 AtomicBool
    while IS_RECORDING.load(Ordering::SeqCst) {
        // 使用 recv_timeout 避免在停止时被永久阻塞
        // 如果 100ms 没收到数据，就检查一下 IS_RECORDING 状态
        if let Ok(packet) = rx.recv_timeout(Duration::from_millis(100)) {
            if packet.is_mic {
                mic_queue.extend(packet.data);
                // 队列过长时丢弃旧数据，保证实时性和稳定性
                while mic_queue.len() > max_queue_samples {
                    mic_queue.pop_front();
                }
            } else {
                spk_queue.extend(packet.data);
                // 队列过长时丢弃旧数据，保证实时性和稳定性
                while spk_queue.len() > max_queue_samples {
                    spk_queue.pop_front();
                }
            }

            let config = get_recording_config();
            if config != last_config {
                dsp.reconfigure(config.clone())?;
                last_config = config.clone();
            }

            let frame_size = dsp.frame_size();
            while mic_queue.len() >= frame_size * TARGET_CHANNELS && spk_queue.len() >= frame_size * TARGET_CHANNELS {
                let mut mic_mono = vec![0.0f32; frame_size];
                let mut spk_mono = vec![0.0f32; frame_size];
                let mut spk_l_buf = vec![0.0f32; frame_size];
                let mut spk_r_buf = vec![0.0f32; frame_size];

                for i in 0..frame_size {
                    let mic_l = mic_queue.pop_front().unwrap_or(0.0);
                    let spk_l = spk_queue.pop_front().unwrap_or(0.0);
                    let mic_r = mic_queue.pop_front().unwrap_or(0.0);
                    let spk_r = spk_queue.pop_front().unwrap_or(0.0);
                    mic_mono[i] = (mic_l + mic_r) * 0.5;
                    spk_mono[i] = (spk_l + spk_r) * 0.5;
                    spk_l_buf[i] = spk_l;
                    spk_r_buf[i] = spk_r;
                }

                dsp.process_mic_frame(&mut mic_mono, &spk_mono, &config)?;

                for i in 0..frame_size {
                    let mut mic_l = mic_mono[i];
                    let mut mic_r = mic_mono[i];
                    let spk_l = spk_l_buf[i];
                    let spk_r = spk_r_buf[i];

                    if !dsp.webrtc_aec_active() {
                        // 简易回声抑制：在未启用 WebRTC AEC 或不可用时降低回授
                        // 默认阈值 0.15、最大衰减 0.6；回授明显可降低阈值或提高衰减，音色变薄可反向调整
                        let spk_abs = spk_l.abs().max(spk_r.abs());
                        if spk_abs > ECHO_SUPPRESS_THRESHOLD {
                            let over = ((spk_abs - ECHO_SUPPRESS_THRESHOLD) / (1.0 - ECHO_SUPPRESS_THRESHOLD))
                                .clamp(0.0, 1.0);
                            let duck = 1.0 - over * ECHO_SUPPRESS_MAX_REDUCTION;
                            mic_l *= duck;
                            mic_r *= duck;
                        }
                    }

                    let mic_gain = config.mic_gain as f32;
                    let spk_gain = config.spk_gain as f32;
                    let (out_l, out_r) = match config.mix_mode {
                        MixMode::Split => {
                            // 分轨：左=麦克风，右=扬声器
                            let mic_out = mic_l * mic_gain;
                            let spk_out = ((spk_l + spk_r) * 0.5) * spk_gain;
                            (mic_out, spk_out)
                        }
                        MixMode::Mix => {
                            let out_l = mic_l * mic_gain + spk_l * spk_gain;
                            let out_r = mic_r * mic_gain + spk_r * spk_gain;
                            (out_l, out_r)
                        }
                    };

                    let sample_l = float_to_i16(out_l, &mut dither_state);
                    let sample_r = float_to_i16(out_r, &mut dither_state);
                    writer.write_sample(sample_l)?;
                    writer.write_sample(sample_r)?;
                }
            }
            if last_flush.elapsed() >= Duration::from_secs(FLUSH_INTERVAL_SECS) {
                writer.flush()?;
                last_flush = Instant::now();
            }
        }
    }

    // 循环结束后，writer 离开作用域时会自动 flush 并关闭文件
    report_info_log!("录制循环结束，文件保存中...");
    writer.flush()?;
    Ok(())
}

#[napi]
pub fn stop_record() -> napi::Result<()> {
    if !IS_RECORDING.load(Ordering::SeqCst) {
        return Err(napi::Error::new(Status::GenericFailure, "录制未在进行中"));
    }
    IS_RECORDING.store(false, Ordering::SeqCst);
    Ok(())
}

fn float_to_i16(sample: f32, state: &mut u32) -> i16 {
    // 轻量抖动：减少量化失真并避免长时间录制的低电平噪声调制
    *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
    let noise = ((*state >> 9) as f32 / (1u32 << 23) as f32) * 2.0 - 1.0;
    let dithered = (sample + noise * DITHER_LEVEL).clamp(-1.0, 1.0);
    (dithered * 32767.0).round() as i16
}

struct DspProcessor {
    #[cfg(feature = "webrtc_apm")]
    webrtc: Option<WebRtcProcessor>,
    rnnoise: Option<Box<DenoiseState>>,
    frame_size: usize,
    // 标记当前是否启用 WebRTC AEC，决定是否走简易回声抑制兜底
    webrtc_aec_active: bool,
    // WebRTC 处理所需的固定帧缓冲
    webrtc_render_buffer: Vec<f32>,
    webrtc_capture_buffer: Vec<f32>,
    #[cfg(feature = "webrtc_apm")]
    webrtc_config: WebRtcConfig,
}

impl DspProcessor {
    fn new(config: RecordingConfig) -> AppResult<Self> {
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
            frame_size,
            webrtc_aec_active: false,
            webrtc_render_buffer: vec![0.0; frame_size],
            webrtc_capture_buffer: vec![0.0; frame_size],
            #[cfg(feature = "webrtc_apm")]
            webrtc_config: WebRtcConfig::default(),
        };
        processor.reconfigure(config)?;
        Ok(processor)
    }

    fn frame_size(&self) -> usize {
        self.frame_size
    }

    fn webrtc_aec_active(&self) -> bool {
        self.webrtc_aec_active
    }

    fn reconfigure(&mut self, config: RecordingConfig) -> AppResult<()> {
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
            self.webrtc_aec_active = self.webrtc.is_some() && config.enable_webrtc_aec;
        }
        #[cfg(not(feature = "webrtc_apm"))]
        {
            self.webrtc_aec_active = false;
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

    fn process_mic_frame(&mut self, mic_mono: &mut [f32], spk_mono: &[f32], config: &RecordingConfig) -> AppResult<()> {
        #[cfg(not(feature = "webrtc_apm"))]
        let _ = spk_mono;
        #[cfg(feature = "webrtc_apm")]
        if let Some(processor) = &mut self.webrtc {
            // WebRTC 处理需要与渲染端对齐的固定帧长
            if mic_mono.len() == self.frame_size && spk_mono.len() == self.frame_size {
                self.webrtc_render_buffer.copy_from_slice(spk_mono);
                self.webrtc_capture_buffer.copy_from_slice(mic_mono);
                processor.process_render_frame(&mut self.webrtc_render_buffer)?;
                processor.process_capture_frame(&mut self.webrtc_capture_buffer)?;
                mic_mono.copy_from_slice(&self.webrtc_capture_buffer);
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
