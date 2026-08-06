# 录音采样率对齐与单侧重采样改造计划

## Summary

- 目标：在录制启动时获取并对比 `mic_sr` 与 `spk_sr`，按会话动态决定 WAV 输出采样率与重采样策略。
- 目标策略：
  - 若 `mic_sr == spk_sr`：不启用重采样，WAV 使用该相同采样率。
  - 若 `mic_sr != spk_sr`：以 `mic_sr` 作为会话基准采样率，仅对扬声器流重采样到 `mic_sr`。
- 已确认偏好：
  - 麦克风不可用、仅扬声器可用时：WAV 采样率采用 `spk_sr`。
  - “获取并对比采样率”仅通过 `report_info_log!` 输出，不新增对外导出 API。

## Current State Analysis

- `src/lib.rs`
  - `start_record_impl` 负责设备发现、启动流、创建 WAV 写入器。
  - 当前 `WavSpec.sample_rate` 固定为 `TARGET_SAMPLE_RATE`，未使用设备真实采样率。
  - 混音/节拍预算等逻辑均以 `TARGET_SAMPLE_RATE` 作为时基。
- `src/capture.rs`
  - `start_stream` 内已能读取设备 `sample_rate/channels`，并已通过 `report_info_log!` 打印。
  - `AudioProcessor::new` 当前签名只接受 `source_sample_rate`，目标采样率固定写死到全局常量路径。
- `src/processor.rs`
  - 重采样开关条件是 `source_sample_rate != TARGET_SAMPLE_RATE`。
  - `ratio`、高通滤波 `dt`、输出缓冲长度估算都隐式绑定 `TARGET_SAMPLE_RATE`。
- `src/global.rs`
  - 存在 `TARGET_SAMPLE_RATE` 常量，当前被多个模块用于“固定目标采样率”。

## Proposed Changes

### 1) `src/capture.rs`：流启动返回“运行时元信息”

- **What**
  - 新增一个流启动结果结构体（例如 `StartedStream`），包含：
    - `stream: cpal::Stream`
    - `sample_rate: usize`
    - `channels: usize`
  - 将 `start_stream` 返回类型从 `AppResult<cpal::Stream>` 调整为 `AppResult<StartedStream>`。
  - `start_stream` 增加参数 `target_sample_rate: usize`，传给 `AudioProcessor::new(...)`。
- **Why**
  - `lib.rs` 需要在同一时刻拿到 mic/spk 实际采样率做策略决策。
  - 重采样目标不再全局固定，改为会话级动态值。
- **How**
  - 获取默认配置后读取 `sample_rate/channels`。
  - 保持现有日志输出，并新增“目标采样率 + 是否预计重采样”的日志字段。
  - 回调和队列逻辑不变，避免扩大改动面。

### 2) `src/processor.rs`：改为“源采样率 -> 会话目标采样率”模型

- **What**
  - 修改 `AudioProcessor::new` 签名为：
    - `new(source_sample_rate: usize, target_sample_rate: usize, input_channels: usize, is_mic: bool)`
  - 内部所有 `TARGET_SAMPLE_RATE` 参与运算的位置改为 `target_sample_rate` 字段。
  - `use_resampler` 条件改为 `source_sample_rate != target_sample_rate`。
- **Why**
  - 支持“仅扬声器重采样到 mic_sr”与“同采样率直通”两种策略。
- **How**
  - 新增结构字段 `target_sample_rate: usize`。
  - `ratio = target / source`。
  - 高通滤波 `dt = 1.0 / target_sample_rate as f32`，保证算法时基与输出一致。
  - `final_output` 仍输出双声道交错格式，保持下游兼容。

### 3) `src/lib.rs`：会话级采样率决策 + WAV 动态采样率

- **What**
  - 在启动流前先读取设备默认配置采样率（只读），得到 `mic_sr_opt` 与 `spk_sr_opt`。
  - 根据设备可用性与用户规则决策 `session_sample_rate`：
    - mic+spk 都可用：
      - 相等：`session_sample_rate = mic_sr`，双流不重采样。
      - 不等：`session_sample_rate = mic_sr`，仅 spk 流重采样到 `mic_sr`。
    - 仅 mic：`session_sample_rate = mic_sr`，不重采样。
    - 仅 spk：`session_sample_rate = spk_sr`，不重采样（已确认）。
  - `WavSpec.sample_rate` 改为 `session_sample_rate as u32`。
  - 启动 `capture::start_stream` 时将 `session_sample_rate` 传入。
  - 用 `report_info_log!` 输出 `mic_sr/spk_sr/decision`。
- **Why**
  - 满足“智能判断是否重采样 + WAV 采用动态采样率”核心需求。
- **How**
  - 增加一段“策略判定函数”或内联逻辑，避免分支散落。
  - 当前预算限流与时间估算中依赖 `TARGET_SAMPLE_RATE` 的位置，替换为会话变量（例如 `session_sr`），避免时长与节拍偏差。
  - 队列容量等与时间强相关的计算（如 `max_queue_samples`）改为基于 `session_sr`。

### 4) `src/global.rs`：最小化常量语义调整

- **What**
  - 保留 `TARGET_SAMPLE_RATE` 作为“默认/回退常量”与编译期基线，不再强制代表实际会话输出采样率。
  - 如有必要，注释说明“实际输出采样率由会话策略决定”。
- **Why**
  - 降低一次性大范围重构风险，兼容当前代码结构。
- **How**
  - 仅修改注释或少量引用点，避免引入额外全局状态。

### 5) 文档与测试样例同步

- **What**
  - 更新 `README.md` 中“实现原理/容错行为”相关段落，新增采样率决策说明。
  - 如 `test.js` 中有固定 `48000` 的 WAV 头示例，补充注释说明“示例需与会话采样率一致”，或改为读取实际会话采样率后写头。
- **Why**
  - 防止代码行为与文档示例不一致。
- **How**
  - 文档以行为描述为主，不新增不必要 API。

## Assumptions & Decisions

- 决策已锁定：
  - 仅日志暴露 `mic_sr/spk_sr` 与决策结果，不新增 JS 导出接口。
  - 仅扬声器可用时，`session_sample_rate = spk_sr`。
- 兼容性约束：
  - 维持现有录制 API（`startRecording/stopRecording/...`）签名不变。
  - 不改变 `listenRecordingProgress` 的数据结构。
- 风险点：
  - 预算限流/时长统计若仍引用固定 `TARGET_SAMPLE_RATE`，会产生时长偏差；本次会一并改为会话采样率时基。
  - 示例脚本若写死 `48000`，可能导致外部分析工具解读异常；需同步说明或修正。

## Verification Steps

1. 构建验证
   - 执行 `npm run build`，确保 Rust 编译与 NAPI 生成成功。
2. 日志验证（采样率获取与决策）
   - 启动录制，检查日志包含：
     - `mic_sr`、`spk_sr`
     - 是否相等
     - `session_sample_rate`
     - 哪一路启用重采样
3. 功能验证（同采样率场景）
   - 当 mic/spk 同采样率时，日志应显示“双路不重采样”，输出 WAV 采样率为该值。
4. 功能验证（异采样率场景）
   - 当 mic/spk 异采样率时，日志应显示“仅 spk 重采样到 mic_sr”，输出 WAV 采样率为 `mic_sr`。
5. 单侧验证
   - 仅 mic 可用：WAV 采样率 = `mic_sr`。
   - 仅 spk 可用：WAV 采样率 = `spk_sr`。
6. 回归验证
   - `listenRecordingProgress.totalDuration` 单调递增且会话切换后从 0 开始。
   - pause/resume、stop 后再 start 不出现时长串会话问题。
