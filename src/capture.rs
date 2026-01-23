// src/capture.rs
use cpal::traits::{DeviceTrait, StreamTrait};
use crossbeam_channel::Sender;
use std::sync::{Arc, Mutex};

use crate::global::{AudioPacket, AppResult}; // 引入自定义 Result
use crate::processor::AudioProcessor;

pub fn start_stream(
    device: &cpal::Device,
    tx: Sender<AudioPacket>,
    is_mic: bool,
) -> AppResult<cpal::Stream> {
    let config = if is_mic {
        device.default_input_config()?
    } else {
        device.default_output_config()?
    };
    
    let sample_rate = config.sample_rate() as usize;
    let channels = config.channels() as usize;
    
    println!("配置设备 [{}]: {}Hz, {}ch", if is_mic {"Mic"} else {"Spk"}, sample_rate, channels);

    // AudioProcessor::new 现在返回 AppResult，? 可以直接工作
    let processor = Arc::new(Mutex::new(AudioProcessor::new(sample_rate, channels)?));

    let stream_config: cpal::StreamConfig = config.clone().into();
    let err_fn = move |err| eprintln!("Stream error: {}", err);

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => build_callback::<f32>(device, &stream_config, tx, is_mic, processor, err_fn),
        cpal::SampleFormat::I16 => build_callback::<i16>(device, &stream_config, tx, is_mic, processor, err_fn),
        cpal::SampleFormat::U16 => build_callback::<u16>(device, &stream_config, tx, is_mic, processor, err_fn),
        // 将字符串错误转换为 Box<dyn Error>
        _ => return Err("不支持的采样格式".into()), 
    }?;

    stream.play()?;
    Ok(stream)
}

fn build_callback<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    tx: Sender<AudioPacket>,
    is_mic: bool,
    processor: Arc<Mutex<AudioProcessor>>, 
    err_fn: impl Fn(cpal::StreamError) + Send + 'static,
) -> AppResult<cpal::Stream>
where
    T: cpal::Sample + cpal::SizedSample,
    f32: From<T>,
{
    let callback = move |data: &[T], _: &_| {
        let float_samples: Vec<f32> = data.iter().map(|&s| f32::from(s)).collect();

        let mut proc = processor.lock().unwrap();
        let processed_data = proc.process(&float_samples);

        if !processed_data.is_empty() {
            tx.send(AudioPacket {
                is_mic,
                data: processed_data,
            }).ok();
        }
    };

    let stream = device.build_input_stream(config, callback, err_fn, None)?;
    Ok(stream)
}