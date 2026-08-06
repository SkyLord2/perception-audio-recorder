#![deny(clippy::all)]
mod global;
mod processor;
mod capture;

use napi_derive::napi;
use napi::threadsafe_function::{ThreadsafeFunction};
use napi::{ Env, Status };

use std::sync::atomic::Ordering;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::path::PathBuf;
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
    AppResult, CHANNEL_QUEUE_MAX_SECONDS, DISK_FLUSH_INTERVAL_MS, DITHER_LEVEL,
    ENABLE_MIC_ECHO_CANCELLATION, GLOBAL_LOG, IS_PAUSED, IS_RECORDING, MIC_MIX_GAIN,
    MONITOR_THREAD_ID, OUTPUT_BITS_PER_SAMPLE, PACKET_QUEUE_MAX, RECORD_DURATION_MS, RECORDED_DATA_BYTES,
    PROGRESS_REPORT_INTERVAL_MS, RecordDeviceMode, SPK_MIX_GAIN, SOME_EVENT, TARGET_SAMPLE_RATE,
    STOP_REQUESTED, QUEUE_TRIM_COUNT, TRY_SEND_DROP_COUNT,
    emit_pause_recording, emit_recording_error, emit_recording_progress, emit_resume_recording, emit_start_recording, emit_stop_recording,
    parse_start_recording_options,
    get_current_format_time, register_pause_recording_listener, register_progress_listener,
    register_recording_error_listener, register_resume_recording_listener, register_start_recording_listener, register_stop_recording_listener,
    reset_recording_stats, set_current_output_file, RecordingErrorInfo, RecordingProgressInfo, StartRecordingOptions,
};

use cpal::traits::{DeviceTrait, HostTrait};
use crossbeam_channel::bounded;
use crossbeam_channel::RecvTimeoutError;
use hound::WavSpec;
use std::collections::VecDeque;

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
pub fn do_initialize(mut log: ThreadsafeFunction<String>, env: Env) -> napi::Result<()> {
    #[allow(deprecated)]
    log.unref(&env)?;
    
    GLOBAL_LOG.set(log).map_err(|_| napi::Error::new(Status::GenericFailure, "Global log listener already registered"))?;

    SOME_EVENT.get_or_init(|| Mutex::new((String::from("Ready"), Instant::now() - Duration::from_secs(100))));


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

    Ok(())
}
#[napi]
pub fn start_recording(
    save_dir: Option<String>,
    record_device: Option<i32>,
    sample_rate: Option<i32>,
    channels: Option<String>,
    save_to_file: Option<bool>,
) -> napi::Result<()> {
    if IS_RECORDING.load(Ordering::SeqCst) {
        return Err(napi::Error::new(Status::GenericFailure, "录制已在进行中"));
    }
    let options = parse_start_recording_options(save_dir, record_device, sample_rate, channels, save_to_file)
        .map_err(|msg| napi::Error::new(Status::InvalidArg, msg))?;
    reset_recording_stats();
    set_current_output_file(None);
    STOP_REQUESTED.store(false, Ordering::SeqCst);
    IS_RECORDING.store(true, Ordering::SeqCst);
    thread::spawn(move || {
        report_info_log!("开始录制...");
        if let Err(e) = start_record_impl(options) {
            report_error_log!("录制过程中出错: {:?}", e);
        }
        report_info_log!("录制已结束...");
        set_current_output_file(None);
        STOP_REQUESTED.store(false, Ordering::SeqCst);
        IS_PAUSED.store(false, Ordering::SeqCst);
        IS_RECORDING.store(false, Ordering::SeqCst);
        emit_stop_recording();
    });
    Ok(())
}

fn resolve_record_save_dir(save_dir: Option<String>) -> PathBuf {
    if let Some(raw_dir) = save_dir {
        let candidate = raw_dir.trim();
        if !candidate.is_empty() {
            let path = PathBuf::from(candidate);
            if path.is_dir() {
                return path;
            }
            report_info_log!("传入目录不存在或不可用，回退默认目录: {}", candidate);
        }
    }

    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        let default_music_dir = PathBuf::from(user_profile).join("Music");
        if default_music_dir.is_dir() {
            return default_music_dir;
        }
        report_info_log!(
            "默认音乐目录不存在或不可用，回退当前目录: {}",
            default_music_dir.display()
        );
    }

    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn start_record_impl(options: StartRecordingOptions) -> AppResult<()> {
    let host = cpal::default_host();
    let mut mic_device = host.default_input_device();
    let mut speaker_capture_device = host.default_output_device();
    let mut render_reference_device = None;
    let record_device_mode = options.record_device;
    let session_channels = options.channels_mode.channels();
    let mic_only_aec_requested =
        record_device_mode == RecordDeviceMode::MicOnly && ENABLE_MIC_ECHO_CANCELLATION;

    match record_device_mode {
        RecordDeviceMode::MicOnly => {
            if mic_device.is_none() {
                report_error_log!("record_device=0，但麦克风设备不可用，录制无法启动");
                emit_recording_error(
                    "MicDeviceUnavailable",
                    "record_device=0(仅麦克风) 时未找到默认麦克风输入设备",
                    1002,
                );
                return Err("record_device=0，但麦克风设备不可用".into());
            }
            speaker_capture_device = None;
            if mic_only_aec_requested {
                render_reference_device = host.default_output_device();
                if render_reference_device.is_none() {
                    report_info_log!(
                        "record_device=0 且请求 MicOnly-AEC，但未找到默认输出设备，将关闭 AEC 并继续录制麦克风"
                    );
                }
            }
        }
        RecordDeviceMode::SpkOnly => {
            if speaker_capture_device.is_none() {
                report_error_log!("record_device=1，但扬声器设备不可用，录制无法启动");
                emit_recording_error(
                    "SpkDeviceUnavailable",
                    "record_device=1(仅扬声器) 时未找到默认扬声器输出设备",
                    1003,
                );
                return Err("record_device=1，但扬声器设备不可用".into());
            }
            mic_device = None;
        }
        RecordDeviceMode::MicAndSpk => {
            if mic_device.is_none() && speaker_capture_device.is_none() {
                report_error_log!("麦克风与扬声器设备均不可用，录制无法启动");
                emit_recording_error(
                    "BothDevicesUnavailable",
                    "未找到可用的麦克风和扬声器设备，录制无法启动",
                    1001,
                );
                return Err("麦克风与扬声器设备均不可用".into());
            }

            if mic_device.is_none() {
                report_error_log!("麦克风设备不可用，将降级为仅扬声器录制");
                emit_recording_error(
                    "MicDeviceUnavailable",
                    "未找到默认麦克风输入设备，已降级为仅扬声器录制",
                    1002,
                );
            }
            if speaker_capture_device.is_none() {
                report_error_log!("扬声器设备不可用，将降级为仅麦克风录制");
                emit_recording_error(
                    "SpkDeviceUnavailable",
                    "未找到默认扬声器输出设备，已降级为仅麦克风录制",
                    1003,
                );
            }
        }
    }

    let mic_desc = mic_device
        .as_ref()
        .and_then(|device| device.description().ok().map(|desc| desc.to_string()))
        .unwrap_or_else(|| "NotSelectedOrUnavailable".to_string());
    let speaker_capture_desc = speaker_capture_device
        .as_ref()
        .and_then(|device| device.description().ok().map(|desc| desc.to_string()))
        .unwrap_or_else(|| "NotSelectedOrUnavailable".to_string());
    let render_reference_desc = render_reference_device
        .as_ref()
        .and_then(|device| device.description().ok().map(|desc| desc.to_string()))
        .unwrap_or_else(|| "NotSelectedOrUnavailable".to_string());

    report_info_log!("麦克风设备: {}", mic_desc);
    report_info_log!("扬声器业务设备: {}", speaker_capture_desc);
    if record_device_mode == RecordDeviceMode::MicOnly && mic_only_aec_requested {
        report_info_log!("MicOnly-AEC render 参考设备: {}", render_reference_desc);
    }
    report_info_log!(
        "录制参数: record_device={:?}, requested_sample_rate={}, channels={}, save_to_file={}",
        record_device_mode,
        options.requested_sample_rate,
        if session_channels == 1 { "mono" } else { "stereo" },
        options.save_to_file
    );

    // 会话采样率决策：
    // 1) mic_sr == spk_sr -> 双路不重采样，WAV 使用该采样率
    // 2) mic_sr != spk_sr -> 以 mic_sr 为基准，仅 spk 重采样到 mic_sr
    // 3) 单侧可用 -> 使用该侧采样率
    let mic_sr_opt = mic_device.as_ref().and_then(|device| {
        device
            .default_input_config()
            .ok()
            .map(|cfg| cfg.sample_rate() as usize)
    });
    let spk_sr_opt = speaker_capture_device
        .as_ref()
        .or(render_reference_device.as_ref())
        .and_then(|device| {
        device
            .default_output_config()
            .ok()
            .map(|cfg| cfg.sample_rate() as usize)
        });
    let dynamic_sample_rate = if let Some(mic_sr) = mic_sr_opt {
        mic_sr
    } else if let Some(spk_sr) = spk_sr_opt {
        spk_sr
    } else {
        TARGET_SAMPLE_RATE
    };
    let session_sample_rate = options.sample_rate_override.unwrap_or(dynamic_sample_rate);

    report_info_log!(
        "采样率探测: mic_sr={}Hz, spk_sr={}Hz",
        mic_sr_opt
            .map(|v| v.to_string())
            .unwrap_or_else(|| "N/A".to_string()),
        spk_sr_opt
            .map(|v| v.to_string())
            .unwrap_or_else(|| "N/A".to_string())
    );
    if let Some(forced_sr) = options.sample_rate_override {
        report_info_log!(
            "采样率决策: 使用用户指定采样率={}Hz（requested={}）",
            forced_sr,
            options.requested_sample_rate
        );
    } else {
        report_info_log!(
            "采样率决策: requested={} 非法，回退动态策略={}Hz",
            options.requested_sample_rate,
            session_sample_rate
        );
        match (mic_sr_opt, spk_sr_opt) {
            (Some(mic_sr), Some(spk_sr)) if mic_sr == spk_sr => {
                report_info_log!(
                    "动态策略细节: mic_sr == spk_sr，关闭双路重采样，WAV采样率={}Hz",
                    session_sample_rate
                );
            }
            (Some(mic_sr), Some(spk_sr)) => {
                report_info_log!(
                    "动态策略细节: mic_sr({}Hz) != spk_sr({}Hz)，WAV采样率={}Hz，仅Spk重采样",
                    mic_sr,
                    spk_sr,
                    session_sample_rate
                );
            }
            (Some(_), None) => {
                report_info_log!(
                    "动态策略细节: 仅Mic采样率可用，WAV采样率={}Hz，不启用重采样",
                    session_sample_rate
                );
            }
            (None, Some(_)) => {
                report_info_log!(
                    "动态策略细节: 仅Spk采样率可用，WAV采样率={}Hz，不启用重采样",
                    session_sample_rate
                );
            }
            (None, None) => {
                report_info_log!(
                    "动态策略细节: 未探测到有效采样率，回退默认={}Hz",
                    session_sample_rate
                );
            }
        }
    }

    // 使用有界队列避免录制回调阻塞导致积压
    let (tx, rx) = bounded(PACKET_QUEUE_MAX);
    // 仅在会话真实拿到 render 参考流后，才为 mic processor 启用 AEC。
    // 这样 MicOnly 可以先起麦克风，再根据内部 loopback 是否成功决定是否开启 AEC，
    // 避免出现“名义开启但没有 render reference”的半状态。
    let aec_requested_by_mode = ENABLE_MIC_ECHO_CANCELLATION
        && mic_device.is_some()
        && matches!(
            record_device_mode,
            RecordDeviceMode::MicOnly | RecordDeviceMode::MicAndSpk
        );

    let mut mic_started = if let Some(ref device) = mic_device {
        match capture::start_stream(
            device,
            Some(tx.clone()),
            capture::StreamRole::MicCapture,
            session_sample_rate,
            session_channels,
            false,
            None,
        ) {
            Ok(started) => {
                report_info_log!(
                    "Mic流启动成功: source={}Hz, {}ch, target={}Hz",
                    started.sample_rate,
                    started.channels,
                    session_sample_rate
                );
                Some(started)
            }
            Err(e) => {
                report_error_log!("麦克风流启动失败: {:?}", e);
                emit_recording_error(
                    "MicStreamStartFailed",
                    &format!("麦克风流启动失败: {:?}", e),
                    1004,
                );
                None
            }
        }
    } else {
        None
    };
    let mic_processor: Option<Arc<Mutex<crate::processor::AudioProcessor>>> =
        mic_started.as_ref().map(|started| started.processor.clone());
    let mut spk_started = if let Some(ref device) = speaker_capture_device {
        match capture::start_stream(
            device,
            Some(tx.clone()),
            capture::StreamRole::SpeakerCapture,
            session_sample_rate,
            session_channels,
            false,
            mic_processor.clone(),
        ) {
            Ok(started) => {
                report_info_log!(
                    "Spk流启动成功: source={}Hz, {}ch, target={}Hz",
                    started.sample_rate,
                    started.channels,
                    session_sample_rate
                );
                Some(started)
            }
            Err(e) => {
                report_error_log!("扬声器流启动失败: {:?}", e);
                emit_recording_error(
                    "SpkStreamStartFailed",
                    &format!("扬声器流启动失败: {:?}", e),
                    1005,
                );
                None
            }
        }
    } else {
        None
    };
    let mut render_reference_started = if let Some(ref device) = render_reference_device {
        match capture::start_stream(
            device,
            None,
            capture::StreamRole::RenderReference,
            session_sample_rate,
            session_channels,
            false,
            mic_processor.clone(),
        ) {
            Ok(started) => {
                report_info_log!(
                    "MicOnly-AEC render参考流启动成功: source={}Hz, {}ch, target={}Hz",
                    started.sample_rate,
                    started.channels,
                    session_sample_rate
                );
                Some(started)
            }
            Err(e) => {
                report_error_log!("MicOnly-AEC render参考流启动失败: {:?}", e);
                emit_recording_error(
                    "RenderReferenceStartFailed",
                    &format!("MicOnly-AEC render参考流启动失败，已降级关闭AEC: {:?}", e),
                    1009,
                );
                None
            }
        }
    } else {
        None
    };

    let render_reference_active = spk_started.is_some() || render_reference_started.is_some();
    let mut mic_aec_active = false;
    if let Some(mic_processor) = mic_processor.as_ref() {
        match mic_processor.lock() {
            Ok(mut processor) => {
                processor.set_aec_enabled(aec_requested_by_mode && render_reference_active);
                mic_aec_active = processor.aec_enabled();
            }
            Err(err) => {
                report_error_log!("录制启动后获取麦克风处理器锁失败: {:?}", err);
            }
        }
    }

    let _mic_stream = mic_started.take().map(|started| started.stream);
    let _spk_stream = spk_started.take().map(|started| started.stream);
    let _render_reference_stream =
        render_reference_started.take().map(|started| started.stream);

    if _mic_stream.is_none() && _spk_stream.is_none() {
        report_error_log!("麦克风与扬声器流均启动失败，录制无法继续");
        emit_recording_error(
            "BothStreamsStartFailed",
            "麦克风与扬声器流均启动失败，录制无法继续",
            1006,
        );
        return Err("麦克风与扬声器流均启动失败".into());
    }
    let mic_active = _mic_stream.is_some();
    let spk_active = _spk_stream.is_some();
    if aec_requested_by_mode {
        if mic_aec_active {
            match record_device_mode {
                RecordDeviceMode::MicOnly => report_info_log!(
                    "MicOnly-AEC 已启用：内部 render reference 仅供回声消除使用，不进入业务 spkBuffer"
                ),
                RecordDeviceMode::MicAndSpk => report_info_log!(
                    "双路录制 AEC 已启用：业务扬声器流同时承担 render reference"
                ),
                RecordDeviceMode::SpkOnly => {}
            }
        } else {
            match record_device_mode {
                RecordDeviceMode::MicOnly => report_info_log!(
                    "MicOnly-AEC 未生效：缺少可用 render reference，已降级为仅保留降噪/自动增益"
                ),
                RecordDeviceMode::MicAndSpk => report_info_log!(
                    "双路录制 AEC 未生效：缺少可用扬声器参考流，已降级为仅保留降噪/自动增益"
                ),
                RecordDeviceMode::SpkOnly => {}
            }
        }
    }

    report_info_log!("录制系统已启动...");

    // 原生 WAV 始终使用双声道：左声道为麦克风，右声道为扬声器。
    let spec = WavSpec {
        channels: 2,
        sample_rate: session_sample_rate as u32,
        bits_per_sample: OUTPUT_BITS_PER_SAMPLE,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = if options.save_to_file {
        let output_dir = resolve_record_save_dir(options.save_dir.clone());
        let filename = format!("output_{}.wav", get_current_format_time("%Y-%m-%d_%H-%M-%S_%3f"));
        let full_path = output_dir.join(filename);
        let writer = hound::WavWriter::create(&full_path, spec)?;
        set_current_output_file(Some(full_path.to_string_lossy().to_string()));
        report_info_log!("录音目录: {}", output_dir.display());
        report_info_log!("录音文件: {}", full_path.display());
        Some(writer)
    } else {
        if let Some(save_dir) = options.save_dir.as_ref() {
            if !save_dir.trim().is_empty() {
                report_info_log!(
                    "save_to_file=false，本次会话忽略 save_dir 参数: {}",
                    save_dir
                );
            }
        }
        set_current_output_file(None);
        report_info_log!("录音模式: 当前会话仅实时处理音频，不保存本地文件");
        None
    };
    emit_start_recording();

    let max_queue_samples = session_sample_rate * session_channels * CHANNEL_QUEUE_MAX_SECONDS;
    let mut mic_queue: VecDeque<f32> = VecDeque::with_capacity(max_queue_samples);
    // mic 上报队列：承载经 3A 处理后的真实麦克风数据，不参与 PLC/补零链路。
    let mut mic_progress_queue: VecDeque<f32> = VecDeque::with_capacity(max_queue_samples);
    let mut spk_queue: VecDeque<f32> = VecDeque::with_capacity(max_queue_samples);
    let mut dither_state: u32 = 0x1234_5678;
    // 进度回调直接上报 Float32Array，因此这里缓存样本而非字节。
    let mut flush_pcm_chunk: Vec<f32> = Vec::new();
    let mut flush_spk_pcm_chunk: Vec<f32> = Vec::new();
    let mut last_packet_at = Instant::now();
    let mut no_packet_error_reported = false;
    // 启动阶段预热，避免双流初始抖动直接落盘导致前段卡顿。
    const STARTUP_PREROLL_MS: u64 = 800;
    // 启动阶段双流等待超时：超时后允许单侧先推进，避免扬声器静默导致无上报。
    const STARTUP_FALLBACK_MS: u64 = 1500;
    const STARTUP_READY_MS: u64 = 20;
    // 每轮最大处理 20ms，避免突发大块写盘造成听感抽动。
    const MAX_PROCESS_MS: u64 = 20;
    // 当队列积压较大时，允许单轮扩大到 80ms，优先把历史积压消费掉，避免溢出丢帧。
    const MAX_PROCESS_CATCHUP_MS: u64 = 80;
    const CATCHUP_BACKLOG_MS: u64 = 200;
    // 缺包侧短时保持上一帧并快速衰减到静音（简易 PLC），减少抖动齿音。
    const PLC_HOLD_MS: u64 = 8;
    // 恢复录制后短预热，等待双流重新对齐，避免立即写盘造成抖动。
    const RESUME_PREROLL_MS: u64 = 300;
    // 恢复阶段双流等待超时：超时后允许单侧先推进，避免恢复后卡住不出进度。
    const RESUME_FALLBACK_MS: u64 = 600;
    // 恢复后前 20ms 做淡入，降低边界切换点击音。
    const RESUME_FADE_IN_MS: u64 = 20;
    let startup_preroll_until = Instant::now() + Duration::from_millis(STARTUP_PREROLL_MS);
    let startup_fallback_until = Instant::now() + Duration::from_millis(STARTUP_FALLBACK_MS);
    let startup_ready_frames = ((session_sample_rate as u64 * STARTUP_READY_MS) / 1000) as usize;
    let max_process_frames = ((session_sample_rate as u64 * MAX_PROCESS_MS) / 1000).max(1) as usize;
    let max_process_catchup_frames = ((session_sample_rate as u64 * MAX_PROCESS_CATCHUP_MS) / 1000).max(1) as usize;
    let catchup_backlog_frames = ((session_sample_rate as u64 * CATCHUP_BACKLOG_MS) / 1000).max(1) as usize;
    let plc_hold_frames = ((session_sample_rate as u64 * PLC_HOLD_MS) / 1000).max(1) as usize;
    let resume_fade_in_frames = ((session_sample_rate as u64 * RESUME_FADE_IN_MS) / 1000).max(1) as usize;
    let progress_chunk_frames =
        ((session_sample_rate as u64 * PROGRESS_REPORT_INTERVAL_MS) / 1000).max(1) as usize;
    // 主 buffer 固定使用 [mic, spk] 双声道布局。
    let mixed_output_channels = 2usize;
    // micBuffer 和 spkBuffer 仍由 channels 参数决定声道数。
    let independent_progress_samples_per_frame = session_channels;
    let file_bytes_per_frame = session_channels * 2;
    let target_mixed_chunk_samples = progress_chunk_frames * mixed_output_channels;
    let target_independent_chunk_samples =
        progress_chunk_frames * independent_progress_samples_per_frame;
    let mut startup_settled = false;
    let mut resume_settled = true;
    let mut resume_preroll_until = Instant::now();
    let mut resume_fallback_until = Instant::now();
    let mut resume_fade_left_frames = 0usize;
    let mut last_mic_l = 0.0f32;
    let mut last_mic_r = 0.0f32;
    let mut last_spk_l = 0.0f32;
    let mut last_spk_r = 0.0f32;
    let mut mic_missing_frames = 0usize;
    let mut spk_missing_frames = 0usize;

    // 预算限流锚点：将“已写帧数”与“真实时间”绑定，避免写盘速度超前实时导致时长膨胀。
    let mut pacing_anchor_at = Instant::now();
    let mut pacing_anchor_written_frames: u64 = 0;
    let mut total_written_frames: u64 = 0;
    let mut was_paused = false;
    let mut last_disk_flush_at = Instant::now();
    let mut has_unflushed_write = false;
    let mut last_stats_log_at = Instant::now();

    let emit_progress_chunk = |mixed_payload: Vec<f32>,
                               mut mic_payload: Vec<f32>,
                               mut spk_payload: Vec<f32>| -> AppResult<()> {
        if mixed_payload.is_empty() {
            return Ok(());
        }
        let frames_in_chunk = mixed_payload.len() / mixed_output_channels;
        let file_chunk_bytes = (frames_in_chunk * file_bytes_per_frame) as u64;
        let total_data_bytes =
            RECORDED_DATA_BYTES.fetch_add(file_chunk_bytes, Ordering::SeqCst) + file_chunk_bytes;
        let total_frames = total_data_bytes / file_bytes_per_frame as u64;
        let total_duration = (total_frames * 1000) / session_sample_rate as u64;
        let total_size = if options.save_to_file {
            total_data_bytes + 44
        } else {
            total_data_bytes
        };
        RECORD_DURATION_MS.store(total_duration, Ordering::SeqCst);
        match record_device_mode {
            RecordDeviceMode::MicOnly => spk_payload.clear(),
            RecordDeviceMode::SpkOnly => mic_payload.clear(),
            RecordDeviceMode::MicAndSpk => {}
        }
        emit_recording_progress(
            total_duration,
            &mixed_payload,
            &mic_payload,
            &spk_payload,
            total_size,
        );
        Ok(())
    };

    let drain_chunk_exact = |chunk: &mut Vec<f32>, size: usize| -> Vec<f32> {
        let tail = chunk.split_off(size);
        std::mem::replace(chunk, tail)
    };

    fn enqueue_packet(
        packet: crate::global::AudioPacket,
        mic_queue: &mut VecDeque<f32>,
        mic_progress_queue: &mut VecDeque<f32>,
        spk_queue: &mut VecDeque<f32>,
        last_packet_at: &mut Instant,
        no_packet_error_reported: &mut bool,
        max_queue_samples: usize,
    ) {
        *last_packet_at = Instant::now();
        *no_packet_error_reported = false;
        if packet.is_mic {
            let data = packet.data;
            mic_progress_queue.extend(data.iter().copied());
            mic_queue.extend(data);
            while mic_queue.len() > max_queue_samples {
                mic_queue.pop_front();
                QUEUE_TRIM_COUNT.fetch_add(1, Ordering::Relaxed);
            }
            while mic_progress_queue.len() > max_queue_samples {
                mic_progress_queue.pop_front();
            }
        } else {
            spk_queue.extend(packet.data);
            while spk_queue.len() > max_queue_samples {
                spk_queue.pop_front();
                QUEUE_TRIM_COUNT.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    let drain_mic_progress_payload =
        |processed_queue: &mut VecDeque<f32>, target_payload_samples: usize| -> Vec<f32> {
            let read_count = target_payload_samples.min(processed_queue.len());
            let mut payload = Vec::with_capacity(read_count);
            for _ in 0..read_count {
                if let Some(sample) = processed_queue.pop_front() {
                    payload.push(sample);
                }
            }
            payload
        };

    while !STOP_REQUESTED.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(packet) => {
                enqueue_packet(
                    packet,
                    &mut mic_queue,
                    &mut mic_progress_queue,
                    &mut spk_queue,
                    &mut last_packet_at,
                    &mut no_packet_error_reported,
                    max_queue_samples,
                );
            }
            Err(RecvTimeoutError::Timeout) => {
                if !no_packet_error_reported && last_packet_at.elapsed() >= Duration::from_secs(5) {
                    report_error_log!("连续5秒未收到麦克风和扬声器音频数据包");
                    emit_recording_error(
                        "NoAudioPacketTimeout",
                        "连续5秒未收到麦克风和扬声器音频数据包",
                        1007,
                    );
                    no_packet_error_reported = true;
                    last_packet_at = Instant::now();
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                report_error_log!("音频数据通道已断开");
                emit_recording_error("AudioChannelDisconnected", "音频数据通道已断开", 1008);
                break;
            }
        }
        if has_unflushed_write
            && last_disk_flush_at.elapsed() >= Duration::from_millis(DISK_FLUSH_INTERVAL_MS)
        {
            if let Some(writer) = writer.as_mut() {
                writer.flush()?;
            }
            has_unflushed_write = false;
            last_disk_flush_at = Instant::now();
        }
        if last_stats_log_at.elapsed() >= Duration::from_secs(1) {
            // report_info_log!(
            //     "录制统计: try_send_drop_count={}, queue_trim_count={}",
            //     TRY_SEND_DROP_COUNT.load(Ordering::Relaxed),
            //     QUEUE_TRIM_COUNT.load(Ordering::Relaxed)
            // );
            last_stats_log_at = Instant::now();
        }

        let mic_available_frames = mic_queue.len() / session_channels;
        let spk_available_frames = spk_queue.len() / session_channels;
        // 启动预热：等待双流达到最小可用缓冲后再落盘，降低启动抖动写入。
        if !startup_settled {
            let mic_ready = !mic_active || mic_available_frames >= startup_ready_frames;
            let spk_ready = !spk_active || spk_available_frames >= startup_ready_frames;
            let now = Instant::now();
            let dual_ready = mic_ready && spk_ready;
            let fallback_to_single_side = now >= startup_fallback_until && (mic_ready || spk_ready);
            if now < startup_preroll_until || (!dual_ready && !fallback_to_single_side) {
                // 预热期间不累计写盘预算，避免开始写入时出现预算突发。
                pacing_anchor_at = Instant::now();
                pacing_anchor_written_frames = total_written_frames;
                continue;
            }
            if !dual_ready && fallback_to_single_side {
                report_info_log!("启动预热超时，降级为单侧推进（MicReady={}, SpkReady={}）", mic_ready, spk_ready);
            }
            // 预热结束时仅保留最新一小段，丢弃历史积压，避免开始后长时间“追历史”导致抖动。
            let keep_samples = startup_ready_frames * session_channels;
            while mic_queue.len() > keep_samples {
                mic_queue.pop_front();
            }
            while mic_progress_queue.len() > keep_samples {
                mic_progress_queue.pop_front();
            }
            while spk_queue.len() > keep_samples {
                spk_queue.pop_front();
            }
            pacing_anchor_at = Instant::now();
            pacing_anchor_written_frames = total_written_frames;
            startup_settled = true;
            report_info_log!("录制预热完成，开始稳定写盘");
        }

        let frames_to_process = std::cmp::max(mic_available_frames, spk_available_frames);
        if frames_to_process == 0 {
            continue;
        }

        if IS_PAUSED.load(Ordering::SeqCst) {
            // 暂停期间重置节流锚点，避免暂停时“时间预算”继续累积，恢复后突发写盘。
            if !was_paused {
                pacing_anchor_at = Instant::now();
                pacing_anchor_written_frames = total_written_frames;
                was_paused = true;
            }
            mic_queue.clear();
            mic_progress_queue.clear();
            spk_queue.clear();
            continue;
        }
        if was_paused {
            // 仅在“真正恢复后的首轮循环”启动恢复计时，避免长暂停导致恢复瞬间就触发超时降级。
            was_paused = false;
            resume_settled = false;
            resume_preroll_until = Instant::now() + Duration::from_millis(RESUME_PREROLL_MS);
            resume_fallback_until = Instant::now() + Duration::from_millis(RESUME_FALLBACK_MS);
            // 恢复瞬间重置预算锚点，避免把暂停前的时间差折算成可写预算。
            pacing_anchor_at = Instant::now();
            pacing_anchor_written_frames = total_written_frames;
        }

        if !resume_settled {
            let mic_available_frames = mic_queue.len() / session_channels;
            let spk_available_frames = spk_queue.len() / session_channels;
            let mic_ready = !mic_active || mic_available_frames >= startup_ready_frames;
            let spk_ready = !spk_active || spk_available_frames >= startup_ready_frames;
            let now = Instant::now();
            let dual_ready = mic_ready && spk_ready;
            let fallback_to_single_side = now >= resume_fallback_until && (mic_ready || spk_ready);
            if now < resume_preroll_until || (!dual_ready && !fallback_to_single_side) {
                // 恢复预热期间不写盘，不累计预算，避免恢复瞬态抖动落盘。
                pacing_anchor_at = Instant::now();
                pacing_anchor_written_frames = total_written_frames;
                continue;
            }
            if !dual_ready && fallback_to_single_side {
                report_info_log!("恢复预热超时，降级为单侧推进（MicReady={}, SpkReady={}）", mic_ready, spk_ready);
            }
            resume_settled = true;
            resume_fade_left_frames = resume_fade_in_frames;
            report_info_log!("恢复录制预热完成，进入平滑写盘");
        }

        // 以真实时间计算“当前最多允许写入的帧数预算”，防止单侧积压导致超前写盘。
        let elapsed_secs = pacing_anchor_at.elapsed().as_secs_f64();
        let expected_written_frames =
            pacing_anchor_written_frames + (elapsed_secs * session_sample_rate as f64) as u64;
        let budget_frames = expected_written_frames.saturating_sub(total_written_frames);
        let frames_to_process = std::cmp::min(frames_to_process as u64, budget_frames) as usize;
        // 积压过大时放宽单轮处理上限，减少队列裁剪导致的断续与齿音。
        let dynamic_max_frames = if std::cmp::max(mic_available_frames, spk_available_frames) > catchup_backlog_frames {
            max_process_catchup_frames
        } else {
            max_process_frames
        };
        let frames_to_process = std::cmp::min(frames_to_process, dynamic_max_frames);
        if frames_to_process == 0 {
            continue;
        }

        for _ in 0..frames_to_process {
            let mic_has_frame = mic_queue.len() >= session_channels;
            let spk_has_frame = spk_queue.len() >= session_channels;
            let (mic_l, mic_r) = if mic_has_frame {
                let l = mic_queue.pop_front().unwrap_or(0.0);
                let r = if session_channels == 2 {
                    mic_queue.pop_front().unwrap_or(l)
                } else {
                    l
                };
                last_mic_l = l;
                last_mic_r = r;
                mic_missing_frames = 0;
                (l, r)
            } else {
                if mic_missing_frames == 0 {
                    mic_missing_frames = plc_hold_frames;
                }
                if mic_missing_frames > 0 {
                    let gain = mic_missing_frames as f32 / plc_hold_frames as f32;
                    mic_missing_frames -= 1;
                    (last_mic_l * gain, last_mic_r * gain)
                } else {
                    (0.0, 0.0)
                }
            };
            let (spk_l, spk_r) = if spk_has_frame {
                let l = spk_queue.pop_front().unwrap_or(0.0);
                let r = if session_channels == 2 {
                    spk_queue.pop_front().unwrap_or(l)
                } else {
                    l
                };
                last_spk_l = l;
                last_spk_r = r;
                spk_missing_frames = 0;
                (l, r)
            } else {
                if spk_missing_frames == 0 {
                    spk_missing_frames = plc_hold_frames;
                }
                if spk_missing_frames > 0 {
                    let gain = spk_missing_frames as f32 / plc_hold_frames as f32;
                    spk_missing_frames -= 1;
                    (last_spk_l * gain, last_spk_r * gain)
                } else {
                    (0.0, 0.0)
                }
            };

            // 每一路先折叠为一个样本，供固定双声道输出使用：左为麦克风，右为扬声器。
            let mut mic_output_sample = (mic_l + mic_r) * 0.5 * MIC_MIX_GAIN;
            let mut spk_output_sample = (spk_l + spk_r) * 0.5 * SPK_MIX_GAIN;

            if resume_fade_left_frames > 0 {
                let fade_progress =
                    1.0 - (resume_fade_left_frames as f32 / resume_fade_in_frames as f32);
                mic_output_sample *= fade_progress;
                spk_output_sample *= fade_progress;
                resume_fade_left_frames -= 1;
            }

            // 主 WAV 和 progress.buffer 始终使用 [麦克风, 扬声器] 交错双声道。
            if let Some(writer) = writer.as_mut() {
                writer.write_sample(float_to_i16(mic_output_sample, &mut dither_state))?;
                writer.write_sample(float_to_i16(spk_output_sample, &mut dither_state))?;
                has_unflushed_write = true;
            }
            flush_pcm_chunk.push(mic_output_sample);
            flush_pcm_chunk.push(spk_output_sample);

            // spkBuffer 保留 channels 参数定义的独立声道布局。
            if session_channels == 1 {
                flush_spk_pcm_chunk.push((spk_l + spk_r) * 0.5);
            } else {
                flush_spk_pcm_chunk.push(spk_l);
                flush_spk_pcm_chunk.push(spk_r);
            }
        }
        total_written_frames += frames_to_process as u64;
        while flush_pcm_chunk.len() >= target_mixed_chunk_samples {
            let mixed_payload =
                drain_chunk_exact(&mut flush_pcm_chunk, target_mixed_chunk_samples);
            let mic_payload = drain_mic_progress_payload(
                &mut mic_progress_queue,
                target_independent_chunk_samples,
            );
            let spk_payload = if flush_spk_pcm_chunk.len() >= target_independent_chunk_samples {
                drain_chunk_exact(
                    &mut flush_spk_pcm_chunk,
                    target_independent_chunk_samples,
                )
            } else {
                std::mem::take(&mut flush_spk_pcm_chunk)
            };
            emit_progress_chunk(mixed_payload, mic_payload, spk_payload)?;
        }
    }

    report_info_log!("++++++++录制循环结束，文件保存中...");
    if !flush_pcm_chunk.is_empty() {
        let mixed_payload = std::mem::take(&mut flush_pcm_chunk);
        let remaining_frames = mixed_payload.len() / mixed_output_channels;
        let independent_samples = remaining_frames * independent_progress_samples_per_frame;
        let mic_payload =
            drain_mic_progress_payload(&mut mic_progress_queue, independent_samples);
        let spk_payload = std::mem::take(&mut flush_spk_pcm_chunk);
        emit_progress_chunk(mixed_payload, mic_payload, spk_payload)?;
    }
    report_info_log!(
        "录制结束统计: try_send_drop_count={}, queue_trim_count={}",
        TRY_SEND_DROP_COUNT.load(Ordering::Relaxed),
        QUEUE_TRIM_COUNT.load(Ordering::Relaxed)
    );
    if let Some(writer) = writer.as_mut() {
        writer.flush()?;
    }
    Ok(())
}

#[napi]
pub fn stop_recording() -> napi::Result<()> {
    if !IS_RECORDING.load(Ordering::SeqCst) {
        return Err(napi::Error::new(Status::GenericFailure, "录制未在进行中"));
    }
    // 仅发出停止请求，由录制线程完成尾包 flush 与状态切换，避免会话交叉。
    STOP_REQUESTED.store(true, Ordering::SeqCst);
    report_info_log!("++++++++录制正在停止, 完成最后落盘.....");
    Ok(())
}

#[napi]
pub fn pause_recording() -> napi::Result<()> {
    if !IS_RECORDING.load(Ordering::SeqCst) {
        return Err(napi::Error::new(Status::GenericFailure, "录制未在进行中，无法暂停"));
    }
    if IS_PAUSED.load(Ordering::SeqCst) {
        return Err(napi::Error::new(Status::GenericFailure, "录制已处于暂停状态"));
    }
    IS_PAUSED.store(true, Ordering::SeqCst);
    emit_pause_recording();
    report_info_log!("++++++++录制正在暂停.......");
    Ok(())
}

#[napi]
pub fn resume_recording() -> napi::Result<()> {
    if !IS_RECORDING.load(Ordering::SeqCst) {
        return Err(napi::Error::new(Status::GenericFailure, "录制未在进行中，无法恢复"));
    }
    if !IS_PAUSED.load(Ordering::SeqCst) {
        return Err(napi::Error::new(Status::GenericFailure, "录制当前未暂停"));
    }
    IS_PAUSED.store(false, Ordering::SeqCst);
    emit_resume_recording();
    report_info_log!("++++++++录制正在恢复.......");
    Ok(())
}

#[napi]
pub fn is_recording() -> bool {
    IS_RECORDING.load(Ordering::SeqCst)
}

#[napi]
pub fn is_paused() -> bool {
    IS_PAUSED.load(Ordering::SeqCst)
}

#[napi]
pub fn get_record_duration() -> i64 {
    RECORD_DURATION_MS.load(Ordering::SeqCst).min(i64::MAX as u64) as i64
}

#[napi]
pub fn listen_start_recording_audio(listener: ThreadsafeFunction<()>) -> napi::Result<()> {
    register_start_recording_listener(listener);
    Ok(())
}

#[napi]
pub fn listen_stop_recording_audio(listener: ThreadsafeFunction<()>) -> napi::Result<()> {
    register_stop_recording_listener(listener);
    Ok(())
}

#[napi]
pub fn listen_pause_recording_audio(listener: ThreadsafeFunction<()>) -> napi::Result<()> {
    register_pause_recording_listener(listener);
    Ok(())
}

#[napi]
pub fn listen_resume_recording_audio(listener: ThreadsafeFunction<()>) -> napi::Result<()> {
    register_resume_recording_listener(listener);
    Ok(())
}

#[napi]
pub fn listen_recording_progress(listener: ThreadsafeFunction<RecordingProgressInfo>) -> napi::Result<()> {
    register_progress_listener(listener);
    Ok(())
}

#[napi]
pub fn listen_recording_error(listener: ThreadsafeFunction<RecordingErrorInfo>) -> napi::Result<()> {
    register_recording_error_listener(listener);
    Ok(())
}

fn float_to_i16(sample: f32, state: &mut u32) -> i16 {
    // 轻量抖动：减少量化失真并避免长时间录制的低电平噪声调制
    *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
    let noise = ((*state >> 9) as f32 / (1u32 << 23) as f32) * 2.0 - 1.0;
    let dithered = (sample + noise * DITHER_LEVEL).clamp(-1.0, 1.0);
    (dithered * 32767.0).round() as i16
}
