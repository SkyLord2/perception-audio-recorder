# `listen_recording_progress.mic_buffer` 改为“重采样后原始麦克风数据（无 PLC/补零）”实施计划

## Summary

- 目标：将 `listen_recording_progress` 上报的 `mic_buffer` 调整为“重采样后的原始麦克风数据”，并明确**不经过 PLC、缺帧补偿、补零**等处理。
- 范围：仅调整 `mic_buffer` 的生成路径与文档语义；不改变 WAV 写盘与 `buffer/spk_buffer` 现有策略。
- 成功标准：
  - `mic_buffer` 仅来源于麦克风采集回调经 `AudioProcessor`（含重采样/声道映射）后的真实帧。
  - `mic_buffer` 不再受主循环中 `mic_missing_frames` 等 PLC 逻辑影响。
  - 40ms 上报节拍保留，但 `mic_buffer` 在数据不足时允许小于目标分片或为空（不补零）。

## Current State Analysis

- `src/capture.rs`
  - 麦克风与扬声器都通过 `AudioProcessor::process()` 产出 `processed_data` 后入队，已具备“重采样后数据”基础。
- `src/lib.rs`
  - 当前 `mic_buffer` 来自主循环 `for` 循环内的 `mic_l/mic_r`，而该变量会走缺帧保持与衰减（PLC）路径。
  - `flush_mic_pcm_chunk` 与 `flush_pcm_chunk/spk` 一样按统一帧推进，导致 `mic_buffer` 隐式包含补偿样本。
- `src/global.rs`
  - `RecordingProgressInfo.mic_buffer` 为 `Uint8Array`，可继续承载 Float32LE 字节，无需改结构。

## Proposed Changes

### 1) 在 `lib.rs` 中拆分“mic 原始上报缓冲”与“主混音处理缓冲”

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - 新增麦克风上报专用队列（建议：`mic_raw_progress_queue: VecDeque<f32>`）。
  - 在接收 `packet.is_mic` 时，把 `packet.data` 同步写入：
    - 现有 `mic_queue`（供主流程混音/写盘）
    - 新增 `mic_raw_progress_queue`（仅供 `mic_buffer` 上报）
  - `mic_buffer` 组包改为优先从 `mic_raw_progress_queue` 按 40ms 目标字节抽取。
- Why：
  - 彻底切断 `mic_buffer` 与 PLC/补偿链路的耦合，保证其“重采样后原始数据”属性。
- How：
  - `mic_raw_progress_queue` 不参与 `mic_missing_frames`、`last_mic_*` 逻辑。
  - 若该队列数据不足，不补零，允许当轮 `mic_buffer` 小包或空包。

### 2) 保持 `buffer/spk_buffer` 现有策略不变

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - `buffer` 与 `spk_buffer` 继续沿用当前主循环策略（含现有队列/节拍机制）。
  - 仅替换 `mic_payload` 的来源，不改 `emit_progress_chunk` API。
- Why：
  - 最小侵入实现需求，避免扩大回归面。

### 3) 明确单设备语义与无补零行为

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - 保持 `record_device=1` 时 `mic_buffer` 为空的既有语义。
  - 在 `record_device=0/2` 时，`mic_buffer` 仅由 `mic_raw_progress_queue` 出包；数据不足时不补零。
- Why：
  - 与“不要 PLC/补零”等要求一致，同时兼容既有设备模式语义。

### 4) README 同步语义

- 文件：`e:\Code\HaierGit\perception-audio-recorder\README.md`
- What：
  - 新增/更新说明：`micBuffer` 为“重采样后原始麦克风 Float32LE 数据，不含 PLC/补零”。
  - 说明 40ms 节拍下 `micBuffer` 可出现变长分片（不足时小包/空包）。
- Why：
  - 避免调用方误以为 `micBuffer` 与 `buffer` 一样是补偿后连续流。

## Assumptions & Decisions

- 决策 1：`mic_buffer` 只保证“重采样后原始性”，不保证每包严格等长。
- 决策 2：不新增 NAPI 字段；沿用 `RecordingProgressInfo` 当前结构。
- 决策 3：本次不改 PLC 主逻辑（其仍可作用于 `buffer`/写盘链路），仅对 `mic_buffer` 去耦。

## Verification steps

1. 构建与诊断
   - `cargo check` 通过；
   - `src/lib.rs`、`README.md` 无新增诊断错误。
2. 语义验证（关键）
   - 仅麦克风录制时，断续说话/静音场景下 `mic_buffer` 不出现补零持续填充（允许空包或小包）。
   - `mic_buffer` 与主 `buffer` 在静音缺包阶段表现差异可观察（`buffer` 可能连续，`mic_buffer` 可中断）。
3. 设备模式验证
   - `record_device=1` 时 `micBuffer.byteLength == 0`。
   - `record_device=0/2` 时 `micBuffer` 来源为麦克风真实数据，且可正常被 Float32 解码。
