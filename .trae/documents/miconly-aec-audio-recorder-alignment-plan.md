# `start_recording` 新增 `saveToFile` 参数实施计划

## Summary

- 目标：为 `src/lib.rs` 中导出的 `start_recording` / `startRecording` 新增一个末尾布尔参数，用于控制当前录制会话是否保存本地文件。

- 参数形态：
  - Rust 导出：在现有 4 个参数后追加 `save_to_file: Option<bool>`。

  - JS/TS 调用形态：在现有 `startRecording(saveDir, recordDevice, sampleRate, channels)` 后追加第 5 个参数 `saveToFile?: boolean`。

- 默认行为：
  - 未传时默认 `true`，保持当前“启动即创建本地 WAV 文件”的兼容行为。

  - 当 `saveToFile=false` 时，录制、暂停/恢复、错误回调、进度回调继续可用，但不创建/写入本地文件。

- 交付范围：
  - 修改 Rust 核心实现与参数解析。

  - 同步更新 `README.md`、`test.js`。

  - `index.d.ts` / `index.js` 不手工修改，后续通过现有工具链自动生成。

## Current State Analysis

- 当前导出入口位于 `e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`：
  - `start_recording` 当前签名为 4 个参数：`save_dir`、`record_device`、`sample_rate`、`channels`。

  - 启动时会先 `reset_recording_stats()`、`set_current_output_file(None)`，随后进入 `start_record_impl(options)`。

- 当前参数解析位于 `e:\Code\HaierGit\perception-audio-recorder\src\global.rs`：
  - `StartRecordingOptions` 仅包含 `save_dir`、`record_device`、`requested_sample_rate`、`sample_rate_override`、`channels_mode`。

  - `parse_start_recording_options(...)` 尚不支持“是否保存本地文件”。

- 当前写盘逻辑位于 `src/lib.rs` 的 `start_record_impl()`：
  - 固定创建 `WavSpec`。

  - 固定解析输出目录 `resolve_record_save_dir(options.save_dir.clone())`。

  - 固定创建 `output_<timestamp>.wav`，并通过 `hound::WavWriter::create(&full_path, spec)?` 打开本地文件。

  - 固定调用 `set_current_output_file(Some(full_path...))` 记录当前输出文件。

- 当前进度/统计语义：
  - `README.md` 已将 `RecordingProgressInfo.totalSize` 描述为“当前累计录制文件大小（字节）”。

  - 实际实现中 `RECORDED_DATA_BYTES` 在写盘主循环内累加，和文件写入链路强耦合。

- 当前仓库内对外用法已固化为位置参数：
  - `index.d.ts` 当前声明为 `startRecording(saveDir?, recordDevice?, sampleRate?, channels?)`。

  - `README.md` 已公开 4 参数示例。

  - `test.js` 当前存在 `startRecording(saveDir, 0, 16000, 'mono')` 的直接调用。

## Assumptions & Decisions

- 决策：接口继续保持位置参数风格，不改为 `options` 对象。

- 决策：新增参数放在末尾，命名语义为“是否保存本地文件”，默认 `true`。

- 决策：`saveToFile=false` 时仅禁用本地 WAV 创建/写入，不影响录制、暂停/恢复、错误回调、进度回调。

- 决策：`saveToFile=false` 且传入 `saveDir` 时，不报错；忽略该目录并输出 info 日志说明。

- 决策：`listenRecordingProgress.totalSize` 在无文件模式下返回“累计音频数据字节数”，不再严格等于磁盘文件大小；文档需明确按模式解释该字段。

- 决策：不手工修改 `index.d.ts` / `index.js`，由构建产物自动刷新。

## Proposed Changes

### 1) 扩展启动参数解析

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\global.rs`

- What：
  - 为 `StartRecordingOptions` 新增 `save_to_file: bool` 字段。

  - 为 `parse_start_recording_options(...)` 新增第 5 个参数 `save_to_file: Option<bool>`。

  - 解析规则：
    - `None` => `true`

    - `Some(value)` => 使用调用方传值

- Why：
  - 让录制主流程只依赖 `StartRecordingOptions`，避免在 `lib.rs` 各处分散判断。

- How：
  - 保持现有 `record_device/sample_rate/channels` 校验逻辑不变。

  - 仅新增布尔默认值处理，不引入新的错误分支。

### 2) 调整导出入口签名并透传新参数

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`

- What：
  - 修改 `start_recording(...)` 签名，在 `channels: Option<String>` 后新增 `save_to_file: Option<bool>`。

  - 调整 `parse_start_recording_options(...)` 调用，将新增参数传入。

- Why：
  - 保持现有公开 API 的兼容扩展路径，避免破坏已有 4 参数调用。

- How：
  - 不改变已有“重复录制报错”“线程启动”“会话状态重置”的主流程。

  - 仅在 options 构造时引入新字段。

### 3) 将写盘从“必选路径”改为“可选分支”

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`

- What：
  - 将 `start_record_impl()` 中当前固定创建 `WavWriter` 的逻辑改为条件分支：
    - `options.save_to_file == true`：保持现有目录解析、文件命名、`WavWriter::create`、`set_current_output_file(Some(...))`。

    - `options.save_to_file == false`：跳过目录解析与文件创建，`set_current_output_file(None)`，并记录“当前会话不落地文件”的日志。

- Why：
  - 当前实现里本地写盘是硬编码必走路径，必须先拆成可选 writer，后续主循环才能支持“仅采集不上盘”。

- How：
  - 抽象为 `Option<hound::WavWriter<...>>` 或等价可选 writer 持有方式。

  - `saveToFile=false` 时若用户同时传入 `saveDir`，在进入主循环前输出一条 info 日志：该目录在无文件模式下被忽略。

  - `emit_start_recording()` 仍照常触发，保证事件语义不变。

### 4) 解耦主循环中的“统计/进度”与“实际写盘”

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`

- What：
  - 将当前写样本、累计 `RECORDED_DATA_BYTES`、进度上报相关逻辑改成“先形成本轮输出数据，再分别决定是否写盘”。

- Why：
  - 如果继续把统计值绑定在 `writer.write_sample()` 上，无文件模式下 `totalSize` 会永远不增长，且进度语义被动失真。

- How：
  - 保留现有混音、队列、pause/resume、flush 节奏、40ms 进度上报等逻辑。

  - 每轮形成输出块后：
    - 无论是否写盘，都按输出帧数计算并累加“累计音频数据字节数”。

    - `save_to_file=true` 时，再把该输出块写入 `WavWriter` 并按原策略 `flush`。

    - `save_to_file=false` 时，跳过 `writer.write_sample` / `writer.flush`，但不影响 `emit_recording_progress(...)` 的时机与内容。

  - 文档中需明确：
    - 有文件模式：`totalSize` 可视为累计输出文件数据规模。

    - 无文件模式：`totalSize` 表示累计音频数据字节数。

### 5) 校正收尾与状态清理

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`

- What：
  - 确保录制结束时两种模式都能正确收尾。

- Why：
  - 当前收尾默认隐含存在 writer；改为可选 writer 后，结束分支、flush、drop、当前文件状态清理都要防空处理。

- How：
  - 有文件模式：
    - 保持最终 flush / writer 正常释放。

    - 结束后清空 `CURRENT_OUTPUT_FILE`。

  - 无文件模式：
    - 不做文件 flush。

    - 结束后同样确保 `CURRENT_OUTPUT_FILE=None`，避免残留旧路径状态。

  - 两种模式下统一保留：
    - `set_current_output_file(None)`

    - `STOP_REQUESTED / IS_PAUSED / IS_RECORDING` 状态复位

    - `emit_stop_recording()`

### 6) 更新 README 对外契约

- 文件：`e:\Code\HaierGit\perception-audio-recorder\README.md`

- What：
  - 更新 `startRecording` API 列表与示例，新增第 5 个参数说明。

  - 更新 `totalSize` 字段说明，区分保存模式与无文件模式。

  - 补充 `saveToFile=false` 的行为说明与示例。

- Why：
  - 当前 README 已明确公开接口签名与语义；如果只改代码不改文档，使用方会直接按旧理解接入。

- How：
  - 在 `startRecording` 参数增强章节补充：
    - `saveToFile`：是否保存本地文件，可选，默认 `true`

  - 新增示例：
    - `startRecording(undefined, 0, 16000, 'mono', false)` 表示仅麦克风录制并实时上报，不落本地文件。

  - 在 `RecordingProgressInfo.totalSize` 描述中补一条模式说明。

### 7) 更新示例脚本以覆盖新模式

- 文件：`e:\Code\HaierGit\perception-audio-recorder\test.js`

- What：
  - 调整 `startRecording(...)` 调用示例，明确支持传入第 5 个参数。

  - 如脚本中有依赖本地输出文件路径/文件大小的日志，补充对 `saveToFile=false` 的兼容说明或验证日志。

- Why：
  - `test.js` 是仓库内现成联调脚本，也是最直接的手工回归入口。

- How：
  - 保持现有测试脚本主流程不做无关重构。

  - 最小改动方式加入一个显式调用示例，例如：
    - 默认模式：继续走保存文件

    - 或增加注释说明如何切换为 `false` 做“仅实时流处理”验证

  - 若脚本存在“最终 wav finalized”类日志，应按模式分支输出，避免无文件模式下误报。

## Verification Steps

1. 构建验证
   - 执行项目现有构建流程，确认 Rust 编译通过。

   - 确认工具链自动刷新后的 `index.d.ts` / `index.js` 中 `startRecording` 已带第 5 个可选布尔参数。

2. 兼容性验证
   - 保持旧调用 `startRecording(saveDir, 0, 16000, 'mono')` 不变，确认默认仍会写出本地 WAV 文件。

3. 无文件模式验证
   - 调用 `startRecording(undefined, 0, 16000, 'mono', false)`。

   - 确认不会创建本地 WAV 文件。

   - 确认 `listenRecordingProgress` 仍持续上报。

   - 确认 `getRecordDuration()`、pause/resume、stop 语义保持正常。

4. 参数组合验证
   - 调用 `startRecording('C:\\temp', 0, 16000, 'mono', false)`。

   - 确认不报参数错，并在日志中看到 `saveDir` 被忽略的提示。

5. `totalSize` 语义验证
   - 有文件模式下：`totalSize` 随录制推进递增。

   - 无文件模式下：`totalSize` 仍递增，但 README 已明确其含义为累计音频数据字节数。

6. 文档与示例验证
   - 检查 `README.md` 示例、参数表、字段说明与最终实现保持一致。

   - 检查 `test.js` 中的调用方式与新接口一致，可直接用于联调。
