# `test.js` 接入讯飞实时转写计划

## Summary

- 目标：参考 `examples/rtasr_llm_demo/rtasr_llm_demo.py` 的鉴权与实时发送逻辑，在 `test.js` 中把 `listenRecordingProgress` 上报的 `micBuffer` 实时转成讯飞要求的 `pcm_s16le / 16k / 40ms / 1280字节` 音频帧，通过 WebSocket 实时发送给讯飞，并在控制台打印实时识别结果。
- 成功标准：
  - `test.js` 启动录制后能建立讯飞 WebSocket 连接。
  - 只使用 `micBuffer` 作为实时识别音源，不使用 `buffer` 或 `spkBuffer`。
  - 按 40ms 节奏持续发送 1280 字节音频帧。
  - 在控制台输出讯飞返回的中间结果和最终结果。
  - 停止录制后正确发送结束标记并安全关闭连接。
- 已确认偏好：
  - Node 侧 WebSocket 客户端：新增 `ws` 依赖。
  - 密钥接入方式：直接写入 `test.js`。
  - 结果输出方式：控制台打印。

## Current State Analysis

- `test.js` 已能通过 `listenRecordingProgress` 实时拿到 `progress.micBuffer`，其类型是 `Float32Array`，见 `test.js` 与 `index.d.ts`。
- `test.js` 当前主要做两件事：
  - 将 `micBuffer` / `spkBuffer` 保存为本地 Float32 WAV；
  - 打印进度日志。
- `examples/rtasr_llm_demo/rtasr_llm_demo.py` 已提供与讯飞实时转写服务对接所需的关键协议细节：
  - 固定参数 `audio_encode=pcm_s16le`、`lang=autodialect`、`samplerate=16000`
  - 鉴权参数拼接与 HMAC-SHA1 + Base64 签名
  - WebSocket URL `wss://office-api-ast-dx.iflyaisol.com/ast/communicate/v1`
  - 每 40ms 发送 1280 字节二进制音频帧
  - 结束时发送 `{"end": true}`，有 `sessionId` 时附带回传
- `src/global.rs` 当前默认 `startRecording()` 参数是：
  - `record_device` 默认 `0`，即 `MicOnly`
  - `sample_rate` 默认 `16000`
  - `channels` 默认 `mono`
  - 因而 `test.js` 现有 `startRecording(saveDir)` 已与讯飞 demo 的 16k 单声道前提基本一致。
- `package.json` 当前没有 WebSocket 客户端依赖，因此若在 `test.js` 中直接接入讯飞，需要新增 `ws`。

## Proposed Changes

### 1. `package.json`

- 新增运行时依赖 `ws`。
- 原因：
  - 当前仓库没有可复用的 Node WebSocket 客户端依赖。
  - 目标环境 `engines.node >= 10`，不能假设全局存在稳定可用的内置 `WebSocket`。
  - `ws` 能稳定处理：
    - 二进制音频帧发送
    - 文本消息接收
    - `open / message / error / close` 生命周期管理

### 2. `test.js`

#### 2.1 接入讯飞鉴权与连接管理

- 在文件顶部新增：
  - `crypto`、`url` / `URLSearchParams`、`ws` 相关引入
  - 讯飞固定参数常量：
    - `APP_ID = "6dfb13c5"`
    - `ACCESS_KEY_ID = "08ba9014e134f005d48a5b6b2541859f"`
    - `ACCESS_KEY_SECRET = "ZDE3ZTBkZjYyYTRjODA3ZjVhOWViNzhk"`
    - `audio_encode = "pcm_s16le"`
    - `lang = "autodialect"`
    - `samplerate = "16000"`
  - 实时发送常量：
    - `FRAME_INTERVAL_MS = 40`
    - `AUDIO_FRAME_SIZE = 1280`
- 参考 Python demo 实现以下辅助函数：
  - 生成 UTC+8 时间字符串
  - 生成随机 `uuid`
  - 过滤空值、按字典序排序参数
  - 对 key/value 做 URL 编码
  - 用 `ACCESS_KEY_SECRET` 对拼接字符串做 HMAC-SHA1，再 Base64 得到 `signature`
  - 拼接完整 WebSocket URL

#### 2.2 只使用 `micBuffer` 作为 ASR 输入

- `listenRecordingProgress` 中不再把 `progress.buffer` 作为识别输入。
- 仅消费 `progress.micBuffer`：
  - 因为它是经过 3A 处理后的真实麦克风数据；
  - 避免把扬声器或混合链路音频送给讯飞，降低识别噪声。
- 保留现有本地 `micBuffer` WAV 落盘逻辑，用于联调对比。

#### 2.3 将 `Float32Array micBuffer` 实时转换为 `pcm_s16le`

- 新增 `float32ToPcm16Bytes()` 一类的函数：
  - 输入：`Float32Array`
  - 处理：
    - 逐样本裁剪到 `[-1.0, 1.0]`
    - 映射到 `int16`
    - 小端写入 `Uint8Array` / `Buffer`
  - 输出：PCM16LE 字节流
- 不额外做重采样与重声道处理：
  - 因为当前 `startRecording(saveDir)` 默认即为 `MicOnly + 16000 + mono`
  - 与讯飞 demo 的固定协议一致。
- 为避免未来误用，可在 `test.js` 中显式将启动参数改为：
  - `startRecording(saveDir, 0, 16000, 'mono')`
  - 从而把协议前提写死在联调脚本中，减少依赖默认值。

#### 2.4 增加发送队列与 40ms 定时发送

- 新增一个音频字节缓存队列，用于积累由 `micBuffer` 转出的 PCM16 数据。
- 在 `listenRecordingProgress` 回调里：
  - 每次将新产生的 PCM16 字节追加进队列；
  - 不直接按回调节奏发送，避免把“回调时间”误当成“协议发送时间”。
- 新增一个独立的 40ms 定时发送循环，职责如下：
  - 仅在 WebSocket 已连接且录制仍在进行时运行；
  - 每次从队列取出固定 `1280` 字节；
  - 若不足一帧则等待下个 tick，不发送半帧；
  - 每次发送一帧二进制消息；
  - 记录发送帧号与时间戳，方便调试节奏是否稳定。
- 这样实现的原因：
  - `listenRecordingProgress` 的目标频率是 40ms，但真实回调间隔存在抖动；
  - 讯飞 demo 强依赖稳定的 40ms / 1280 字节发送节奏；
  - 因此接收与发送要解耦。

#### 2.5 增加 WebSocket 生命周期与识别结果打印

- 建立 `ws` 连接后：
  - 等待 `open` 事件
  - 再允许发送线程开始出队
- 在 `message` 事件里：
  - 将文本消息 JSON 解析后打印
  - 与 Python demo 一样，若是 `msg_type=action` 且包含 `sessionId`，则保存该 `sessionId`
  - 对非 JSON 文本和二进制消息做容错日志
- 在 `error` / `close` 事件里：
  - 停止发送循环
  - 标记连接状态
  - 打印错误与关闭原因
- 在控制台输出建议区分：
  - 连接状态日志
  - 音频发送节奏日志
  - 讯飞中间识别结果
  - 讯飞最终识别结果

#### 2.6 停止录制时发送结束标记

- 在 `stopRecording()` 触发后的收尾逻辑中：
  - 停止新的 PCM 追加发送
  - 允许发送循环先把队列里完整帧发完，或按实现约定立即结束
  - 向讯飞发送：
    - `{"end": true}`
    - 若已拿到 `sessionId`，则附带 `sessionId`
- 然后再关闭 WebSocket。
- 目的：
  - 让讯飞正常收尾并返回最终识别结果
  - 避免只断连接不发结束帧，导致结果不完整

### 3. `examples/rtasr_llm_demo/rtasr_llm_demo.py`

- 该文件本次不修改，仅作为协议参考实现。
- 在计划与代码注释中保留其参考关系，便于后续对照：
  - 鉴权参数生成
  - 发送节奏
  - 结束标记格式
  - `sessionId` 回传逻辑

## Assumptions & Decisions

- 决策：本次实现仅针对 `test.js` 本地联调脚本，不把讯飞实时转写能力抽成通用 SDK。
- 决策：本次识别输入固定使用 `micBuffer`，不使用 `buffer` 和 `spkBuffer`。
- 决策：本次在 `test.js` 中显式使用 `startRecording(saveDir, 0, 16000, 'mono')`，避免协议与默认参数耦合。
- 决策：密钥按你的要求直接写入 `test.js`，不做环境变量封装。
- 决策：识别结果只打印到控制台，不做文本落盘。
- 决策：Node 侧新增 `ws` 依赖，不依赖运行时是否自带全局 `WebSocket`。
- 假设：讯飞服务端对 Node 实现的签名拼接规则与 Python demo 完全一致。
- 假设：`listenRecordingProgress` 回调中 `micBuffer` 为单声道 `Float32Array`，符合当前录制脚本设定。

## Verification Steps

- 依赖验证：
  - 安装新增的 `ws` 后，`test.js` 能正常启动。
- 协议验证：
  - 启动时打印完整鉴权 URL 的脱敏信息或关键字段确认。
  - 成功建立 WebSocket 连接并收到服务端首个响应。
- 音频验证：
  - `listenRecordingProgress` 中 `micBuffer.length` 持续大于 0。
  - PCM 队列能稳定产出每帧 `1280` 字节数据。
  - 发送节奏接近每 `40ms` 一帧。
- 识别验证：
  - 控制台能看到讯飞返回的中间识别结果。
  - 停止录制后能收到最终识别结果或明确结束响应。
- 兼容验证：
  - 现有本地 `micBuffer` / `spkBuffer` WAV 保存功能不被破坏。
  - 若 WebSocket 连接失败或中途关闭，脚本能打印错误并安全收尾，不影响录音文件关闭。
