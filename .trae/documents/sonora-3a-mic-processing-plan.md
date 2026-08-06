## Summary

- 目标：在现有 Rust 录制链路中接入 `sonora` 3A（降噪/回声消除/自动增益），对麦克风采集数据做实时处理，并让写盘音频与 `listen_recording_progress.micBuffer` 都输出处理后的麦克风数据。
- 已确认的关键决策：
  - `micBuffer` 改为上报处理后的麦克风数据。
  - 当 `AEC` 开启但没有可用扬声器参考流时，自动降级为仅 `NS/AGC` 生效，并输出日志。
- 方案重点：
  - 在 `global.rs` 新增 3 个全局开关常量。
  - 在 `processor.rs` 内引入 `sonora::AudioProcessing`，只对麦克风流启用 3A。
  - 在 `capture.rs` 中为麦克风与扬声器拆分处理路径，并把扬声器帧喂给麦克风处理器作为 `AEC render` 参考。
  - 在 `lib.rs` 中将写盘与 `micBuffer` 的来源切换为处理后的麦克风数据。

## Current State Analysis

### 依赖与示例

- `Cargo.toml` 已包含 `sonora = "0.1.0"`，无需新增依赖声明。
- `examples/sonora/recording.rs` 展示了 `AudioProcessing::builder().config(...).capture_config(...).render_config(...).build()` 的标准用法，并通过：
  - `process_capture_f32()` 处理麦克风帧；
  - `process_render_f32()` 提供扬声器参考帧。
- `examples/sonora/simple.rs` 和 `examples/sonora/karaoke.rs` 进一步说明：`AEC` 要想发挥作用，必须持续向 APM 喂 `render` 数据。

### 当前录制链路

- `src/capture.rs`
  - 当前每个设备各自持有一个 `AudioProcessor`。
  - 处理器仅负责“重采样 + 通道映射”，之后通过 `AudioPacket { is_mic, data }` 发往录制线程。
  - 麦克风和扬声器流彼此独立，没有共享的 3A 状态，也没有 `render -> capture` 的参考喂入链路。
- `src/processor.rs`
  - `AudioProcessor` 当前只有 Rubato 重采样和声道映射逻辑。
  - 没有分帧缓存、没有 `sonora` 配置、没有区分“capture 输入”和“render 输入”的处理入口。
- `src/lib.rs`
  - 当前 `mic_raw_progress_queue` 保存的是“重采样后的原始麦克风数据”。
  - 写盘的主流程仍使用 `mic_queue/spk_queue` 做混合与 PLC 补偿。
  - `listen_recording_progress.micBuffer` 当前来自 `mic_raw_progress_queue`，不经过 3A。
- `src/global.rs`
  - 当前没有任何 3A 开关；只保留了录制时序、统计和进度结构定义。

### 与本次目标的冲突点

- 当前仓库最近将 `micBuffer` 语义收敛为“原始、重采样后、不经 PLC/补零”，而本次新决策要求它改为“处理后麦克风数据”。
- `AEC` 不能只靠麦克风链路单独完成，必须把扬声器数据在相同会话采样率/声道配置下送入同一个 APM 实例。
- 当前 `AudioPacket` 只有一个 `data` 字段；若要同时保留“处理后用于写盘/上报”和“原始用于调试/回退”的能力，需要在计划中明确最终只保留处理后输出，不引入额外原始上报字段。

## Proposed Changes

### 1. `src/global.rs`

- 新增 3 个全局常量开关，默认值均为 `true`：
  - `ENABLE_MIC_NOISE_SUPPRESSION`
  - `ENABLE_MIC_ECHO_CANCELLATION`
  - `ENABLE_MIC_AUTO_GAIN_CONTROL`
- 保持为编译期全局常量，不新增运行时 set/get API。
- 在注释中明确：
  - 仅影响麦克风链路；
  - `AEC` 依赖扬声器参考流，缺失时自动降级；
  - 处理后数据将用于写盘和 `micBuffer` 上报。

### 2. `src/processor.rs`

- 将当前“仅重采样器”扩展为“重采样 + 可选 3A”的统一处理器。
- 重构 `AudioProcessor` 内部状态：
  - 保留现有 Rubato 输入缓存与声道映射逻辑。
  - 新增 `is_mic` 标记，区分麦克风处理器和扬声器处理器。
  - 新增可选 `sonora::AudioProcessing` 实例，仅麦克风处理器创建。
  - 新增 `render_frame_queue` 或等价缓冲，用于缓存来自扬声器的 10ms `render` 帧。
  - 新增 `capture_frame_queue` / `apm_output_queue`，将重采样后的麦克风数据按 `sonora` 所需 frame size 组织后再输出。
- 以 `sonora::StreamConfig::new(target_sample_rate as u32, output_channels as u16)` 为 APM 配置基础，并通过 `stream_config.num_frames()` 获取固定帧长。
- 增加两个明确入口，替代当前单一 `process()`：
  - `process_capture(&mut self, input: &[f32]) -> Vec<f32>`
    - 仅给麦克风流使用。
    - 流程：输入重采样/声道映射 -> 缓存成 APM 帧 -> 若 `AEC` 可用则先消费 render 缓冲 -> `process_capture_f32()` -> 输出处理后 `Vec<f32>`。
  - `process_render(&mut self, input: &[f32]) -> Vec<f32>`
    - 给扬声器流使用。
    - 流程：输入重采样/声道映射 -> 分帧调用 `process_render_f32()` 仅更新 AEC 参考状态 -> 原样返回重采样后的扬声器数据。
- `AEC` 降级策略：
  - 当全局 AEC 开关为 `false` 时，不创建 `EchoCanceller`。
  - 当开关为 `true` 但当前会话没有可用扬声器流，或 render 帧暂时不足时：
    - `AudioProcessing` 配置中仍可带 `EchoCanceller`；
    - 但逻辑上仅在拿到 render 帧时调用 `process_render_f32()`；
    - 录制启动时打印一次“缺少扬声器参考流，AEC 自动降级”的信息日志，避免每帧刷屏。
- `NS/AGC` 配置：
  - 分别由 `NoiseSuppression::default()`、`GainController2::default()` 控制；
  - 关闭时传 `None`。
- 声道处理原则：
  - 当前会话 `mono/stereo` 都按 `output_channels` 构建 APM；
  - 若输入设备声道数大于会话输出声道数，仍先沿用现有 downmix/mapping，再交给 APM。

### 3. `src/capture.rs`

- `start_stream()` 需要支持“扬声器帧喂入麦克风处理器”的协作关系。
- 设计改造：
  - 为麦克风流单独创建一个可共享的 `Arc<Mutex<AudioProcessor>>`。
  - 为扬声器流创建自己的 `AudioProcessor`，但同时持有麦克风处理器的弱引用/共享引用，用于把处理后的扬声器帧送入 `process_render()` 或专门的 `feed_render()` 接口。
- 推荐落地方式：
  - 将 `start_stream()` 签名改为接收一个 `StreamProcessors` 或两个共享处理器引用，避免在两个流之间重复创建彼此不知情的实例。
  - 麦克风回调：
    - 原始设备数据 -> `mic_processor.process_capture()` -> 发送处理后的麦克风 `AudioPacket`。
  - 扬声器回调：
    - 原始设备数据 -> `spk_processor` 做重采样/声道映射，得到会话格式的扬声器帧；
    - 将该帧喂给 `mic_processor.process_render()` 或 `mic_processor.feed_render()`；
    - 再把扬声器帧作为 `AudioPacket` 发往录制线程，保持 `spkBuffer` 语义不变。
- 保留现有 `try_send` 丢包统计。
- 错误处理：
  - 若 `sonora` 处理失败，不在回调线程 panic；
  - 记录错误日志，并跳过该帧或回退为未处理重采样帧，避免中断录制。

### 4. `src/lib.rs`

- 调整录制线程中的麦克风队列语义：
  - `mic_queue` 改为保存“处理后的麦克风数据”。
  - 当前 `mic_raw_progress_queue` 改名为更符合新语义的 `mic_progress_queue`，保存“处理后的麦克风上报数据”。
- 更新 `enqueue_packet()`：
  - 麦克风 `packet.data` 同时进入 `mic_queue` 与 `mic_progress_queue`；
  - 不再保留“原始 micBuffer”语义。
- 更新 `drain_mic_raw_payload()` 闭包命名与注释，改为从处理后队列取 `Float32LE` 数据。
- 写盘逻辑：
  - 因为混音/左麦右扬已经消费 `mic_queue`，所以落盘中的麦克风侧天然变为“处理后麦克风”。
  - 需要同步更新注释，避免仍写“原始数据”。
- 进度上报逻辑：
  - `listen_recording_progress.micBuffer` 改为处理后麦克风 `Float32LE`。
  - `buffer` 仍保持当前主混音语义，即“处理后 mic + 原样 spk 的合并上报”。
- AEC 降级日志位置：
  - 在 `start_record_impl()` 判定设备可用性后，根据 `record_device_mode` 和实际流启动结果记录一次降级说明。
  - 例如：`record_device=0` 或扬声器流启动失败时，若 AEC 开关开启则提示“本次会话无 render 参考，AEC 自动降级为关闭”。

### 5. `README.md`

- 更新 `RecordingProgressInfo` 文档：
  - `micBuffer` 描述从“重采样后的原始麦克风数据”改为“经 3A 处理后的麦克风数据（Float32LE）”。
- 新增 3A 开关说明：
  - 在 `global.rs` 中通过编译期常量控制。
  - `AEC` 依赖扬声器参考流；单麦场景下自动降级。
- 更新行为说明：
  - 写盘和 `micBuffer` 现在都使用处理后的麦克风数据。
  - `spkBuffer` 保持会话采样率/声道映射后的扬声器数据。
- 若 README 中仍保留“`micBuffer` 为原始数据、不经处理”的表述，需要全部改写，避免对接方继续按旧语义集成。

## Assumptions & Decisions

- 决策：本次不恢复历史的运行时音频处理配置接口，只使用 `global.rs` 中的编译期开关。
- 决策：`micBuffer` 与写盘都切换为处理后数据，不再同时保留原始版本。
- 决策：`AEC` 缺少扬声器参考时自动降级，并记日志；不阻止录制启动。
- 决策：`sonora` 仅应用于麦克风链路；扬声器链路只承担 render 参考和原样上报。
- 假设：`sonora 0.1.0` 的 `AudioProcessing` 在当前 `edition = 2024`、Windows、`cpal 0.17.1` 环境下可正常编译链接。
- 假设：现有 40ms progress 上报不变，但 `sonora` 内部按 10ms 帧工作，因此需要在处理器内部做 10ms 分帧缓存。
- 假设：当 `record_device=1`（仅扬声器）时，`micBuffer` 仍为空；当 `record_device=0`（仅麦克风）且 AEC 开启时，会自动降级为仅 `NS/AGC`。

## Verification Steps

### 1. 构建与诊断

- `cargo check` 通过。
- 对 `src/global.rs`、`src/processor.rs`、`src/capture.rs`、`src/lib.rs`、`README.md` 执行诊断，无新增错误。

### 2. 3A 开关验证

- 分别切换 `ENABLE_MIC_NOISE_SUPPRESSION`、`ENABLE_MIC_ECHO_CANCELLATION`、`ENABLE_MIC_AUTO_GAIN_CONTROL`：
  - 关闭 `NS`：静音底噪回升可观察。
  - 关闭 `AGC`：远近说话音量波动更明显。
  - 关闭 `AEC`：扬声器外放时麦克风回声更明显。

### 3. AEC 自动降级验证

- `record_device=0` 启动录制时：
  - 录制成功启动；
  - 日志出现 AEC 自动降级提示；
  - `NS/AGC` 仍可生效。
- `record_device=2` 但扬声器流启动失败时：
  - 录制仍可继续；
  - 日志出现降级提示；
  - 不因缺少 render 参考而 panic。

### 4. 数据语义验证

- `listen_recording_progress.micBuffer` 用 `Float32Array` 解码后可正常回放。
- 对比开启/关闭 3A 的 `micBuffer` 波形或听感，应有明显差异。
- `spkBuffer` 仍可正常按 `Float32LE` 解码，语义不变。

### 5. 写盘验证

- 录制输出 WAV 文件可正常播放。
- 开启 3A 时，写盘结果与 `micBuffer` 的处理后听感一致，不再是原始麦克风底噪/回声版本。

### 6. 回归验证

- `pauseRecording` / `resumeRecording` / `stopRecording` 流程无回归。
- `try_send_drop_count`、`queue_trim_count` 日志仍正常输出。
- `record_device=1` 时 `micBuffer.byteLength == 0`；`record_device=0/2` 时 `micBuffer` 为处理后麦克风数据。
