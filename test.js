const { doInitialize, startRecord, stopRecord } = require('./index.js')

doInitialize(
    (err, info) => {
        if (err) {
            console.error('初始化失败:', err);
            return;
        }
        console.log('初始化成功:', info);
    },
    (err, log) => {
        if (err) {
            console.error('日志记录失败:', err);
            return;
        }
        console.log('日志记录成功:', log);
    }
);

startRecord();

setTimeout(() => {
    console.log("Stopping...");
    stopRecord(); // 这会修改 Rust 中的标志位
}, 1000 * 60);

setInterval(() => {
    console.log("一分钟过去了");
}, 1000 * 60);