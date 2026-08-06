class AudioRecordProcessor extends AudioWorkletProcessor {
  constructor() {
    super()
    console.log('AudioRecordProcessor 已创建')

    // 目标字节大小：1128字节
    const targetBytes = 1128
    this.targetElements = Math.floor(targetBytes / 4) // 1128 ÷ 4 = 282个元素
    this.buffer = new Float32Array(this.targetElements)
    this.currentIndex = 0 // 当前填充到的索引位置

    console.log(`目标字节大小: ${targetBytes}字节`)
    console.log(`目标元素个数: ${this.targetElements}个Float32元素`)
    console.log(`实际字节大小: ${this.targetElements * 4}字节`)
  }

  process(inputs, outputs, parameters) {
    // 获取输入音频数据
    const input = inputs[0]

    if (input && input.length > 0) {
      const channelData = input[0] // 获取第一个声道的数据（通常512个Float32元素）

      if (channelData && channelData.length > 0) {
        // 检查剩余空间是否足够
        const remainingSpace = this.targetElements - this.currentIndex

        if (channelData.length <= remainingSpace) {
          // 空间足够，直接复制整个channelData
          this.buffer.set(channelData, this.currentIndex)
          this.currentIndex += channelData.length
        } else {
          // 空间不够，先填满当前缓冲区
          const canFit = remainingSpace
          this.buffer.set(channelData.subarray(0, canFit), this.currentIndex)

          // 发送完整的缓冲区
          this.port.postMessage({
            type: 'audioData',
            data: new Float32Array(this.buffer), // 发送282个元素的副本
          })

          // 重置缓冲区，存储剩余数据
          this.currentIndex = 0
          const remaining = channelData.subarray(canFit)
          if (remaining.length > 0) {
            this.buffer.set(remaining, 0)
            this.currentIndex = remaining.length
          }
        }

        // 检查缓冲区是否恰好满了
        if (this.currentIndex >= this.targetElements) {
          this.port.postMessage({
            type: 'audioData',
            data: new Float32Array(this.buffer),
          })
          this.currentIndex = 0

          console.log(`发送了 ${this.targetElements} 个Float32元素 (1128字节)`)
        }
      }
    }

    // 返回true表示继续处理
    return true
  }
}

// 注册音频处理器
registerProcessor('audio-record-processor', AudioRecordProcessor)
