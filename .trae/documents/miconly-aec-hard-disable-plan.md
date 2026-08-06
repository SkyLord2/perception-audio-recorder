# MicOnly 场景 AEC 真正关闭修复计划

## Summary

- 目标：修复 `record_device=MicOnly` 场景下 AEC 只是打印“自动降级”日志、但麦克风 `AudioProcessing` 仍按开启 AEC 构建的问题。
- 结果要求：
  - `MicOnly` 启动时，麦克风处理链路中的 AEC 必须在构造期就真正关闭。
  - `MicOnly` 不再打印“无可用扬声器参考流，自动降级”日志。
  - 仅保留降噪和自动增益等仍然可独立工作的 3A 能力。
  - 双路录制或设备缺失导致的真实降级日志行为保持可追踪，不影响现有非 `MicOnly` 逻辑。

## Current State Analysis

- `lib.rs` 在 `RecordDeviceMode::MicOnly` 下会先将 `spk_device = None`，见 `src/lib.rs`。
- 但 `capture::start_stream()` 调用 `AudioProcessor::new()` 时，并没有把“本次会话是否存在 render 参考流”传进去，见 `src/capture.rs`。
- `AudioProcessor::new()` 当前仅根据 `is_mic && ENABLE_MIC_ECHO_CANCELLATION` 计算 `aec_enabled`，见 `src/processor.rs`。
- 因此在 `MicOnly` 下：
  - 麦克风 APM 仍会带上 `EchoCanceller::default()` 构建。
  - 后续由于没有扬声器流，`feed_render_frame()` 永远没有 render 输入。
  - `lib.rs` 只是在流启动后通过 `processor.aec_enabled() && !spk_active` 打一条“自动降级”日志，见 `src/lib.rs`。
- 根因是：AEC 的“是否启用”在处理器构造时缺少会话级上下文，只看全局编译开关，不看当前录制模式与 render 参考流是否可能存在。

## Proposed Changes

### 1. `src/processor.rs`

- 将 `AudioProcessor::new(...)` 的接口从“内部自行推导 AEC 是否启用”改为“显式接收是否启用 AEC 的会话级参数”。
- 计划新增一个布尔参数，例如 `enable_aec`，用于表达本次麦克风处理器是否允许构建 AEC。
- 调整内部逻辑：
  - `aec_enabled` 改为使用 `is_mic && enable_aec`。
  - `Config.echo_canceller` 仅在该布尔值为 `true` 时配置 `EchoCanceller::default()`。
  - 其余 `NoiseSuppression` / `GainController2` 开关逻辑保持不变。
- 保留 `aec_enabled()` getter 作为“当前处理器是否真实启用了 AEC”的事实来源。
- 补充注释，明确该标志表示“有效启用状态”，而不是“全局开关请求状态”。

### 2. `src/capture.rs`

- 扩展 `start_stream(...)` 接口，增加与 AEC 相关的显式参数，例如 `enable_aec: bool`。
- 在构造 `AudioProcessor::new(...)` 时，将该参数透传给处理器。
- 约束：
  - 扬声器流 `is_mic = false` 时，该参数即使传入也不会生效，但接口仍保持统一，减少分支复杂度。
  - 注释中说明：是否启用 AEC 由 `lib.rs` 的会话级决策负责，`capture.rs` 仅负责透传。

### 3. `src/lib.rs`

- 在启动麦克风流之前，基于当前会话上下文计算“有效 AEC 启用状态”，而不是让 `processor.rs` 自己猜。
- 具体决策：
  - `MicOnly`：强制 `enable_mic_aec = false`。
  - `SpkOnly`：不存在麦克风流，无需 AEC。
  - `MicAndSpk`：
    - 若当前已选择/保留扬声器设备，则允许 `enable_mic_aec = ENABLE_MIC_ECHO_CANCELLATION`。
    - 若扬声器设备在模式筛选后已不可用，则 `enable_mic_aec = false`，视为会话级降级。
- 将上述 `enable_mic_aec` 传给麦克风 `capture::start_stream(...)`。
- 对“自动降级”日志进行语义收紧：
  - `MicOnly` 场景不再打印自动降级日志。
  - 仅当用户配置或会话模式本来期望存在 AEC 参考流，但最终没有可用扬声器流时，才打印降级日志。
- 日志建议区分两种语义：
  - `MicOnly`：直接说明“当前模式不启用 AEC”，不属于降级。
  - `MicAndSpk` 但无有效扬声器流：说明“本次会话降级关闭 AEC，仅保留降噪/自动增益”。

### 4. 文档与注释范围

- 本次需求未明确要求改 README 或导出接口，默认不修改外部文档。
- 仅在受影响代码处补充注释，解释：
  - `MicOnly` 为什么必须在构造期关闭 AEC。
  - “模式不启用”与“运行时降级”是两种不同语义。

## Assumptions & Decisions

- 决策：本次修复严格以 `MicOnly` 场景为主，不额外重构 APM 的运行时重建能力。
- 决策：AEC 是否真实启用，应由 `lib.rs` 在掌握录制模式和设备可用性后一次性决定，而不是由 `processor.rs` 仅凭全局开关推断。
- 决策：`processor.aec_enabled()` 返回“真实有效状态”；如果需要表达“用户请求过但被降级”，由 `lib.rs` 通过模式和设备上下文单独判断并记录日志。
- 假设：当前非 `MicOnly` 场景允许保留现有“先启动 Mic，再启动 Spk”的顺序，不在本次修复中调整流启动顺序。
- 假设：本次验收重点是“MicOnly 不再构建 AEC 且不再打印误导日志”，不把“Spk 流启动失败后动态重建 APM”纳入本轮范围。

## Verification Steps

- 代码级验证：
  - 检查 `src/processor.rs` 中 `aec_enabled` 不再直接等于 `ENABLE_MIC_ECHO_CANCELLATION`。
  - 检查 `src/lib.rs` 中 `MicOnly` 路径传入的 `enable_mic_aec` 为 `false`。
  - 检查 `src/lib.rs` 的自动降级日志条件不会在 `MicOnly` 下触发。
- 编译验证：
  - 运行 `cargo check`，确保 `AudioProcessor::new()` 与 `capture::start_stream()` 的签名变更已全部同步。
- 行为验证：
  - 以 `record_device=0` 启动录制，确认日志中不再出现“AEC 已启用，但本次会话无可用扬声器参考流，自动降级...”。
  - 以 `record_device=0` 启动录制，确认麦克风链路仍保留降噪/自动增益能力。
  - 以 `record_device=2` 且扬声器不可用/启动失败场景验证，确认仍可输出合理的 AEC 降级日志。
