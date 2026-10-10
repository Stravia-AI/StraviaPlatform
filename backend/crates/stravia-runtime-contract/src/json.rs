//! 大型不可变 JSON 值的序列化；避免几何扩容留下接近两倍的缓冲区容量。

use std::io::{self, Write};

use serde::Serialize;

#[derive(Default)]
struct ByteCount(usize);

impl Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("JSON size exceeds address space"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 返回紧凑 JSON 的 UTF-8 字节数，不分配完整正文。
pub fn serialized_len<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<usize> {
    let mut count = ByteCount::default();
    serde_json::to_writer(&mut count, value)?;
    Ok(count.0)
}

/// 将不可变值编码为紧凑 JSON，按计数结果预分配正文。
///
/// 值会序列化两次：首次只计数，第二次写入。调用方必须提供两次序列化
/// 一致的快照；不得传入依赖时钟、随机数或可变外部状态的序列化实现。
/// 任一次序列化失败均返回原始错误。
pub fn to_vec<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(serialized_len(value)?);
    serde_json::to_writer(&mut bytes, value)?;
    Ok(bytes)
}
