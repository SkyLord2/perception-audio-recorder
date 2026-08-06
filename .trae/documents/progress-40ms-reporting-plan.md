# `listen_recording_progress` 改为 40ms 固定分片上报计划

## Summary

- 目标：将 `listen_recording_progress` 的上报机制从当前“按时间间隔 flush”改为“按 40ms 固定分片上报”。
- 核心规则：
  - 上报周期目标：每 40ms 一次。
  - 单次上报目标字节数：`sample_rate × 16bit × channels ÷ 8 × 0.04s`。
  - 示例：`16000 × 16 × 1 ÷ 8 × 0.04 = 1280 bytes`。
- 适配范围：
  - `buffer/micBuffer/spkBuffer` 三路数据统一按 40ms 分片粒度上报。
  - 保持当前 `recordDevice=0/1` 未录制侧空数据语义不变。

## Current State Analysis

- 当前上报触发条件在 `src/lib.rs` 中依赖：
  - `last_flush.elapsed() >= Duration::from_secs(FLUSH_INTERVAL_SECS)`；
  - `FLUSH_INTERVAL_SECS` 当前为 1 秒（在 `src/global.rs`）。
- 数据写入流程为逐帧追加到 `flush_*_chunk`，到 flush 条件后一次性 `emit_recording_progress(...)`。
- 主循环 `rx.recv_timeout(Duration::from_millis(100))`，该阻塞上限会直接影响 40ms 粒度的及时触发。
- 因此仅改 `FLUSH_INTERVAL_SECS` 不能严格满足“40ms + 固定字节公式”的目标，需要改为“按分片字节阈值驱动”的 flush 策略。

## Proposed Changes

### 1) 新增 40ms 分片参数与工具计算

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - 新增常量：`PROGRESS_CHUNK_MS: u64 = 40`。
  - 基于会话参数计算：
    - `bytes_per_sample = 2`（16bit）
    - `bytes_per_frame = session_channels * bytes_per_sample`
    - `chunk_frames = session_sample_rate * 40 / 1000`
    - `target_chunk_bytes = chunk_frames * bytes_per_frame`
- Why：
  - 将“公式”落实为可执行阈值，避免浮动。
- How：
  - 用整数运算，避免浮点误差导致分片抖动。

### 2) 将 flush 触发改为“按字节阈值 while 消费”

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - 替换现有 `if last_flush.elapsed() >= ...` 条件。
  - 新逻辑：
    - 当 `flush_pcm_chunk.len() >= target_chunk_bytes` 时，循环切片并上报；
    - 每次上报严格取 `target_chunk_bytes` 的 mixed 数据；
    - mic/spk 对应取同等分片长度（单设备模式下未录制侧保持空）。
- Why：
  - 确保每次上报数据量符合公式，不依赖 wall clock 偶然性。
- How：
  - 建议引入 `drain_chunk_exact(chunk, n)` 小函数，避免重复切片代码。
  - `flush_progress` 改造成可接收“本次分片 payload”的函数，或拆分为：
    - `emit_progress_chunk(...)`
    - `flush_tail_on_stop(...)`（处理收尾不足 40ms 的尾包）。

### 3) 调整接收轮询上限，避免 40ms 节拍被 100ms 阻塞

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - 将 `rx.recv_timeout(Duration::from_millis(100))` 调整为更小粒度（建议 10ms 或 20ms）。
- Why：
  - 当前 100ms 超时会导致积压后批量上报，破坏 40ms 实时性。
- How：
  - 保持现有无包超时逻辑（5s）不变，仅降低单次等待步长。

### 4) 统计口径与尾包策略

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - `totalDuration/totalSize` 继续按累计写入字节计算。
  - 停止时对剩余不足 40ms 的尾数据执行一次最终上报（保持当前“stop 前尾包可见”行为）。
- Why：
  - 保证时长统计连续、停止语义不倒退。
- How：
  - 维持 `RECORDED_DATA_BYTES` 原子累加逻辑，只改变上报触发点。

### 5) 文档同步

- 文件：`e:\Code\HaierGit\perception-audio-recorder\README.md`
- What：
  - 在 `listenRecordingProgress` 说明中补充“40ms 固定分片”与字节公式。
  - 标注 chunk size 与 `sampleRate/channels` 相关，不是固定常数。
- Why：
  - 让调用方正确预期 `byteLength`。

## Assumptions & Decisions

- 决策 1：40ms 规则作用于 `buffer/micBuffer/spkBuffer` 三路。
- 决策 2：每次上报以“精确字节分片”为准；若某轮积压超过一个分片，可在同一轮发多次。
- 决策 3：停止时允许发送不足 40ms 的最终尾包，避免数据丢失。
- 决策 4：保持当前 `recordDevice=0/1` 的空侧上报语义不变。

## Verification steps

1. 构建与静态验证
   - `cargo check` 通过，改动文件诊断无错误。
2. 分片字节验证
   - `sampleRate=16000, channels=mono`：`progress.buffer.byteLength` 常态为 `1280`。
   - `sampleRate=16000, channels=stereo`：常态为 `2560`。
   - `sampleRate=48000, channels=mono`：常态为 `3840`。
3. 频次验证
   - 日志时间戳观测上报间隔接近 40ms（允许调度抖动）。
   - 积压场景下可出现同轮多次发包，但单包字节仍满足公式。
4. 单设备语义验证
   - `recordDevice=0` 时 `spkBuffer.byteLength == 0`；
   - `recordDevice=1` 时 `micBuffer.byteLength == 0`。
5. 停止尾包验证
   - stop 后最后一次上报允许小于目标分片，且总时长/总大小连续增长无跳变。
