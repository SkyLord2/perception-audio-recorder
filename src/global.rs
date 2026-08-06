use std::sync::atomic::{AtomicU32, AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use std::fmt;
use std::error::Error;

use chrono::Local;

use napi::bindgen_prelude::Float32Array;
use napi_derive::napi;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};

pub static SOME_EVENT: OnceLock<Mutex<(String, Instant)>> = OnceLock::new();

pub static GLOBAL_LOG: OnceLock<ThreadsafeFunction<String>> = OnceLock::new();
pub static START_RECORDING_LISTENERS: OnceLock<Mutex<Vec<ThreadsafeFunction<()>>>> = OnceLock::new();
pub static STOP_RECORDING_LISTENERS: OnceLock<Mutex<Vec<ThreadsafeFunction<()>>>> = OnceLock::new();
pub static PAUSE_RECORDING_LISTENERS: OnceLock<Mutex<Vec<ThreadsafeFunction<()>>>> = OnceLock::new();
pub static RESUME_RECORDING_LISTENERS: OnceLock<Mutex<Vec<ThreadsafeFunction<()>>>> = OnceLock::new();
pub static PROGRESS_LISTENERS: OnceLock<Mutex<Vec<ThreadsafeFunction<RecordingProgressInfo>>>> = OnceLock::new();
pub static RECORDING_ERROR_LISTENERS: OnceLock<Mutex<Vec<ThreadsafeFunction<RecordingErrorInfo>>>> = OnceLock::new();
pub static CURRENT_OUTPUT_FILE: OnceLock<Mutex<Option<String>>> = OnceLock::new();

// 用于记录后台监控线程的 ID
pub static MONITOR_THREAD_ID: AtomicU32 = AtomicU32::new(0);

pub type AppError = Box<dyn Error + Send + Sync>;
pub type AppResult<T> = Result<T, AppError>;
// 统一目标采样率
pub const TARGET_SAMPLE_RATE: usize = 48000;
// Rubato 每次处理的帧数
pub const RESAMPLER_CHUNK_SIZE: usize = 1024;
// 设备采集流的目标缓冲帧数，用于平衡实时性与稳定性
pub const STREAM_BUFFER_FRAMES: u32 = 1024;
// 录制队列允许的最大积压秒数，避免长时间录制时内存持续增长
pub const CHANNEL_QUEUE_MAX_SECONDS: usize = 2;
// 线程间音频包队列长度上限，防止回调阻塞
pub const PACKET_QUEUE_MAX: usize = 128;
// 录制进度上报间隔（毫秒）
pub const PROGRESS_REPORT_INTERVAL_MS: u64 = 40;
// 向磁盘执行强制 flush 的间隔（毫秒）
pub const DISK_FLUSH_INTERVAL_MS: u64 = 1000;
// 录制输出采用双通道分离：左声道=麦克风，右声道=扬声器
// 混音模式下的增益系数
pub const MIC_MIX_GAIN: f32 = 1.0;
pub const SPK_MIX_GAIN: f32 = 1.0;
// 麦克风 3A 处理开关：仅作用于麦克风链路。
pub const ENABLE_MIC_NOISE_SUPPRESSION: bool = true;
// AEC 依赖扬声器 render 参考流，缺失时会自动降级为关闭。
pub const ENABLE_MIC_ECHO_CANCELLATION: bool = true;
// 自动增益作用于经过重采样后的麦克风数据，并影响写盘与 micBuffer 上报。
pub const ENABLE_MIC_AUTO_GAIN_CONTROL: bool = true;
// 输出 WAV 位深（16-bit PCM）以降低磁盘占用
pub const OUTPUT_BITS_PER_SAMPLE: u16 = 16;
// 16-bit PCM 的抖动幅度，减少量化失真
pub const DITHER_LEVEL: f32 = 1.0 / 32768.0;
// 是否正在录制
pub static IS_RECORDING: AtomicBool = AtomicBool::new(false);
// 是否收到停止请求（与 IS_RECORDING 分离，避免 stop 后旧线程未退出就被重新 start）
pub static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
// 是否处于暂停态
pub static IS_PAUSED: AtomicBool = AtomicBool::new(false);
// 本次会话累计时长（毫秒，不含暂停时段）
pub static RECORD_DURATION_MS: AtomicU64 = AtomicU64::new(0);
// 本次会话累计写入数据量（仅 PCM 数据区，不含 WAV 头）
pub static RECORDED_DATA_BYTES: AtomicU64 = AtomicU64::new(0);
// 采集回调向队列 try_send 失败（队列满/断开）累计次数
pub static TRY_SEND_DROP_COUNT: AtomicU64 = AtomicU64::new(0);
// 队列超限后 pop_front 裁剪累计次数
pub static QUEUE_TRIM_COUNT: AtomicU64 = AtomicU64::new(0);
// 在线程间传递的音频数据包
pub struct AudioPacket {
    pub is_mic: bool,
    // 始终是 Interleaved Stereo (L, R, L, R...)
    pub data: Vec<f32>, 
}

#[napi(object)]
pub struct RecordingProgressInfo {
    pub total_duration: i64,
    pub buffer: Float32Array,
    pub mic_buffer: Float32Array,
    pub spk_buffer: Float32Array,
    pub total_size: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordDeviceMode {
    MicOnly,
    SpkOnly,
    MicAndSpk,
}

impl RecordDeviceMode {
    pub fn from_i32(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::MicOnly),
            1 => Some(Self::SpkOnly),
            2 => Some(Self::MicAndSpk),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelMode {
    Mono,
    Stereo,
}

impl ChannelMode {
    pub fn from_str(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "mono" => Some(Self::Mono),
            "stereo" => Some(Self::Stereo),
            _ => None,
        }
    }

    pub fn channels(self) -> usize {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
        }
    }
}

#[derive(Clone, Debug)]
pub struct StartRecordingOptions {
    pub save_dir: Option<String>,
    pub save_to_file: bool,
    pub record_device: RecordDeviceMode,
    pub requested_sample_rate: i32,
    pub sample_rate_override: Option<usize>,
    pub channels_mode: ChannelMode,
}

pub fn parse_start_recording_options(
    save_dir: Option<String>,
    record_device: Option<i32>,
    sample_rate: Option<i32>,
    channels: Option<String>,
    save_to_file: Option<bool>,
) -> Result<StartRecordingOptions, String> {
    let record_device_value = record_device.unwrap_or(0);
    let record_device_mode = RecordDeviceMode::from_i32(record_device_value)
        .ok_or_else(|| "record_device 仅支持 0(麦克风)/1(扬声器)/2(双路)".to_string())?;

    let channels_value = channels.unwrap_or_else(|| "mono".to_string());
    let channels_mode = ChannelMode::from_str(channels_value.trim())
        .ok_or_else(|| "channels 仅支持 \"mono\" 或 \"stereo\"".to_string())?;

    let requested_sample_rate = sample_rate.unwrap_or(16000);
    let sample_rate_override = match requested_sample_rate {
        8000 | 16000 | 32000 | 48000 => Some(requested_sample_rate as usize),
        _ => None,
    };
    let save_to_file = save_to_file.unwrap_or(true);

    Ok(StartRecordingOptions {
        save_dir,
        save_to_file,
        record_device: record_device_mode,
        requested_sample_rate,
        sample_rate_override,
        channels_mode,
    })
}

#[napi(object)]
pub struct RecordingErrorInfo {
    pub error_type: String,
    pub error_message: String,
    pub error_code: i32,
    pub occurred_at: String,
}

pub fn register_start_recording_listener(listener: ThreadsafeFunction<()>) {
    START_RECORDING_LISTENERS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("start listener mutex poisoned")
        .push(listener);
}

pub fn register_stop_recording_listener(listener: ThreadsafeFunction<()>) {
    STOP_RECORDING_LISTENERS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("stop listener mutex poisoned")
        .push(listener);
}

pub fn register_pause_recording_listener(listener: ThreadsafeFunction<()>) {
    PAUSE_RECORDING_LISTENERS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("pause listener mutex poisoned")
        .push(listener);
}

pub fn register_resume_recording_listener(listener: ThreadsafeFunction<()>) {
    RESUME_RECORDING_LISTENERS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("resume listener mutex poisoned")
        .push(listener);
}

pub fn register_progress_listener(listener: ThreadsafeFunction<RecordingProgressInfo>) {
    PROGRESS_LISTENERS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("progress listener mutex poisoned")
        .push(listener);
}

pub fn register_recording_error_listener(listener: ThreadsafeFunction<RecordingErrorInfo>) {
    RECORDING_ERROR_LISTENERS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("recording error listener mutex poisoned")
        .push(listener);
}

pub fn set_current_output_file(path: Option<String>) {
    let file_slot = CURRENT_OUTPUT_FILE.get_or_init(|| Mutex::new(None));
    if let Ok(mut file_guard) = file_slot.lock() {
        *file_guard = path;
    }
}

pub fn reset_recording_stats() {
    STOP_REQUESTED.store(false, Ordering::SeqCst);
    IS_PAUSED.store(false, Ordering::SeqCst);
    RECORD_DURATION_MS.store(0, Ordering::SeqCst);
    RECORDED_DATA_BYTES.store(0, Ordering::SeqCst);
    TRY_SEND_DROP_COUNT.store(0, Ordering::SeqCst);
    QUEUE_TRIM_COUNT.store(0, Ordering::SeqCst);
}

pub fn emit_start_recording() {
    if let Some(listeners) = START_RECORDING_LISTENERS.get() {
        if let Ok(guard) = listeners.lock() {
            for listener in guard.iter() {
                let _ = listener.call(Ok(()), ThreadsafeFunctionCallMode::NonBlocking);
            }
        }
    }
}

pub fn emit_stop_recording() {
    if let Some(listeners) = STOP_RECORDING_LISTENERS.get() {
        if let Ok(guard) = listeners.lock() {
            for listener in guard.iter() {
                let _ = listener.call(Ok(()), ThreadsafeFunctionCallMode::NonBlocking);
            }
        }
    }
}

pub fn emit_pause_recording() {
    if let Some(listeners) = PAUSE_RECORDING_LISTENERS.get() {
        if let Ok(guard) = listeners.lock() {
            for listener in guard.iter() {
                let _ = listener.call(Ok(()), ThreadsafeFunctionCallMode::NonBlocking);
            }
        }
    }
}

pub fn emit_resume_recording() {
    if let Some(listeners) = RESUME_RECORDING_LISTENERS.get() {
        if let Ok(guard) = listeners.lock() {
            for listener in guard.iter() {
                let _ = listener.call(Ok(()), ThreadsafeFunctionCallMode::NonBlocking);
            }
        }
    }
}

pub fn emit_recording_progress(
    total_duration: u64,
    mixed_samples: &[f32],
    mic_samples: &[f32],
    spk_samples: &[f32],
    total_size: u64,
) {
    if let Some(listeners) = PROGRESS_LISTENERS.get() {
        if let Ok(guard) = listeners.lock() {
            for listener in guard.iter() {
                let progress = RecordingProgressInfo {
                    total_duration: total_duration.min(i64::MAX as u64) as i64,
                    // 进度回调直接暴露 Float32 样本数组，避免 JS 侧重复执行字节解包。
                    buffer: Float32Array::new(mixed_samples.to_vec()),
                    mic_buffer: Float32Array::new(mic_samples.to_vec()),
                    spk_buffer: Float32Array::new(spk_samples.to_vec()),
                    total_size: total_size.min(i64::MAX as u64) as i64,
                };
                let _ = listener.call(Ok(progress), ThreadsafeFunctionCallMode::NonBlocking);
            }
        }
    }
}

pub fn emit_recording_error(error_type: &str, error_message: &str, error_code: i32) {
    if let Some(listeners) = RECORDING_ERROR_LISTENERS.get() {
        if let Ok(guard) = listeners.lock() {
            for listener in guard.iter() {
                let error_info = RecordingErrorInfo {
                    error_type: error_type.to_string(),
                    error_message: error_message.to_string(),
                    error_code,
                    occurred_at: get_current_time(),
                };
                let _ = listener.call(Ok(error_info), ThreadsafeFunctionCallMode::NonBlocking);
            }
        } else {
            report_log("录音错误监听器锁获取失败".to_string());
        }
    }
}

fn report_log(msg: String) {
    if cfg!(debug_assertions) {
        println!("{}", msg);
    } else if let Some(tsfn) = GLOBAL_LOG.get() {
        tsfn.call(Ok(msg), ThreadsafeFunctionCallMode::NonBlocking);
    } else {
        println!("Warning: No report log listener registered yet!");
    }
}

#[doc(hidden)]
pub(crate) fn report_error(
    msg: fmt::Arguments,
    module_path: &'static str,
    file: &'static str,
    line: u32,
    column: u32,
) {
  let curr_time = get_current_time();
  let log_msg = format!(
    "[audio record error]:{} - {}:{}:{} {} - {}",
    curr_time, file, line, column, module_path, msg
  );
  report_log(log_msg);
}

#[doc(hidden)]
pub(crate) fn report_info(
    msg: fmt::Arguments,
    module_path: &'static str,
    file: &'static str,
    line: u32,
    column: u32,
) {
  let curr_time = get_current_time();
  let log_msg = format!(
    "[audio record info]:{} - {}:{}:{} {} - {}",
    curr_time, file, line, column, module_path, msg
  );
  report_log(log_msg);
}

#[macro_export]
macro_rules! report_error_log {
    // format_args! 是编译器内置宏，它不分配内存，只打包参数
    ($($arg:tt)*) => {
        $crate::global::report_error(
            format_args!($($arg)*),
            module_path!(),
            file!(),
            line!(),
            column!(),
        )
    }
}

#[macro_export]
macro_rules! report_info_log {
    // format_args! 是编译器内置宏，它不分配内存，只打包参数
    ($($arg:tt)*) => {
        $crate::global::report_info(
            format_args!($($arg)*),
            module_path!(),
            file!(),
            line!(),
            column!(),
        )
    }
}

pub fn get_current_time() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S.%3f").to_string()
}

pub fn get_current_format_time(format: &str) -> String {
    Local::now().format(format).to_string()
}
