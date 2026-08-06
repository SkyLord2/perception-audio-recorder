# 录音错误监听与单侧数据包容错改造计划

## Summary

- 目标：新增 `listen_recording_error` 能力，并增强录制主循环在“单侧有包/双侧无包/设备不可用”场景下的行为。
- 核心要求落地：
  - 新增错误回调 `listen_recording_error`，上报错误类型、错误信息等。
  - 麦克风/扬声器只有一侧有数据包时，不影响 `listen_recording_progress` 上报。
  - 连续 5 秒双侧都无声音数据包时，上报错误（仅上报，不自动停止录制）。
  - 麦克风和扬声器设备都不可用时，向上层抛出错误。
  - 在易错点补充 `report_error_log`。
  - 同步更新 `README.md` 文档。

## Current State Analysis

- 当前 `src/global.rs`：
  - 已有进度结构 `RecordingProgressInfo`（`buffer/mic_buffer/spk_buffer`）与多监听容器。
  - 尚无“录音错误监听器”结构与错误对象模型。
- 当前 `src/lib.rs`：
  - `start_record_impl()` 里用 `available_frames = min(mic_queue.len(), spk_queue.len()) / TARGET_CHANNELS`。
  - 这导致单侧无包时 `available_frames=0`，进度上报和写盘都停滞。
  - 设备获取当前是：
    - `default_input_device().ok_or("未找到默认麦克风输入设备")?`
    - `default_output_device().ok_or("未找到默认扬声器输出设备")?`
    - 一旦任一不存在就立即返回，尚未区分“都不可用”与“单侧不可用”。
  - 错误日志主要集中在外层 `start_recording` 线程捕获，细粒度易错点日志不足。
- 当前导出类型 `index.d.ts`：
  - 尚无 `listenRecordingError` 与对应错误对象类型。
- 当前 `README.md`：
  - 尚未说明错误监听与“单侧有包仍上报”的容错策略。

## Proposed Changes

### 1) `src/global.rs`：新增错误回调模型与广播能力

- 新增 NAPI 错误对象（camelCase）：
  - `RecordingErrorInfo`：
    - `errorType: String`
    - `errorMessage: String`
    - `errorCode: i32`（可选业务码，未定义时填 0）
    - `occurredAt: String`（时间字符串）
- 新增监听器容器：
  - `RECORDING_ERROR_LISTENERS: OnceLock<Mutex<Vec<ThreadsafeFunction<RecordingErrorInfo>>>>`
- 新增注册与广播函数：
  - `register_recording_error_listener(...)`
  - `emit_recording_error(error_type, error_message, error_code)`
- 在广播失败或锁失败时增加 `report_error_log!`，确保错误链路可观测。

### 2) `src/lib.rs`：新增导出接口与核心循环容错

- 新增导出方法：
  - `listen_recording_error(listener: ThreadsafeFunction<RecordingErrorInfo>) -> napi::Result<()>`
- 设备可用性处理改造：
  - 获取 `mic_device`、`spk_device` 后做组合判定：
    - **两者都不可用**：立即返回 `Err(...)`（向上层抛错），并打 `report_error_log!`。
    - **仅一侧可用**：允许继续录制（可用侧真实数据，不可用侧补静音），并打 `report_error_log!` + `emit_recording_error(...)` 提醒降级运行。
- 单侧数据包容错（核心）：
  - 不再硬依赖 `min(mic,spk)` 才推进。
  - 以“任一侧有数据即可推进”为原则：
    - 计算每侧可用帧数。
    - 取 `frames_to_process = max(mic_frames, spk_frames)`。
    - 缺失侧样本用 `0.0` 补齐，保证混音、三路 chunk、进度上报持续进行。
- 双侧无包错误检测：
  - 增加“最近收到任一包”的时间戳 `last_packet_at`。
  - 在循环中如果连续 5 秒未收到任一侧数据包：
    - 调用 `emit_recording_error("NoAudioPacketTimeout", "...", code)`。
    - 打 `report_error_log!`。
    - **仅上报，不自动 stop/pause**，并重置计时起点避免日志风暴。
- 易错点 `report_error_log` 增强位置：
  - 设备获取/组合判定失败。
  - 启动单侧流失败与降级。
  - 进度 flush 失败路径。
  - 双侧无包超时触发点。
  - 错误监听广播异常点（通过 `global.rs` 辅助函数处理）。

### 3) `src/capture.rs`：为单侧启动提供更清晰错误语义（必要时）

- 保持 `start_stream` 接口不变。
- 在 `stream.play()`/构建回调错误时补充上下文日志（Mic/Spk 标识）。
- 不改动数据处理链路，避免超出本次需求边界。

### 4) 导出与类型产物同步

- 构建后自动刷新：
  - `index.d.ts`：新增
    - `listenRecordingError(listener: (err, arg: RecordingErrorInfo) => any): void`
    - `RecordingErrorInfo` 类型（camelCase 字段）
  - `index.js` / `perception-audio-recorder.wasi-browser.js` / `perception-audio-recorder.wasi.cjs` 同步导出。

### 5) `README.md` 同步更新

- 新增“错误监听”章节：
  - `listenRecordingError` 用法、字段含义、触发场景。
- 更新“进度上报行为”说明：
  - 单侧有包仍会上报（另一侧补静音）。
  - 连续 5 秒双侧无包会触发错误回调，但录制继续。
- 更新示例代码，包含 `listenRecordingError` 监听与日志打印。

## Assumptions & Decisions

- 无包超时阈值固定为 **5 秒**。
- 超时后动作固定为“仅上报继续录制”，不自动停止。
- 错误对象字段采用 **camelCase**：`errorType/errorMessage/errorCode/occurredAt`。
- 单侧不可用时采用“另一侧真实数据 + 缺失侧补 0”策略，优先保证进度与录制可持续。
- 统计口径保持不变：`totalDuration/totalSize` 仍以合并落盘数据为准。

## Verification Steps

1. 构建验证：`npm run build` 成功，导出产物刷新。
2. 类型验证：
   - `index.d.ts` 出现 `listenRecordingError` 与 `RecordingErrorInfo`。
   - 进度类型仍包含 `buffer/micBuffer/spkBuffer`。
3. 单侧数据验证（关键）：
   - 模拟仅麦克风有包或仅扬声器有包时，`listenRecordingProgress` 持续上报。
   - 检查缺失侧 `byteLength` 与内容符合补静音预期。
4. 双侧无包验证：
   - 连续 5 秒无包触发 `listenRecordingError`，录制线程仍继续运行。
5. 设备不可用验证：
   - 麦克风与扬声器都不可用时，`startRecording` 失败并向上层抛错。
6. 日志验证：
   - 关键易错点能看到 `report_error_log` 输出，便于定位。
7. 文档回归：
   - `README.md` API 与行为描述与实现一致。
