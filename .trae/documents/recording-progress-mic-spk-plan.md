# listen_recording_progress 增强计划（上报麦克风/扬声器音频）

## Summary

- 目标：增强 `listen_recording_progress` 回调上报内容，在现有合并音频 `buffer` 的基础上，同时上报：
  - `mic_buffer`：麦克风音频数据
  - `spk_buffer`：扬声器音频数据
- 字段命名：`mic_buffer`、`spk_buffer`（按你的指定）。
- 数据口径：与当前落盘一致，统一为 `PCM16LE Uint8Array`（按你的指定）。
- 上报时机：保持现有逻辑（每次落盘 flush 时上报）。

## Current State Analysis

- 进度结构定义在 `src/global.rs` 的 `RecordingProgressInfo`，当前字段仅有：
  - `total_duration`
  - `buffer`
  - `total_size`
- 进度广播由 `emit_recording_progress(total_duration, pcm_bytes, total_size)` 负责，当前仅广播合并音频字节。
- 录制循环在 `src/lib.rs` 的 `start_record_impl()`：
  - 每帧从 `mic_queue/spk_queue` 取样并生成混合输出 `sample_l/sample_r`。
  - 当前只将混合输出累计到 `flush_pcm_chunk`，flush 时调用 `emit_recording_progress(...)`。
- 导出类型在 `index.d.ts` 中，`RecordingProgressInfo` 目前仅声明 `buffer: Uint8Array`。

## Proposed Changes

### 1) `src/global.rs`

- 扩展 `RecordingProgressInfo` 结构：
  - 保留现有字段：`total_duration`、`buffer`、`total_size`
  - 新增字段：`mic_buffer: Uint8Array`、`spk_buffer: Uint8Array`
- 扩展进度广播函数签名：
  - 从 `emit_recording_progress(total_duration, pcm_bytes, total_size)`
  - 调整为 `emit_recording_progress(total_duration, mixed_pcm_bytes, mic_pcm_bytes, spk_pcm_bytes, total_size)`
- 在广播时同时构造三路 `Uint8Array`：
  - `buffer`（合并音频）
  - `mic_buffer`（麦克风）
  - `spk_buffer`（扬声器）

### 2) `src/lib.rs`

- 在 `start_record_impl()` 增加两个落盘周期缓存：
  - `flush_mic_pcm_chunk: Vec<u8>`
  - `flush_spk_pcm_chunk: Vec<u8>`
- 在每帧处理循环中，除原有混合输出外，新增两路编码：
  - 将当前帧麦克风样本（经过现有处理链后的 `mic_l/mic_r`）转换为 `i16` 并写入 `flush_mic_pcm_chunk`
  - 将当前帧扬声器样本（`spk_l/spk_r`）转换为 `i16` 并写入 `flush_spk_pcm_chunk`
  - 混合输出继续按现有逻辑写入 `flush_pcm_chunk` 并落盘
- 调整 `flush_progress` 闭包签名与调用：
  - 接收三路 chunk（mixed/mic/spk）
  - flush 时一次性调用新的 `emit_recording_progress(...)`
  - 保持 `total_duration`、`total_size` 计算逻辑不变（仍以落盘混合音频为基准）
- 暂停逻辑不变：
  - 暂停期间仅清空队列，不产生任何三路 chunk，不触发新增额外上报。

### 3) 导出类型产物（自动生成）

- 构建后刷新以下文件（由 NAPI 自动生成）：
  - `index.d.ts`
  - `index.js`
  - `perception-audio-recorder.wasi-browser.js`
  - `perception-audio-recorder.wasi.cjs`
- 重点检查 `index.d.ts` 中 `RecordingProgressInfo` 是否新增：
  - `micBuffer`（NAPI 可能将 `mic_buffer` 映射为 camelCase）
  - `spkBuffer`
- 如需保留下划线字段名暴露到 JS，执行阶段在结构体字段上增加 `#[napi(js_name = "mic_buffer")]` / `#[napi(js_name = "spk_buffer")]` 显式锁定命名。

### 4) 示例与文档同步

- 更新 `test.js` 的进度回调打印：
  - 打印合并音频、麦克风音频、扬声器音频三路 `byteLength`。
- 必要时同步 `README.md` 的 `RecordingProgressInfo` 字段说明，避免文档与导出类型不一致。

## Assumptions & Decisions

- 三路数据均采用 `PCM16LE Uint8Array`，且帧粒度与 flush 周期一致。
- `buffer` 继续代表“合并后并实际落盘”的音频，不改变既有语义。
- `mic_buffer/spk_buffer` 采用当前混音循环可得的样本口径，避免引入额外采集线程或格式转换链。
- `total_duration/total_size` 仍以合并落盘结果为准，不改统计基准。

## Verification Steps

1. 执行 `npm run build`，确保 Rust 编译通过且导出文件更新成功。
2. 检查 `index.d.ts`：
   - `RecordingProgressInfo` 含三路音频字段（合并/麦克风/扬声器）。
   - 字段命名符合预期（必要时通过 `js_name` 固定为下划线）。
3. 运行 `test.js` 或等价脚本：
   - 进度回调中三路 `byteLength` 均可读取且随录制持续变化。
   - 暂停期间进度不上报或不增长；恢复后继续上报。
4. 回归检查：
   - `pause/resume/stop`、时长统计与文件落盘行为不回退。
