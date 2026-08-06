# start_recording 参数增强与 progress 单侧空数据计划

## Summary

- 目标：增强 `start_recording` 入参能力，支持按设备、采样率、声道控制录制行为，并确保 `listen_recording_progress` 在单设备录制时对未录制侧上报空数据。
- 本次已确认决策：
  - 参数形态：采用位置参数。
  - `channels=mono`：上报真实单声道 PCM（不做双声道复制）。
  - `record_device=0/1` 且目标设备不可用：直接报错，不自动降级。
- 成功标准：
  - `startRecording(saveDir?, recordDevice?, sampleRate?, channels?)` 可用且兼容旧调用。
  - `record_device=0/1/2` 分别实现仅麦克风/仅扬声器/双路录制。
  - `sample_rate` 在 `{8000,16000,32000,48000}` 时强制采用；非法值回落“当前采样率决策方案”。
  - `channels` 支持 `"mono"|"stereo"`，默认 `"mono"`。
  - 单设备录制时，未录制设备对应 `micBuffer` 或 `spkBuffer` 为空 `Uint8Array`。

## Current State Analysis

- 当前 `start_recording` 签名为 `start_recording(save_dir: Option<String>)`，未支持设备/采样率/声道参数。
- 设备发现后默认“尽可能启动双路”，仅在设备不存在时做降级处理，不满足“严格按 record_device 指定”。
- 采样率采用“设备探测动态决策”方案，缺少用户强制采样率能力。
- 声道在多个模块硬编码依赖 `TARGET_CHANNELS=2`：
  - WAV 头、队列帧长、预算计算、上报 chunk、处理器输出布局均按双声道。
- `listen_recording_progress` 的 `micBuffer/spkBuffer` 当前均由循环过程持续累积，不区分“未启用录制设备”场景。
- 当前 `RecordingProgressInfo` 结构可复用，无需新增字段。

## Proposed Changes

### 1) 新增录制配置模型与解析

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\global.rs`
- What：
  - 新增录制设备枚举（内部）：
    - `0 => MicOnly`
    - `1 => SpkOnly`
    - `2 => MicAndSpk`
  - 新增声道模式枚举（内部）：`Mono|Stereo`。
  - 新增 `StartRecordingOptions`（内部运行时结构），包含：
    - `save_dir: Option<String>`
    - `record_device`
    - `sample_rate_override: Option<usize>`
    - `channels_mode`
- Why：
  - 将参数合法性与默认值集中管理，避免 `lib.rs` 分支膨胀。
- How：
  - 合法采样率白名单：`8000/16000/32000/48000`，其他值记为 `None`（回落原策略）。
  - `channels` 默认 `mono`。

### 2) 扩展 `start_recording` 导出签名（位置参数）

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - 将导出签名改为：
    - `start_recording(save_dir: Option<String>, record_device: Option<i32>, sample_rate: Option<i32>, channels: Option<String>)`
  - 内部转换为 `StartRecordingOptions` 并校验参数。
- Why：
  - 满足用户要求的明确参数形态。
- How：
  - 默认值：
    - `record_device` 默认 `2`
    - `sample_rate` 默认 `16000`（合法白名单内，因此优先生效）
    - `channels` 默认 `"mono"`
  - 非法 `record_device/channels` 直接返回参数错误。

### 3) 严格按 `record_device` 选择设备并启动流

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - 根据 `record_device` 决定是否需要 mic/spk：
    - `MicOnly`：必须有 mic，可忽略 spk。
    - `SpkOnly`：必须有 spk，可忽略 mic。
    - `MicAndSpk`：尽量双路，沿用当前可用性判断（两路都不可用时报错）。
  - `MicOnly/SpkOnly` 目标设备不存在时直接报错并触发对应错误事件。
- Why：
  - 对齐“按指定设备严格录制”的约束。
- How：
  - `mic_active/spk_active` 由“目标 + 实际启动结果”共同决定。
  - 不在 `MicOnly/SpkOnly` 模式做自动降级到另一侧。

### 4) 采样率决策接入“强制值优先”

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - `sample_rate` 合法时，`session_sample_rate = sample_rate_override`。
  - 非法时，继续走“当前采样率决策方案”（mic/spk 探测优先逻辑）。
- Why：
  - 满足用户对固定采样率控制需求，同时保留原有鲁棒回退逻辑。
- How：
  - 日志明确输出“forced/fallback”决策来源。

### 5) 声道从固定常量改为会话变量

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - 引入 `session_channels: usize`（`mono=1`, `stereo=2`），替换 `TARGET_CHANNELS` 在录制主循环中的时基/队列/预算计算。
  - WAV 头 `spec.channels = session_channels`。
  - `bytes_per_frame = session_channels * 2`。
  - `flush_pcm_chunk/mic/spk` 按会话声道写入。
- Why：
  - 当前逻辑深度绑定双声道，必须改为会话态才能真实支持 mono。
- How：
  - 对 `mono`：
    - 混音输出只写 1 个样本；
    - mic/spk 上报 chunk 也写 1 声道。
  - 对 `stereo`：保持现有行为。

### 6) 处理器输出声道数可配置

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\processor.rs`
- What：
  - `AudioProcessor` 新增 `output_channels`（1 或 2）。
  - `AudioProcessor::new(...)` 增加入参 `output_channels`。
  - `process()` 的 `Planar -> Interleaved` 映射按 `output_channels` 生成。
- Why：
  - 目前处理器总是输出双声道，无法支撑 `mono` 全链路一致。
- How：
  - `mono`：输出单声道样本（输入双声道时按 `(L+R)/2`）。
  - `stereo`：维持现有 L/R 输出逻辑。

### 7) 采集链路透传会话声道

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\capture.rs`
- What：
  - `start_stream(...)` 新增 `output_channels` 参数并透传给 `AudioProcessor::new(...)`。
- Why：
  - 打通 `lib -> capture -> processor` 的声道配置链路。

### 8) progress 上报“未录制侧空数据”

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - 在 `flush_progress` 上报前，根据 `record_device` 控制：
    - `MicOnly`：`spk_payload` 强制为空。
    - `SpkOnly`：`mic_payload` 强制为空。
    - `MicAndSpk`：维持现有逻辑。
- Why：
  - 满足“未录制设备上报空数据”要求，避免调用方误判。

### 9) 对外类型与文档同步

- 文件：`e:\Code\HaierGit\perception-audio-recorder\index.d.ts`（构建生成）
- What：
  - 更新 `startRecording` 声明为新位置参数签名。
- 文件：`e:\Code\HaierGit\perception-audio-recorder\README.md`
- What：
  - 补充 `recordDevice/sampleRate/channels` 参数说明、默认值、合法值与回退行为；
  - 说明 `mono` 下 progress 为单声道 PCM；
  - 说明 `recordDevice=0/1` 时未录制侧上报空数据。

## Assumptions & Decisions

- 决策 1：`startRecording` 采用位置参数扩展，不新增并行 API。
- 决策 2：`sample_rate` 默认值 16000，且属于合法白名单，默认将覆盖动态采样率策略。
- 决策 3：`channels` 默认 mono，progress 与落盘均返回/写入真实单声道。
- 决策 4：`record_device=0/1` 严格模式，目标设备不可用直接报错。
- 决策 5：保留现有 `RecordingProgressInfo` 结构，不新增字段。

## Verification steps

1. 构建与导出验证
   - `npm run build` 通过；
   - `index.d.ts` 中 `startRecording` 签名更新。
2. 设备选择验证
   - `recordDevice=0`：仅 mic 有数据，`spkBuffer` 空；
   - `recordDevice=1`：仅 spk 有数据，`micBuffer` 空；
   - `recordDevice=2`：双路行为与当前一致。
3. 采样率验证
   - 传 `8000/16000/32000/48000`，日志显示强制采样率生效；
   - 传非法值，日志显示回退动态策略。
4. 声道验证
   - `channels=mono`：WAV 头为 1 声道，progress chunk 按单声道字节增长；
   - `channels=stereo`：保持双声道行为。
5. 兼容回归
   - 旧调用 `startRecording(saveDir)` 仍可运行（其余参数走默认值）。
