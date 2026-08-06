# 根据音频处理开关计划文档的实施编码计划

## Summary

- 目标：按 `audio-processing-switches-plan.md` 落地编码，新增 5 个处理开关并打通 NAPI 导出、录制链路与文档。
- 范围：
  - 代码：`src/global.rs`、`src/lib.rs`、`src/capture.rs`、`src/processor.rs`
  - 文档：`README.md`
  - 产物：`index.d.ts`（通过构建自动生成）
- 成功标准：
  - 5 个开关可通过新接口配置并查询。
  - 开关在录制链路按预期生效。
  - 默认行为与当前版本相反（全关闭）。
  - 构建通过，类型导出可用，README 同步。

## Current State Analysis

- `src/processor.rs`：麦克风高通/降噪/AGC/软限幅为固定启用，暂无分项开关。
- `src/lib.rs`：扬声器触发麦克风衰减固定启用，暂无会话配置快照。
- `src/global.rs`：暂无音频处理配置模型与全局配置存储。
- `index.d.ts`：暂无 `setAudioProcessingConfig/getAudioProcessingConfig` 导出。
- `README.md`：暂无开关配置说明。

## Proposed Changes

### 1) `global.rs`：新增配置模型与全局读写

- What：
  - 新增 `#[napi(object)] pub struct AudioProcessingConfig`，包含：
    - `enable_high_pass`
    - `enable_noise_suppression`
    - `enable_agc`
    - `enable_soft_limiter`
    - `enable_speaker_ducking`
  - 新增默认配置函数：默认全 `true`。
  - 新增全局存储：`OnceLock<Mutex<AudioProcessingConfig>>`。
  - 新增全局读写函数：`set_audio_processing_config_global`、`get_audio_processing_config_global`。
- Why：
  - 统一配置来源，避免多模块重复状态。
- How：
  - `AudioProcessingConfig` 实现 `Clone`，支持会话快照传递。

### 2) `lib.rs`：新增 NAPI 导出与会话快照

- What：
  - 新增导出：
    - `set_audio_processing_config(config: AudioProcessingConfig) -> napi::Result<()>`
    - `get_audio_processing_config() -> AudioProcessingConfig`
  - 在 `start_record_impl` 开始时读取配置快照并日志打印。
  - 在录制中调用 `set` 时返回错误（不允许热更新）。
  - 在主循环中将扬声器触发衰减逻辑包裹 `enable_speaker_ducking`。
- Why：
  - 对外提供可控配置入口并保证会话稳定性。
- How：
  - 配置“会话级生效”：每次 `startRecording` 时读取一次快照，不在回调线程加锁读取。

### 3) `capture.rs`：配置透传到处理器

- What：
  - `start_stream(...)` 增加 `AudioProcessingConfig` 参数（只读快照）。
  - 传入 `AudioProcessor::new(...)`。
- Why：
  - 打通配置到每个流处理器实例。
- How：
  - Mic/Spk 分别共享同一会话快照（按内部 `is_mic` 决定实际生效项）。

### 4) `processor.rs`：按开关执行麦克风处理链

- What：
  - `AudioProcessor` 新增配置字段。
  - `AudioProcessor::new(...)` 增加配置入参并存储。
  - 在 `process()` 中：
    - 高通仅在 `enable_high_pass` 开启时执行。
    - 降噪仅在 `enable_noise_suppression` 开启时生效。
    - AGC 仅在 `enable_agc` 开启时生效。
    - 软限幅仅在 `enable_soft_limiter` 开启时生效。
    - 增益组合逻辑保持可叠加，关闭项按 `1.0` 处理。
- Why：
  - 实现“分别控制”而不是整体 on/off。
- How：
  - 保持处理边界：仅 Mic 分支执行上述逻辑，Spk 分支不变。

### 5) 类型导出与文档同步

- What：
  - 执行构建生成新的 `index.d.ts` 导出。
  - 更新 `README.md`：新增开关字段、默认值、调用示例、生效时机与录制中设置限制。
- Why：
  - 确保调用方与实现一致，降低误用成本。

## Assumptions & Decisions

- 默认值全关。
- 录制中不允许更新配置（返回错误），下一次录制生效。
- 不改 `startRecording` 参数签名，新增独立 set/get 接口。
- 本次只做开关化，不改既有参数数值。

## Verification steps

1. 代码诊断
   - 对改动文件执行诊断，确保无新增错误。
2. 构建验证
   - 执行 `npm run build`，确保编译通过并更新 `index.d.ts`。
3. 接口验证
   - `getAudioProcessingConfig()` 默认全 `true`。
   - `setAudioProcessingConfig({...})` 后读取一致。
   - 录制中调用 `set` 返回预期错误。
4. 行为抽检
   - 分别关闭每个开关，对应处理行为发生变化。
5. 回归验证
   - 不调用新接口时，录制行为与改造前一致。
