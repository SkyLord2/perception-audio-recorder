const unsubscribe = callBridgeListener('h5Api.listenRecordingProgress', (progress: any) => {
  // console.log("start listenRecordingProgress----------", progress);

  const nowTs = Date.now()
  const intervalMs = lastTickTs ? nowTs - lastTickTs : 0
  lastTickTs = nowTs

  // console.log("listenRecordingProgress interval(ms):", intervalMs, "seq:", sequenceNumber);

  // 当未处于录音状态时，直接跳过处理
  if (!getIsRecording() || getIsPaused()) {
    return
  }

  dispatch(updateAudioProgress(progress))

  if (progress.buffer) {
    try {
      const pcmBytes = convertToPCM16Bytes(progress.buffer, {
        inputIsFloat32: progress.buffer.byteLength % 4 === 0,
        targetSampleRate: 16000,
      })

      byteQueue.push(pcmBytes)

      const PAYLOAD_SIZE = 4096
      let payload: Uint8Array | null = byteQueue.consume(PAYLOAD_SIZE)

      while (payload) {
        // 直接发送音频数据，不添加头部
        const currentWs = resourceManager?.getWebSocket()
        if (currentWs && currentWs.readyState === WebSocket.OPEN) {
          const base64Data = btoa(String.fromCharCode(...payload))
          let audioData: any = {
            audio: base64Data,
          }
          if (recordingStatus == 0) {
            if (sequenceNumber == 0) {
              audioData.status = '0'
            } else if (sequenceNumber == 1) {
              audioData.status = '1'
            }
          }
          currentWs.send(JSON.stringify(audioData))
        }

        sequenceNumber++
        dispatch(updateSequenceNumber(sequenceNumber))
        payload = byteQueue.consume(PAYLOAD_SIZE)
      }

      // 将剩余未消费的PCM数据保存到lastByteQueue
      if (byteQueue.length > 0) {
        // 复制剩余数据，不清空队列
        const remainingData = new Uint8Array(byteQueue.length)
        let offset = 0

        // 遍历所有chunks并复制数据
        for (const chunk of byteQueue['chunks']) {
          remainingData.set(chunk, offset)
          offset += chunk.length
        }

        if (remainingData.length > 0) {
          dispatch(updateLastByteQueue(remainingData))
        }
      }
    } catch (error) {
      console.error('音频数据处理失败:', error)
    }
  }

  // 前端不再计算最大录音时长，改为后端通过 WebSocket 返回 503 状态码时处理
})
