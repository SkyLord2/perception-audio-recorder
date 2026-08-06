// src/capture.rs
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{BufferSize, SupportedBufferSize};
use crossbeam_channel::{Sender, TrySendError};
use std::sync::{Arc, Mutex};

use crate::global::{AudioPacket, AppResult, STREAM_BUFFER_FRAMES, TRY_SEND_DROP_COUNT}; // 引入自定义 Result
use crate::processor::AudioProcessor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamRole {
    MicCapture,
    SpeakerCapture,
    RenderReference,
}

impl StreamRole {
    fn uses_input_config(self) -> bool {
        matches!(self, Self::MicCapture)
    }

    fn emits_frames(self) -> bool {
        !matches!(self, Self::RenderReference)
    }

    fn is_mic_packet(self) -> bool {
        matches!(self, Self::MicCapture)
    }

    fn feeds_render_target(self) -> bool {
        matches!(self, Self::SpeakerCapture | Self::RenderReference)
    }

    fn label(self) -> &'static str {
        match self {
            Self::MicCapture => "Mic",
            Self::SpeakerCapture => "Spk",
            Self::RenderReference => "SpkLoopback",
        }
    }
}

pub struct StartedStream {
    pub stream: cpal::Stream,
    pub sample_rate: usize,
    pub channels: usize,
    pub processor: Arc<Mutex<AudioProcessor>>,
}

pub fn start_stream(
    device: &cpal::Device,
    tx: Option<Sender<AudioPacket>>,
    role: StreamRole,
    target_sample_rate: usize,
    output_channels: usize,
    enable_aec: bool,
    mic_render_target: Option<Arc<Mutex<AudioProcessor>>>,
) -> AppResult<StartedStream> {
    let config = if role.uses_input_config() {
        device.default_input_config()?
    } else {
        device.default_output_config()?
    };
    
    let sample_rate = config.sample_rate() as usize;
    let channels = config.channels() as usize;
    let is_mic = role.is_mic_packet();
    
    crate::report_info_log!(
        "配置设备 [{}]: {}Hz, {}ch, target={}Hz, resample={}",
        role.label(),
        sample_rate,
        channels,
        target_sample_rate,
        if sample_rate != target_sample_rate { "on" } else { "off" }
    );

    let processor = Arc::new(Mutex::new(AudioProcessor::new(
        sample_rate,
        target_sample_rate,
        channels,
        output_channels,
        is_mic,
        enable_aec,
    )?));

    let mut stream_config: cpal::StreamConfig = config.clone().into();
    // 将缓冲大小限制在设备支持范围内，避免回调抖动导致的丢帧
    stream_config.buffer_size = choose_buffer_size(&config);
    let device_tag = role.label();
    let err_fn = move |err| eprintln!("Stream error [{}]: {}", device_tag, err);

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => build_callback::<f32>(
            device,
            &stream_config,
            tx.clone(),
            role,
            processor.clone(),
            mic_render_target,
            err_fn,
        ),
        cpal::SampleFormat::I16 => build_callback::<i16>(
            device,
            &stream_config,
            tx.clone(),
            role,
            processor.clone(),
            mic_render_target,
            err_fn,
        ),
        cpal::SampleFormat::U16 => build_callback::<u16>(
            device,
            &stream_config,
            tx,
            role,
            processor.clone(),
            mic_render_target,
            err_fn,
        ),
        // 将字符串错误转换为 Box<dyn Error>
        _ => return Err("不支持的采样格式".into()), 
    }?;

    stream.play()?;
    Ok(StartedStream {
        stream,
        sample_rate,
        channels,
        processor,
    })
}

fn build_callback<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    tx: Option<Sender<AudioPacket>>,
    role: StreamRole,
    processor: Arc<Mutex<AudioProcessor>>,
    mic_render_target: Option<Arc<Mutex<AudioProcessor>>>,
    err_fn: impl Fn(cpal::StreamError) + Send + 'static,
) -> AppResult<cpal::Stream>
where
    T: cpal::Sample + cpal::SizedSample,
    f32: From<T>,
{
    let callback = move |data: &[T], _: &_| {
        let float_samples: Vec<f32> = data.iter().map(|&s| f32::from(s)).collect();

        let processed_data = match processor.lock() {
            Ok(mut proc) => {
                if role.is_mic_packet() {
                    proc.process_capture(&float_samples)
                } else {
                    proc.process_render(&float_samples)
                }
            }
            Err(err) => {
                crate::report_error_log!("{} 处理器锁获取失败: {:?}", role.label(), err);
                return;
            }
        };

        match processed_data {
            Ok(processed_data) => {
                if role.feeds_render_target() && !processed_data.is_empty() {
                    if let Some(target) = mic_render_target.as_ref() {
                        match target.lock() {
                            Ok(mut mic_proc) => {
                                if let Err(err) = mic_proc.feed_render_frame(&processed_data) {
                                    crate::report_error_log!("扬声器 render 参考喂入失败: {:?}", err);
                                }
                            }
                            Err(err) => {
                                crate::report_error_log!("麦克风处理器锁获取失败: {:?}", err);
                            }
                        }
                    }
                }

                if role.emits_frames() && !processed_data.is_empty() {
                    // 队列满时直接丢弃当前包，避免长时间录制时回调阻塞
                    if let Some(tx) = tx.as_ref() {
                        match tx.try_send(AudioPacket {
                            is_mic: role.is_mic_packet(),
                            data: processed_data,
                        }) {
                            Ok(_) => {}
                            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                                TRY_SEND_DROP_COUNT
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                    }
                }
            }
            Err(err) => {
                crate::report_error_log!(
                    "{} 数据处理失败: {:?}",
                    role.label(),
                    err
                );
            }
        }
    };

    let stream = device.build_input_stream(config, callback, err_fn, None)?;
    Ok(stream)
}

fn choose_buffer_size(config: &cpal::SupportedStreamConfig) -> BufferSize {
    match config.buffer_size() {
        SupportedBufferSize::Range { min, max } => {
            let target = STREAM_BUFFER_FRAMES;
            let fixed = target.clamp(*min, *max);
            BufferSize::Fixed(fixed)
        }
        SupportedBufferSize::Unknown => BufferSize::Default,
    }
}
