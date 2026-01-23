// src/processor.rs

// 修正引入路径：使用 HeapRb (堆分配环形缓冲)
use ringbuf::{HeapRb, Producer, Consumer}; 
use rubato::{Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};
use std::sync::Arc;
use crate::global::{TARGET_CHANNELS, TARGET_SAMPLE_RATE, RESAMPLER_CHUNK_SIZE, AppResult};

pub struct AudioProcessor {
    resampler: SincFixedIn<f32>,
    
    // 修正：在 ringbuf 0.3 中，split() 返回的 Producer/Consumer 需要知道底层的容器类型。
    // HeapRb<f32> 默认会被 wrap 进 Arc，所以这里泛型参数是 Arc<HeapRb<f32>>
    producer: Producer<f32, Arc<HeapRb<f32>>>,
    consumer: Consumer<f32, Arc<HeapRb<f32>>>,
    
    input_channels: usize,
    scratch_input: Vec<Vec<f32>>, 
    scratch_output: Vec<Vec<f32>>,
}

impl AudioProcessor {
    pub fn new(source_sample_rate: usize, input_channels: usize) -> AppResult<Self> {
        // --- Rubato 配置 (保持不变) ---
        let params = SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: WindowFunction::BlackmanHarris2,
        };

        let ratio = TARGET_SAMPLE_RATE as f64 / source_sample_rate as f64;
        
        let resampler = SincFixedIn::<f32>::new(
            ratio,
            2.0,
            params,
            RESAMPLER_CHUNK_SIZE, 
            input_channels,
        )?;

        // --- Ringbuf 0.3.3 修正初始化 ---
        let buffer_len = RESAMPLER_CHUNK_SIZE * input_channels * 4; 
        
        // 1. 创建堆分配的 RingBuffer
        let rb = HeapRb::<f32>::new(buffer_len);
        
        // 2. split() 会自动将 rb 包装在 Arc 中，并返回两个拥有者
        let (producer, consumer) = rb.split();

        let scratch_input = vec![vec![0.0; RESAMPLER_CHUNK_SIZE]; input_channels];
        let output_frames_max = (RESAMPLER_CHUNK_SIZE as f64 * ratio).ceil() as usize + 10;
        let scratch_output = vec![vec![0.0; output_frames_max]; input_channels];

        Ok(Self {
            resampler,
            producer,
            consumer,
            input_channels,
            scratch_input,
            scratch_output,
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

            // B. 重采样 (保持不变)
            let (_, out_len) = self.resampler.process_into_buffer(
                &self.scratch_input,
                &mut self.scratch_output,
                None
            ).expect("Resampling internal error");

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

                final_output[start_idx + i * 2] = left_sample;
                final_output[start_idx + i * 2 + 1] = right_sample;
            }
        }

        final_output
    }
}