# 移除高通/降噪/AGC/软限幅/ducking 处理链实施计划

## Summary

- 目标：彻底移除以下处理能力及其配置入口：
  - 麦克风高通（High-pass）
  - 麦克风降噪（Noise Suppression）
  - 麦克风 AGC
  - 麦克风软限幅（Soft Limiter）
  - 扬声器触发麦克风衰减（Ducking）
- 已确认决策：
  - 删除 `setAudioProcessingConfig/getAudioProcessingConfig` 导出接口。
  - 删除 `AudioProcessingConfig` 模型及文档字段（彻底清理，不保留兼容壳）。

## Current State Analysis

- `src/processor.rs`
  - 麦克风处理链逻辑（高通/降噪/AGC/软限幅）都在 `AudioProcessor` 内，并由 `processing_config` 控制开关。
  - `AudioProcessor` 当前持有大量仅处理链使用的状态字段（`hp_*`、`agc_*`、`ns_*`、`soft_k`）。
- `src/lib.rs`
  - ducking 在主循环中通过 `processing_config.enable_speaker_ducking` 控制。
  - 导出接口 `set_audio_processing_config/get_audio_processing_config` 仍存在。
  - 启动日志中输出了处理开关状态。
- `src/capture.rs`
  - `start_stream` 与 `AudioProcessor::new` 仍透传 `processing_config`。
- `src/global.rs`
  - 定义了 `AudioProcessingConfig`、全局存储 `AUDIO_PROCESSING_CONFIG` 及 set/get 全局函数。
- `index.d.ts` / `README.md`
  - 暴露了 `AudioProcessingConfig` 类型与 set/get API，文档中有对应章节。

## Proposed Changes

### 1) 删除配置模型与全局存储

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\global.rs`
- What：
  - 删除 `AudioProcessingConfig` 结构体及 `Default` 实现。
  - 删除 `AUDIO_PROCESSING_CONFIG` 全局存储。
  - 删除 `set_audio_processing_config_global/get_audio_processing_config_global` 函数。
- Why：
  - 已确认要彻底移除处理链，不再需要配置模型。

### 2) 删除 NAPI 配置接口与相关调用

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - 删除 `set_audio_processing_config/get_audio_processing_config` 导出函数。
  - 删除导入与调用：
    - `AudioProcessingConfig`
    - `get_audio_processing_config_global/set_audio_processing_config_global`
  - 删除处理开关日志输出。
- Why：
  - 对外接口与实现保持一致，避免出现“接口存在但无效”的语义歧义。

### 3) 移除 ducking 逻辑

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - 删除 `spk_abs` 触发的 `mic_l/mic_r` duck 衰减分支。
  - 删除与其仅相关的常量依赖（`ECHO_SUPPRESS_THRESHOLD`、`ECHO_SUPPRESS_MAX_REDUCTION`）在 `lib.rs` 的使用。
- Why：
  - ducking 属于明确要移除的第 5 类处理。

### 4) 移除 processor 处理链（保留重采样与通道映射）

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\processor.rs`
- What：
  - 删除 `processing_config` 字段和构造参数。
  - 删除高通/降噪/AGC/软限幅相关状态字段与计算逻辑。
  - 保留：
    - ringbuf 输入缓存
    - 可选重采样
    - 声道映射（mono/stereo 输出）
- Why：
  - 实现层彻底移除四类麦克风处理，保留基础采集必要逻辑。

### 5) 调整 capture 调用链

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\capture.rs`
- What：
  - `start_stream(...)` 删除 `processing_config` 参数。
  - `AudioProcessor::new(...)` 调用移除对应入参。
- Why：
  - 与 processor 新签名保持一致，避免无效参数透传。

### 6) 文档与类型导出同步清理

- 文件：`e:\Code\HaierGit\perception-audio-recorder\README.md`
- What：
  - 删除 `AudioProcessingConfig` 章节与示例。
  - 删除 `setAudioProcessingConfig/getAudioProcessingConfig` API 列表项。
  - 保留并校正与当前行为一致的其它说明。
- 文件：`e:\Code\HaierGit\perception-audio-recorder\index.d.ts`（构建生成）
- What：
  - 通过构建移除 `AudioProcessingConfig` 类型和 set/get 导出声明。

## Assumptions & Decisions

- 决策 1：本次为“彻底移除”方案，不保留兼容壳接口。
- 决策 2：仅移除上述五类处理链，不触及重采样、分片上报、统计计数等近期新增能力。
- 决策 3：以构建产物 `index.d.ts` 为最终外部 API 真值。

## Verification steps

1. 编译与诊断
   - `cargo check` 通过；
   - `src/global.rs/src/lib.rs/src/capture.rs/src/processor.rs/README.md` 无新增诊断错误。
2. 导出验证
   - `index.d.ts` 中不再出现：
     - `AudioProcessingConfig`
     - `setAudioProcessingConfig`
     - `getAudioProcessingConfig`
3. 行为验证
   - 录制链路可正常启动、暂停、恢复、停止。
   - `micBuffer` 不再经过高通/降噪/AGC/软限幅/ducking 处理（保留采集+重采样+通道映射路径）。
