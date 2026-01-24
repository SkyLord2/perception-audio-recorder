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
    MIX_SPLIT_CHANNELS, MIC_MIX_GAIN, SPK_MIX_GAIN, ECHO_SUPPRESS_THRESHOLD, ECHO_SUPPRESS_MAX_REDUCTION, 
    FLUSH_INTERVAL_SECS, OUTPUT_BITS_PER_SAMPLE, DITHER_LEVEL
};

use cpal::traits::{DeviceTrait, HostTrait};
use crossbeam_channel::bounded;
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
pub fn do_initialize(mut report: ThreadsafeFunction<Vec<SomeInfo>>, mut log: ThreadsafeFunction<String>, env: Env) -> napi::Result<()> {
    #[allow(deprecated)]
    report.unref(&env)?;
    #[allow(deprecated)]
    log.unref(&env)?;
    
    GLOBAL_REPORT.set(report).map_err(|_| napi::Error::new(Status::GenericFailure, "Global report listener already registered"))?;
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

    report_func(vec![SomeInfo {
        pname: "".to_string(),
        pid: 0,
        title: "".to_string()
    }]);

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

            let available_frames = std::cmp::min(mic_queue.len(), spk_queue.len()) / TARGET_CHANNELS;

            for _ in 0..available_frames {
                let mut mic_l = mic_queue.pop_front().unwrap_or(0.0);
                let spk_l = spk_queue.pop_front().unwrap_or(0.0);
                
                let mut mic_r = mic_queue.pop_front().unwrap_or(0.0);
                let spk_r = spk_queue.pop_front().unwrap_or(0.0);

                // 简易回声抑制：扬声器越响，麦克风衰减越多
                let spk_abs = spk_l.abs().max(spk_r.abs());
                if spk_abs > ECHO_SUPPRESS_THRESHOLD {
                    let over = ((spk_abs - ECHO_SUPPRESS_THRESHOLD) / (1.0 - ECHO_SUPPRESS_THRESHOLD)).clamp(0.0, 1.0);
                    let duck = 1.0 - over * ECHO_SUPPRESS_MAX_REDUCTION;
                    mic_l *= duck;
                    mic_r *= duck;
                }

                let (out_l, out_r) = if MIX_SPLIT_CHANNELS {
                    // 双通道分离：左=麦克风，右=扬声器（均转为单声道）
                    let mic_mono = (mic_l + mic_r) * 0.5 * MIC_MIX_GAIN;
                    let spk_mono = (spk_l + spk_r) * 0.5 * SPK_MIX_GAIN;
                    (mic_mono, spk_mono)
                } else {
                    let out_l = mic_l * MIC_MIX_GAIN + spk_l * SPK_MIX_GAIN;
                    let out_r = mic_r * MIC_MIX_GAIN + spk_r * SPK_MIX_GAIN;
                    (out_l, out_r)
                };

                let sample_l = float_to_i16(out_l, &mut dither_state);
                let sample_r = float_to_i16(out_r, &mut dither_state);
                writer.write_sample(sample_l)?;
                writer.write_sample(sample_r)?;
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
