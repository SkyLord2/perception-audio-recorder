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
