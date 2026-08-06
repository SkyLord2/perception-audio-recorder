# 音频处理开关配置改造计划

## Summary

- 目标：增加可配置开关，分别控制以下能力是否启用：
  - 麦克风高通（High-pass）
  - 麦克风降噪（Noise Suppression）
  - 麦克风 AGC（自动增益）
  - 麦克风软限幅（Soft Limiter）
  - 扬声器触发的麦克风衰减（Ducking）
- 约束：保持现有录制 API 主流程可用，默认行为与当前版本一致（即 5 个开关默认都开启）。
- 交付形态：新增 NAPI 配置接口 + 处理链路按开关条件执行 + README 同步说明。

## Current State Analysis

- `src/processor.rs`
  - 麦克风处理链（高通/降噪/AGC/软限幅）均为硬编码启用，且集中在 `AudioProcessor::process()` 内。
  - 当前仅区分 `is_mic`，没有进一步按功能粒度开关。
- `src/lib.rs`
  - 扬声器触发麦克风衰减逻辑在录制主循环中固定执行（`spk_abs > ECHO_SUPPRESS_THRESHOLD` 分支）。
  - 没有“配置对象”或“会话配置快照”机制。
- `src/global.rs`
  - 有全局常量和状态，但暂无音频处理开关结构体与全局配置存储。
- `index.d.ts` / `README.md`
  - 暂无用于设置处理开关的导出方法与文档说明。

## Proposed Changes

### 1) 新增配置模型与全局存储

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\global.rs`
- What：
  - 新增 `#[napi(object)]` 配置结构体（建议名：`AudioProcessingConfig`）：
    - `enable_high_pass: bool`
    - `enable_noise_suppression: bool`
    - `enable_agc: bool`
    - `enable_soft_limiter: bool`
    - `enable_speaker_ducking: bool`
  - 新增默认配置函数（默认全 `true`）。
  - 新增全局存储：`OnceLock<Mutex<AudioProcessingConfig>>`。
  - 新增读写函数：
    - `set_audio_processing_config_global(cfg: AudioProcessingConfig)`
    - `get_audio_processing_config_global() -> AudioProcessingConfig`
- Why：
  - 提供统一来源，避免开关散落在多个模块难以维护。
- How：
  - 通过 `Mutex` 保证跨线程读写安全；配置结构体实现 `Clone`，便于会话启动时快照传递。

### 2) 暴露 NAPI 配置接口

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - 新增导出函数：
    - `set_audio_processing_config(config: AudioProcessingConfig) -> napi::Result<()>`
    - `get_audio_processing_config() -> AudioProcessingConfig`
  - 在 `start_record_impl()` 启动时读取一次配置快照并记录日志。
- Why：
  - 满足“通过配置开关控制处理链”的需求，且不破坏现有 `startRecording(saveDir?)` 签名。
- How：
  - `set` 在录制中调用时返回错误（建议）：`录制进行中不允许修改音频处理配置，请先停止录制`。
  - 配置“会话级生效”：启动时快照，当前会话中不热更新，下一次录制生效，降低状态竞争风险。

### 3) 让麦克风处理链按开关执行

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\processor.rs`
- What：
  - `AudioProcessor` 新增配置字段（保存开关快照）。
  - `AudioProcessor::new(...)` 增加配置参数并存储。
  - 在 `process()` 内将麦克风处理拆成可独立开关：
    - 高通：仅 `enable_high_pass` 为 `true` 时执行
    - 降噪：仅 `enable_noise_suppression` 为 `true` 时计算并应用 `ns_gain`
    - AGC：仅 `enable_agc` 为 `true` 时计算并应用 `agc_gain`
    - 两者增益组合：`total_gain = ns_gain * agc_gain`（某项关闭则该项按 `1.0`）
    - 软限幅：仅 `enable_soft_limiter` 为 `true` 时执行 `tanh` 限幅
- Why：
  - 满足“分别控制”而非“整体开关”。
- How：
  - 保持“仅麦克风链路应用这些处理”的边界不变，扬声器处理路径不引入新处理。

### 4) 让扬声器触发衰减按开关执行

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - 在主循环中将 `spk_abs` 触发的麦克风衰减逻辑包裹在 `enable_speaker_ducking` 条件内。
- Why：
  - 这是用户明确要求的第 5 个可控开关。
- How：
  - 关闭开关时跳过 `duck` 计算，保留其余混音路径不变。

### 5) 贯通调用链与类型导出

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\capture.rs`
- What：
  - `start_stream(...)` 与 `AudioProcessor::new(...)` 增加配置参数透传。
- Why：
  - 让会话配置快照从录制入口传到处理器。
- How：
  - 以只读值传递，避免在回调线程做锁操作。

- 文件：`e:\Code\HaierGit\perception-audio-recorder\index.d.ts`（构建产物）
- What：
  - 通过 `npm run build` 自动生成新的 TS 声明：
    - `AudioProcessingConfig` 接口
    - `setAudioProcessingConfig(...)`
    - `getAudioProcessingConfig()`

### 6) 文档更新

- 文件：`e:\Code\HaierGit\perception-audio-recorder\README.md`
- What：
  - 新增“音频处理开关”章节，说明 5 个开关的默认值、生效时机（下一次录制）与调用示例。
- Why：
  - 避免调用方误解“录制中热更新”行为。

## Assumptions & Decisions

- 决策 1：默认配置全部开启，保证升级后行为与当前版本一致。
- 决策 2：配置采用“会话级快照”，录制中不允许修改；若需切换，先 `stopRecording` 再设置。
- 决策 3：不改 `startRecording` 入参，新增独立设置/读取接口，减少对现有调用方影响。
- 决策 4：仅引入开关控制，不调整现有处理参数（阈值、增益系数等）数值本身。

## Verification Steps

1. 构建验证
   - 执行 `npm run build`，确认 Rust 编译通过并生成最新 `index.d.ts`。
2. 接口验证
   - 调用 `getAudioProcessingConfig()`，确认默认值全为 `true`。
   - 调用 `setAudioProcessingConfig({...})` 后再次读取，确认设置生效。
3. 行为验证（逐项）
   - 关闭 `enable_speaker_ducking`：扬声器放音时麦克风不再被额外压低。
   - 关闭 `enable_high_pass`：低频成分明显增加（可通过频谱/听感对比）。
   - 关闭 `enable_noise_suppression`：静音段底噪回升。
   - 关闭 `enable_agc`：说话远近导致音量变化更明显。
   - 关闭 `enable_soft_limiter`：大音量时削顶风险上升（可观察波形峰值）。
4. 兼容性验证
   - 不调用新接口时，录制行为与改造前一致。
   - 录制中调用 `setAudioProcessingConfig` 返回预期错误，不影响当前会话稳定性。
