# ASR 定向链路调整计划（全关处理 + 丢包统计 + progress 去量化）

## Summary

- 目标：按需求完成三项改造：
  - 全关处理（高通/降噪/AGC/软限幅/ducking 全 `false`）。
  - 统计 `try_send` 丢包次数与 queue 裁剪次数（仅日志输出）。
  - 去掉 `f32 -> i16` 量化（仅作用于 progress 上报，不改 WAV 写盘）。
- 已确认决策：
  - 去量化范围：仅 `listen_recording_progress` 上报数据改为 `Float32` 字节流。
  - 统计呈现：仅日志输出，不改对外 API 字段。
- 成功标准：
  - 不额外设置配置时，5 个处理开关默认为全 `false`（当前已是全关，保留并固化）。
  - 录制期间可看到 `try_send_drop_count`、`queue_trim_count` 的周期日志与 stop 汇总日志。
  - `micBuffer/spkBuffer/buffer` 上报为 `Float32LE` 字节流（4 bytes/sample），WAV 继续 `i16` 落盘。

## Current State Analysis

- `src/global.rs`
  - `AudioProcessingConfig::default()` 已是全 `false`，满足“全关处理”目标。
  - `RecordingProgressInfo` 仍是 `Uint8Array`，可承载任意字节流，暂不需改类型签名。
- `src/capture.rs`
  - `try_send(...).ok()` 当前静默丢包，未统计丢包次数。
- `src/lib.rs`
  - queue 裁剪发生在 `enqueue_packet` 的 `while queue.len() > max_queue_samples`，未统计裁剪次数。
  - progress 上报当前来自 `flush_*_chunk` 的 i16 字节（`float_to_i16` + `to_le_bytes`）。
  - WAV 写盘使用 `hound::SampleFormat::Int` + `writer.write_sample(i16)`。

## Proposed Changes

### 1) 固化“全关处理”默认行为（文档与日志对齐）

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\global.rs`
- What：
  - 保持 `AudioProcessingConfig::default()` 五项均 `false`（不新增行为变更）。
- 文件：`e:\Code\HaierGit\perception-audio-recorder\README.md`
- What：
  - 明确默认配置为全关，避免调用方误解。
- Why：
  - 该项当前已满足，需在计划落地中做“确认+文档对齐”，不做多余改动。

### 2) 增加 `try_send` 丢包计数（仅日志）

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\global.rs`
- What：
  - 新增原子计数器（如 `TRY_SEND_DROP_COUNT`）与重置函数。
- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\capture.rs`
- What：
  - 将 `tx.try_send(...).ok()` 改为 `match`：
    - 成功：无操作；
    - 失败（`Full/Disconnected`）：原子计数 + 可选 debug 日志。
- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - 录制循环按 1s 周期打印计数快照；
  - stop 时打印最终汇总计数。
- Why：
  - 精准验证“回调端丢包”是否导致 ASR 准确率下降。

### 3) 增加 queue 裁剪计数（仅日志）

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\global.rs`
- What：
  - 新增原子计数器（如 `QUEUE_TRIM_COUNT`）与重置函数。
- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - 在 `enqueue_packet` 中每次 `pop_front` 裁剪时累加计数。
  - 与丢包计数一起输出周期日志和 stop 汇总日志。
- Why：
  - 定位“消费跟不上导致历史样本被裁剪”的程度。

### 4) progress 上报改为 Float32 字节流（去 i16 量化）

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - 保留写盘路径（`writer.write_sample(i16)`）不变；
  - 新增 progress 专用缓冲（如 `flush_pcm_f32_chunk/mic/spk`）；
  - 在混音循环中把 `f32` 直接按 `to_le_bytes()` 追加到 progress 缓冲；
  - `target_chunk_bytes` 改为按 `4 bytes/sample` 计算（progress 专用）。
  - `emit_progress_chunk` 改为发送 float32 字节缓冲。
- Why：
  - 按用户要求仅对上报去量化，最大程度保留原始动态细节，减少 ASR 信息损失。
- How：
  - i16 路径仅用于 WAV；
  - progress 路径仅用 f32，不再依赖 `float_to_i16` 结果。
  - 保持 `recordDevice=0/1` 的空侧上报语义不变。

### 5) README 补充编码说明

- 文件：`e:\Code\HaierGit\perception-audio-recorder\README.md`
- What：
  - 明确 `listenRecordingProgress` 的 `buffer/micBuffer/spkBuffer` 为 `Float32LE` 字节流；
  - 说明 chunk 大小从 `2 bytes/sample` 变为 `4 bytes/sample`；
  - 说明 WAV 文件仍为 PCM16，不受该改动影响。
- Why：
  - 避免接入方仍按 i16 解码导致识别失败。

## Assumptions & Decisions

- 决策 1：处理开关默认全关，不增加新开关。
- 决策 2：统计仅日志输出，不新增 `RecordingProgressInfo` 字段与新 API。
- 决策 3：去量化仅限 progress 上报；WAV 写盘继续 i16。
- 决策 4：`Uint8Array` 字段保留，承载 `Float32LE` 原始字节。

## Verification steps

1. 编译与诊断
   - `cargo check` 通过；
   - `global.rs/capture.rs/lib.rs/README.md` 无新增诊断错误。
2. 处理默认值验证
   - `getAudioProcessingConfig()` 返回 5 项均 `false`。
3. 统计日志验证
   - 录制中每秒出现 `try_send_drop_count` 与 `queue_trim_count` 快照；
   - stop 后出现最终汇总日志。
4. progress 编码验证
   - `sampleRate=16000, mono` 时单包目标字节应为 `16000 * 4 * 0.04 = 2560`；
   - 前端按 `Float32Array` 解码后波形连续且幅值在合理区间。
5. 兼容性验证
   - WAV 文件可正常回放（仍是 PCM16）；
   - 若前端仍按 i16 解码 progress，需按文档改为 f32 解码后恢复正确。
