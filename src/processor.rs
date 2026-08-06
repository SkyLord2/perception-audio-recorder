use std::collections::VecDeque;
use std::sync::Arc;

use ringbuf::{Consumer, HeapRb, Producer};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use sonora::config::{EchoCanceller, GainController2, NoiseSuppression};
use sonora::{AudioProcessing, Config, StreamConfig};

use crate::global::{
    AppResult, ENABLE_MIC_AUTO_GAIN_CONTROL, ENABLE_MIC_NOISE_SUPPRESSION,
    RESAMPLER_CHUNK_SIZE,
};

pub struct AudioProcessor {
    resampler: Option<SincFixedIn<f32>>,
    producer: Producer<f32, Arc<HeapRb<f32>>>,
    consumer: Consumer<f32, Arc<HeapRb<f32>>>,
    use_resampler: bool,
    input_channels: usize,
    target_sample_rate: usize,
    output_channels: usize,
    is_mic: bool,
    aec_enabled: bool,
    scratch_input: Vec<Vec<f32>>,
    scratch_output: Vec<Vec<f32>>,
    apm: Option<AudioProcessing>,
    apm_frame_samples: usize,
    capture_frame_queue: VecDeque<f32>,
    render_frame_queue: VecDeque<f32>,
    apm_capture_in: Vec<Vec<f32>>,
    apm_capture_out: Vec<Vec<f32>>,
    apm_render_in: Vec<Vec<f32>>,
    apm_render_out: Vec<Vec<f32>>,
}

impl AudioProcessor {
    pub fn new(
        source_sample_rate: usize,
        target_sample_rate: usize,
        input_channels: usize,
        output_channels: usize,
        is_mic: bool,
        enable_aec: bool,
    ) -> AppResult<Self> {
        let params = SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: WindowFunction::BlackmanHarris2,
        };

        let ratio = target_sample_rate as f64 / source_sample_rate as f64;
        let use_resampler = source_sample_rate != target_sample_rate;
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

        let buffer_len = RESAMPLER_CHUNK_SIZE * input_channels * 4;
        let rb = HeapRb::<f32>::new(buffer_len);
        let (producer, consumer) = rb.split();

        let scratch_input = vec![vec![0.0; RESAMPLER_CHUNK_SIZE]; input_channels];
        let output_frames_max = (RESAMPLER_CHUNK_SIZE as f64 * ratio).ceil() as usize + 10;
        let scratch_output = vec![vec![0.0; output_frames_max]; input_channels];

        Ok(Self {
            resampler,
            producer,
            consumer,
            use_resampler,
            input_channels,
            target_sample_rate,
            output_channels,
            is_mic,
            aec_enabled: false,
            scratch_input,
            scratch_output,
            apm: None,
            apm_frame_samples: 0,
            capture_frame_queue: VecDeque::new(),
            render_frame_queue: VecDeque::new(),
            apm_capture_in: Vec::new(),
            apm_capture_out: Vec::new(),
            apm_render_in: Vec::new(),
            apm_render_out: Vec::new(),
        })
        .map(|mut processor| {
            processor.rebuild_apm(enable_aec);
            processor
        })
    }

    pub fn aec_enabled(&self) -> bool {
        self.aec_enabled
    }

    pub fn set_aec_enabled(&mut self, enable_aec: bool) {
        self.rebuild_apm(enable_aec);
    }

    fn rebuild_apm(&mut self, enable_aec: bool) {
        self.aec_enabled = self.is_mic && enable_aec;
        self.apm = None;
        self.apm_frame_samples = 0;
        self.capture_frame_queue.clear();
        self.render_frame_queue.clear();
        self.apm_capture_in.clear();
        self.apm_capture_out.clear();
        self.apm_render_in.clear();
        self.apm_render_out.clear();

        if !self.is_mic
            || (!ENABLE_MIC_NOISE_SUPPRESSION
                && !ENABLE_MIC_AUTO_GAIN_CONTROL
                && !self.aec_enabled)
        {
            return;
        }

        let stream_config =
            StreamConfig::new(self.target_sample_rate as u32, self.output_channels as u16);
        self.apm_frame_samples = stream_config.num_frames();
        let config = Config {
            echo_canceller: if self.aec_enabled {
                Some(EchoCanceller::default())
            } else {
                None
            },
            noise_suppression: if ENABLE_MIC_NOISE_SUPPRESSION {
                Some(NoiseSuppression::default())
            } else {
                None
            },
            gain_controller2: if ENABLE_MIC_AUTO_GAIN_CONTROL {
                Some(GainController2::default())
            } else {
                None
            },
            ..Default::default()
        };
        self.apm_capture_in = vec![vec![0.0; self.apm_frame_samples]; self.output_channels];
        self.apm_capture_out = vec![vec![0.0; self.apm_frame_samples]; self.output_channels];
        self.apm_render_in = vec![vec![0.0; self.apm_frame_samples]; self.output_channels];
        self.apm_render_out = vec![vec![0.0; self.apm_frame_samples]; self.output_channels];
        self.apm = Some(
            AudioProcessing::builder()
                .config(config)
                .capture_config(stream_config)
                .render_config(stream_config)
                .build(),
        );
    }

    pub fn process_capture(&mut self, input: &[f32]) -> AppResult<Vec<f32>> {
        let mapped = self.resample_and_map(input)?;
        if !self.is_mic || self.apm.is_none() {
            return Ok(mapped);
        }

        let frame_samples = self.apm_frame_samples * self.output_channels;
        // APM 只能按固定 10ms 帧工作，这里先把重采样后的交错数据缓存起来，
        // 等攒够一整帧后再送入 3A，避免出现半帧处理导致的状态紊乱。
        self.capture_frame_queue.extend(mapped.iter().copied());

        let mut output = Vec::with_capacity(mapped.len());
        while self.capture_frame_queue.len() >= frame_samples {
            Self::pop_interleaved_frame_into_planar(
                &mut self.capture_frame_queue,
                &mut self.apm_capture_in,
                self.output_channels,
                self.apm_frame_samples,
            );

            let input_refs = Self::planar_refs(&self.apm_capture_in);
            let mut output_refs = Self::planar_mut_refs(&mut self.apm_capture_out);
            if let Some(apm) = self.apm.as_mut() {
                apm.process_capture_f32(&input_refs, &mut output_refs)?;
            }

            let base = output.len();
            output.resize(base + frame_samples, 0.0);
            Self::interleave_from(&self.apm_capture_out, &mut output[base..base + frame_samples]);
        }

        Ok(output)
    }

    pub fn process_render(&mut self, input: &[f32]) -> AppResult<Vec<f32>> {
        self.resample_and_map(input)
    }

    pub fn feed_render_frame(&mut self, render_samples: &[f32]) -> AppResult<()> {
        if !self.is_mic || self.apm.is_none() || !self.aec_enabled {
            return Ok(());
        }

        let frame_samples = self.apm_frame_samples * self.output_channels;
        // render 链路只承担 AEC 参考流输入职责，因此同样按固定帧送入 APM，
        // 但不把处理结果回传到业务侧。
        self.render_frame_queue.extend(render_samples.iter().copied());

        while self.render_frame_queue.len() >= frame_samples {
            Self::pop_interleaved_frame_into_planar(
                &mut self.render_frame_queue,
                &mut self.apm_render_in,
                self.output_channels,
                self.apm_frame_samples,
            );
            let input_refs = Self::planar_refs(&self.apm_render_in);
            let mut output_refs = Self::planar_mut_refs(&mut self.apm_render_out);
            if let Some(apm) = self.apm.as_mut() {
                apm.process_render_f32(&input_refs, &mut output_refs)?;
            }
        }

        Ok(())
    }

    fn resample_and_map(&mut self, input: &[f32]) -> AppResult<Vec<f32>> {
        for &sample in input {
            let _ = self.producer.push(sample);
        }

        let mut final_output = Vec::new();
        let needed_samples = RESAMPLER_CHUNK_SIZE * self.input_channels;

        while self.consumer.len() >= needed_samples {
            for i in 0..RESAMPLER_CHUNK_SIZE {
                for ch in 0..self.input_channels {
                    let sample = self.consumer.pop().unwrap_or(0.0);
                    self.scratch_input[ch][i] = sample;
                }
            }

            let out_len = if self.use_resampler {
                let (_, out_len) = self
                    .resampler
                    .as_mut()
                    .expect("resampler missing when use_resampler=true")
                    .process_into_buffer(&self.scratch_input, &mut self.scratch_output, None)?;
                out_len
            } else {
                for ch in 0..self.input_channels {
                    self.scratch_output[ch][..RESAMPLER_CHUNK_SIZE]
                        .copy_from_slice(&self.scratch_input[ch]);
                }
                RESAMPLER_CHUNK_SIZE
            };

            let start_idx = final_output.len();
            final_output.resize(start_idx + out_len * self.output_channels, 0.0);

            for i in 0..out_len {
                let left_sample;
                let right_sample;

                if self.input_channels == 1 {
                    let sample = self.scratch_output[0][i];
                    left_sample = sample;
                    right_sample = sample;
                } else {
                    left_sample = self.scratch_output[0][i];
                    right_sample = self.scratch_output[1][i];
                }

                if self.output_channels == 1 {
                    final_output[start_idx + i] = (left_sample + right_sample) * 0.5;
                } else {
                    final_output[start_idx + i * 2] = left_sample;
                    final_output[start_idx + i * 2 + 1] = right_sample;
                }
            }
        }

        Ok(final_output)
    }

    fn pop_interleaved_frame_into_planar(
        queue: &mut VecDeque<f32>,
        output: &mut [Vec<f32>],
        output_channels: usize,
        frame_samples: usize,
    ) {
        // 这里直接把交错队列中的一整帧样本写入可复用的 planar 缓冲区，
        // 避免在实时音频热路径上反复创建临时 Vec。
        for frame_idx in 0..frame_samples {
            for ch in 0..output_channels {
                output[ch][frame_idx] = queue.pop_front().unwrap_or(0.0);
            }
        }
    }

    fn planar_refs<'a>(planar: &'a [Vec<f32>]) -> Vec<&'a [f32]> {
        planar.iter().map(|channel| channel.as_slice()).collect()
    }

    fn planar_mut_refs<'a>(planar: &'a mut [Vec<f32>]) -> Vec<&'a mut [f32]> {
        planar
            .iter_mut()
            .map(|channel| channel.as_mut_slice())
            .collect()
    }

    fn interleave_from(planar: &[Vec<f32>], output: &mut [f32]) {
        let channels = planar.len();
        let frames = planar.first().map(|channel| channel.len()).unwrap_or(0);
        for frame_idx in 0..frames {
            for ch in 0..channels {
                output[frame_idx * channels + ch] = planar[ch][frame_idx];
            }
        }
    }
}
