use std::sync::atomic::{AtomicU32, AtomicBool};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use std::fmt;
use std::error::Error;

use chrono::Local;

use napi_derive::napi;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};

pub static SOME_EVENT: OnceLock<Mutex<(String, Instant)>> = OnceLock::new();

pub static GLOBAL_REPORT: OnceLock<ThreadsafeFunction<Vec<SomeInfo>>> = OnceLock::new();
pub static GLOBAL_LOG: OnceLock<ThreadsafeFunction<String>> = OnceLock::new();

// 用于记录后台监控线程的 ID
pub static MONITOR_THREAD_ID: AtomicU32 = AtomicU32::new(0);

pub type AppError = Box<dyn Error + Send + Sync>;
pub type AppResult<T> = Result<T, AppError>;
// 统一目标采样率
pub const TARGET_SAMPLE_RATE: usize = 48000;
// 统一目标通道数 (立体声)
pub const TARGET_CHANNELS: usize = 2;
// Rubato 每次处理的帧数
pub const RESAMPLER_CHUNK_SIZE: usize = 1024;
// 设备采集流的目标缓冲帧数，用于平衡实时性与稳定性
pub const STREAM_BUFFER_FRAMES: u32 = 1024;
// 录制队列允许的最大积压秒数，避免长时间录制时内存持续增长
pub const CHANNEL_QUEUE_MAX_SECONDS: usize = 2;
// 线程间音频包队列长度上限，防止回调阻塞
pub const PACKET_QUEUE_MAX: usize = 128;
// 录制输出采用双通道分离：左声道=麦克风，右声道=扬声器
pub const MIX_SPLIT_CHANNELS: bool = true;
// 混音模式下的增益系数
pub const MIC_MIX_GAIN: f32 = 1.0;
pub const SPK_MIX_GAIN: f32 = 1.0;
// 简易回声抑制的触发阈值与最大衰减比例
pub const ECHO_SUPPRESS_THRESHOLD: f32 = 0.15;
pub const ECHO_SUPPRESS_MAX_REDUCTION: f32 = 0.6;
// 长时间录制时的周期性 flush 间隔
pub const FLUSH_INTERVAL_SECS: u64 = 5;
// 输出 WAV 位深（16-bit PCM）以降低磁盘占用
pub const OUTPUT_BITS_PER_SAMPLE: u16 = 16;
// 16-bit PCM 的抖动幅度，减少量化失真
pub const DITHER_LEVEL: f32 = 1.0 / 32768.0;
// 是否正在录制
pub static IS_RECORDING: AtomicBool = AtomicBool::new(false);
// 在线程间传递的音频数据包
pub struct AudioPacket {
    pub is_mic: bool,
    // 始终是 Interleaved Stereo (L, R, L, R...)
    pub data: Vec<f32>, 
}

#[napi(object)]
#[derive(Clone)]
pub struct SomeInfo {
    pub pname: String,
    pub pid: u32,
    pub title: String,
}

pub fn report_func(info: Vec<SomeInfo>) {
    if let Some(tsfn) = GLOBAL_REPORT.get() {
        tsfn.call(
            Ok(info), ThreadsafeFunctionCallMode::NonBlocking);
    } else {
        println!("Warning: No report wnd listener registered yet!");
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
    "[selection_error]:{} - {}:{}:{} {} - {}",
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
    "[info]:{} - {}:{}:{} {} - {}",
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
