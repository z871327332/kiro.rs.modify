//! AWS Event Stream 消息帧解析
//!
//! ## 消息格式
//!
//! ```text
//! [4 bytes: total_length]
//! [4 bytes: header_length]
//! [4 bytes: prelude_crc32]      <-- Prelude (12 bytes)
//! [headers: header_length bytes]
//! [payload: variable]
//! [4 bytes: message_crc32]
//! ```
//!
//! ## 状态机设计
//!
//! 参考 kiro-kt 的设计，采用两态模型：
//!
//! ```text
//! ┌───────────────────┐      数据足够 (>= 12 bytes)
//! │  AwaitingPrelude  │ ─────────────────────────────┐
//! └───────────────────┘                              │
//!          ↑                                         ↓
//!          │                              ┌───────────────────┐
//!          │                              │   AwaitingData    │
//!          │                              └───────────────────┘
//!          │                                         │
//!          │      解析完成/失败                       │
//!          └─────────────────────────────────────────┘
//! ```

use super::crc::crc32;
use super::error::{ParseError, ParseResult};
use super::header::{parse_headers, Headers};

/// Prelude 固定大小 (12 字节)
pub const PRELUDE_SIZE: usize = 12;

/// 最小消息大小 (Prelude + Message CRC)
pub const MIN_MESSAGE_SIZE: usize = PRELUDE_SIZE + 4;

/// 最大消息大小限制 (16 MB)
pub const MAX_MESSAGE_SIZE: u32 = 16 * 1024 * 1024;

/// 帧解析状态
///
/// 参考 kiro-kt 的 FrameParser 状态设计
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameParserState {
    /// 等待 Prelude (12 bytes)
    AwaitingPrelude,
    /// 等待完整消息数据
    AwaitingData {
        /// 消息总长度
        total_length: usize,
        /// 头部长度
        header_length: usize,
    },
}

impl Default for FrameParserState {
    fn default() -> Self {
        Self::AwaitingPrelude
    }
}

/// 解析后的消息帧
#[derive(Debug, Clone)]
pub struct Frame {
    /// 消息头部
    pub headers: Headers,
    /// 消息负载
    pub payload: Vec<u8>,
}

impl Frame {
    /// 获取消息类型
    pub fn message_type(&self) -> Option<&str> {
        self.headers.message_type()
    }

    /// 获取事件类型
    pub fn event_type(&self) -> Option<&str> {
        self.headers.event_type()
    }

    /// 将 payload 解析为 JSON
    pub fn payload_as_json<T: serde::de::DeserializeOwned>(&self) -> ParseResult<T> {
        serde_json::from_slice(&self.payload).map_err(ParseError::PayloadDeserialize)
    }

    /// 将 payload 解析为字符串
    pub fn payload_as_str(&self) -> String {
        String::from_utf8_lossy(&self.payload).to_string()
    }
}

/// 消息帧解析器
///
/// 采用状态机设计，支持流式解析
pub struct FrameParser {
    /// 当前状态
    state: FrameParserState,
}

impl Default for FrameParser {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameParser {
    /// 创建新的帧解析器
    pub fn new() -> Self {
        Self {
            state: FrameParserState::AwaitingPrelude,
        }
    }

    /// 获取当前状态
    pub fn state(&self) -> FrameParserState {
        self.state
    }

    /// 重置解析器状态
    pub fn reset(&mut self) {
        self.state = FrameParserState::AwaitingPrelude;
    }

    /// 尝试从缓冲区解析一个完整的帧（有状态版本）
    ///
    /// # Arguments
    /// * `buffer` - 输入缓冲区
    ///
    /// # Returns
    /// - `Ok(Some((frame, consumed)))` - 成功解析，返回帧和消费的字节数
    /// - `Ok(None)` - 数据不足，需要更多数据
    /// - `Err(e)` - 解析错误
    pub fn parse_stateful(&mut self, buffer: &[u8]) -> ParseResult<Option<(Frame, usize)>> {
        loop {
            match self.state {
                FrameParserState::AwaitingPrelude => {
                    // 检查是否有足够的数据读取 prelude
                    if buffer.len() < PRELUDE_SIZE {
                        return Ok(None);
                    }

                    // 读取 prelude
                    let total_length =
                        u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
                    let header_length =
                        u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]);
                    let prelude_crc =
                        u32::from_be_bytes([buffer[8], buffer[9], buffer[10], buffer[11]]);

                    // 验证消息长度范围
                    if total_length < MIN_MESSAGE_SIZE as u32 {
                        self.reset();
                        return Err(ParseError::MessageTooSmall {
                            length: total_length,
                            min: MIN_MESSAGE_SIZE as u32,
                        });
                    }

                    if total_length > MAX_MESSAGE_SIZE {
                        self.reset();
                        return Err(ParseError::MessageTooLarge {
                            length: total_length,
                            max: MAX_MESSAGE_SIZE,
                        });
                    }

                    // 验证 Prelude CRC
                    let actual_prelude_crc = crc32(&buffer[..8]);
                    if actual_prelude_crc != prelude_crc {
                        self.reset();
                        return Err(ParseError::PreludeCrcMismatch {
                            expected: prelude_crc,
                            actual: actual_prelude_crc,
                        });
                    }

                    // 转移到 AwaitingData 状态
                    self.state = FrameParserState::AwaitingData {
                        total_length: total_length as usize,
                        header_length: header_length as usize,
                    };
                }

                FrameParserState::AwaitingData {
                    total_length,
                    header_length,
                } => {
                    // 检查是否有完整的消息
                    if buffer.len() < total_length {
                        return Ok(None);
                    }

                    // 读取 Message CRC
                    let message_crc = u32::from_be_bytes([
                        buffer[total_length - 4],
                        buffer[total_length - 3],
                        buffer[total_length - 2],
                        buffer[total_length - 1],
                    ]);

                    // 验证 Message CRC
                    let actual_message_crc = crc32(&buffer[..total_length - 4]);
                    if actual_message_crc != message_crc {
                        self.reset();
                        return Err(ParseError::MessageCrcMismatch {
                            expected: message_crc,
                            actual: actual_message_crc,
                        });
                    }

                    // 解析头部
                    let headers_start = PRELUDE_SIZE;
                    let headers_end = headers_start + header_length;

                    // 验证头部边界
                    if headers_end > total_length - 4 {
                        self.reset();
                        return Err(ParseError::HeaderParseFailed(
                            "头部长度超出消息边界".to_string(),
                        ));
                    }

                    let headers =
                        parse_headers(&buffer[headers_start..headers_end], header_length)?;

                    // 提取 payload
                    let payload_start = headers_end;
                    let payload_end = total_length - 4;
                    let payload = buffer[payload_start..payload_end].to_vec();

                    // 重置状态，准备解析下一帧
                    self.reset();

                    return Ok(Some((Frame { headers, payload }, total_length)));
                }
            }
        }
    }

    /// 尝试从缓冲区解析一个完整的帧（无状态静态版本）
    ///
    /// 保留原有的静态方法以保持向后兼容
    ///
    /// # Arguments
    /// * `buffer` - 输入缓冲区
    ///
    /// # Returns
    /// - `Ok(Some((frame, consumed)))` - 成功解析，返回帧和消费的字节数
    /// - `Ok(None)` - 数据不足，需要更多数据
    /// - `Err(e)` - 解析错误
    pub fn parse(buffer: &[u8]) -> ParseResult<Option<(Frame, usize)>> {
        // 检查是否有足够的数据读取 prelude
        if buffer.len() < PRELUDE_SIZE {
            return Ok(None);
        }

        // 读取 prelude
        let total_length = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
        let header_length = u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]);
        let prelude_crc = u32::from_be_bytes([buffer[8], buffer[9], buffer[10], buffer[11]]);

        // 验证消息长度范围
        if total_length < MIN_MESSAGE_SIZE as u32 {
            return Err(ParseError::MessageTooSmall {
                length: total_length,
                min: MIN_MESSAGE_SIZE as u32,
            });
        }

        if total_length > MAX_MESSAGE_SIZE {
            return Err(ParseError::MessageTooLarge {
                length: total_length,
                max: MAX_MESSAGE_SIZE,
            });
        }

        let total_length = total_length as usize;
        let header_length = header_length as usize;

        // 检查是否有完整的消息
        if buffer.len() < total_length {
            return Ok(None);
        }

        // 验证 Prelude CRC
        let actual_prelude_crc = crc32(&buffer[..8]);
        if actual_prelude_crc != prelude_crc {
            return Err(ParseError::PreludeCrcMismatch {
                expected: prelude_crc,
                actual: actual_prelude_crc,
            });
        }

        // 读取 Message CRC
        let message_crc = u32::from_be_bytes([
            buffer[total_length - 4],
            buffer[total_length - 3],
            buffer[total_length - 2],
            buffer[total_length - 1],
        ]);

        // 验证 Message CRC (对整个消息不含最后4字节)
        let actual_message_crc = crc32(&buffer[..total_length - 4]);
        if actual_message_crc != message_crc {
            return Err(ParseError::MessageCrcMismatch {
                expected: message_crc,
                actual: actual_message_crc,
            });
        }

        // 解析头部
        let headers_start = PRELUDE_SIZE;
        let headers_end = headers_start + header_length;

        // 验证头部边界
        if headers_end > total_length - 4 {
            return Err(ParseError::HeaderParseFailed(
                "头部长度超出消息边界".to_string(),
            ));
        }

        let headers = parse_headers(&buffer[headers_start..headers_end], header_length)?;

        // 提取 payload (去除最后4字节的 message_crc)
        let payload_start = headers_end;
        let payload_end = total_length - 4;
        let payload = buffer[payload_start..payload_end].to_vec();

        Ok(Some((Frame { headers, payload }, total_length)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_parser_new() {
        let parser = FrameParser::new();
        assert_eq!(parser.state(), FrameParserState::AwaitingPrelude);
    }

    #[test]
    fn test_frame_parser_reset() {
        let mut parser = FrameParser::new();
        parser.state = FrameParserState::AwaitingData {
            total_length: 100,
            header_length: 10,
        };

        parser.reset();
        assert_eq!(parser.state(), FrameParserState::AwaitingPrelude);
    }

    #[test]
    fn test_frame_insufficient_data() {
        let buffer = [0u8; 10]; // 小于 PRELUDE_SIZE
        assert!(matches!(FrameParser::parse(&buffer), Ok(None)));
    }

    #[test]
    fn test_frame_message_too_small() {
        // 构造一个 total_length = 10 的 prelude (小于最小值)
        let mut buffer = vec![0u8; 16];
        buffer[0..4].copy_from_slice(&10u32.to_be_bytes()); // total_length
        buffer[4..8].copy_from_slice(&0u32.to_be_bytes()); // header_length
        let prelude_crc = crc32(&buffer[0..8]);
        buffer[8..12].copy_from_slice(&prelude_crc.to_be_bytes());

        let result = FrameParser::parse(&buffer);
        assert!(matches!(result, Err(ParseError::MessageTooSmall { .. })));
    }

    #[test]
    fn test_frame_parser_state_default() {
        let state = FrameParserState::default();
        assert_eq!(state, FrameParserState::AwaitingPrelude);
    }

    #[test]
    fn test_frame_parser_stateful_insufficient_data() {
        let mut parser = FrameParser::new();
        let buffer = [0u8; 10];

        let result = parser.parse_stateful(&buffer);
        assert!(matches!(result, Ok(None)));
        assert_eq!(parser.state(), FrameParserState::AwaitingPrelude);
    }
}
