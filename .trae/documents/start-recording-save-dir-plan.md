# start_recording 录音目录参数改造计划

## Summary

- 目标：为 `start_recording` 增加可选保存目录参数，用于指定当前录音文件输出目录。
- 规则：
  - 若未传入目录或传入为空字符串：使用默认目录（当前登录用户的视频目录）。
  - 若传入目录不存在：使用默认目录。
  - 若默认目录不存在或无权限：回退到当前工作目录。
- 对外效果：JS 侧 `startRecording` 支持可选参数 `saveDir?: string`。

## Current State Analysis

- 当前录音入口在 `src/lib.rs` 的 `start_recording()`，无参数。
- 文件路径当前在 `start_record_impl()` 中以相对文件名创建：
  - `output_yyyy-mm-dd_hh-mm-ss_mmm.wav`
  - 使用 `hound::WavWriter::create(filename, spec)`，默认写到进程当前目录。
- 自动导出类型目前在 `index.d.ts` 中为 `startRecording(): void`（无参数）。
- 示例脚本 `test.js` 目前按无参方式调用 `startRecording()`。
- 当前 `Cargo.toml` 未引入目录解析依赖（如 `dirs`），可用标准库环境变量 + 路径拼接实现。

## Proposed Changes

### 1) `src/lib.rs`

- 调整导出函数签名：
  - `pub fn start_recording(save_dir: Option<String>) -> napi::Result<()>`
  - 保持原有录制状态检查与线程启动逻辑不变。
- 调整内部实现签名：
  - `fn start_record_impl(save_dir: Option<String>) -> AppResult<()>`
- 新增目录解析辅助函数（同文件内）：
  - `resolve_record_save_dir(save_dir: Option<String>) -> PathBuf`
  - 处理顺序：
    1. 若传入目录存在且是目录，直接使用。
    2. 否则使用默认视频目录（Windows: `%USERPROFILE%\\Videos`）。
    3. 若默认目录不可用，回退 `std::env::current_dir()`。
    4. 若 `current_dir` 获取失败，最终回退到 `.`。
- 写盘路径改造：
  - 由 `filename` 改为 `full_path = resolved_dir.join(filename)`。
  - `WavWriter::create(full_path.clone(), spec)`。
  - `set_current_output_file` 保存完整路径字符串（用于现有上下文一致性）。
- 日志增强：
  - 记录最终录音目录与文件全路径，便于排查目录回退行为。

### 2) `index.d.ts`（自动产物）

- 构建后类型应更新为：
  - `startRecording(saveDir?: string | null): void`（以生成结果为准）。
- 其余导出保持不变。

### 3) `test.js`

- 补充一次显式目录调用示例，例如：
  - `startRecording('C:\\Users\\CDS\\Videos')`（或动态目录变量）。
- 保留原有无参调用场景说明（用于覆盖默认目录逻辑）。

### 4) 产物同步

- 执行构建后同步刷新自动生成文件（以构建结果为准）：
  - `index.js`
  - `index.d.ts`
  - `perception-audio-recorder.wasi-browser.js`
  - `perception-audio-recorder.wasi.cjs`

## Assumptions & Decisions

- 输入目录校验使用“存在且为目录”作为有效标准，不自动创建用户传入目录。
- 默认视频目录采用 Windows 用户目录拼接策略：`%USERPROFILE%\\Videos`。
- 当默认目录不可用时，按你的要求回退到当前工作目录，优先保证录制可继续。
- 空字符串、全空白字符串视为“未传入目录”。
- 不改变已有录音文件命名规则（仍为 `output_时间戳.wav`）。

## Verification Steps

1. 构建验证：执行 `npm run build`，确保 Rust 编译通过且导出产物更新成功。
2. 类型验证：检查 `index.d.ts` 中 `startRecording` 参数已变为可选目录参数。
3. 运行验证（无参）：
   - 调用 `startRecording()`，确认录音文件写入默认视频目录；若默认目录不可用则落到当前目录。
4. 运行验证（传参有效）：
   - 调用 `startRecording(validDir)`，确认文件写入指定目录。
5. 运行验证（传参无效）：
   - 调用 `startRecording(invalidDir)`，确认回退默认目录，再次确认默认不可用时回退当前目录。
6. 回归验证：
   - `pause/resume/stop`、进度回调、时长统计行为与现有实现一致。
