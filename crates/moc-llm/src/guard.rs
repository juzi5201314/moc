//! 默认启用的流式循环检测；所有窗口都有固定上界，不保存整份生成历史。

use serde::{Deserialize, Serialize};

use crate::StreamEvent;
mod prose;
use prose::Prose;

/// 本地流保护配置，不发送给供应商。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StreamGuardConfig {
    /// 是否识别思考、正文与工具参数里的重复内容。
    pub loop_detection: bool,
}

impl Default for StreamGuardConfig {
    fn default() -> Self {
        Self {
            loop_detection: true,
        }
    }
}

/// 重复内容检测命中；提示落地由调用方决定，传输层不得按失败重试。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("检测到{channel}循环：{detail}")]
pub struct LoopViolation {
    /// 命中的通道：正文、思考或工具参数。
    pub channel: &'static str,
    /// 命中描述，用于终止通知。
    pub detail: String,
}

const TAIL: usize = 4096;
const MAX_PERIOD: usize = 1024;
const STRIDE: usize = 128;
const SEGMENT_CHARS: usize = 768;
const SEGMENT_WINDOW: usize = 12;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Channel {
    Text,
    Reasoning,
    Arguments,
}

impl Channel {
    fn label(self) -> &'static str {
        match self {
            Self::Text => "正文",
            Self::Reasoning => "思考",
            Self::Arguments => "工具参数",
        }
    }
}

pub(crate) struct StreamGuard {
    pub(crate) config: StreamGuardConfig,
    blocks: Vec<(Channel, usize, Detector)>,
}

impl StreamGuard {
    pub(crate) fn new(config: StreamGuardConfig) -> Self {
        Self {
            config,
            blocks: Vec::new(),
        }
    }

    pub(crate) fn observe(&mut self, event: &StreamEvent) -> Result<(), LoopViolation> {
        let (channel, index, text) = match event {
            StreamEvent::TextDelta { index, text } => (Channel::Text, *index, text.as_str()),
            StreamEvent::ReasoningDelta { index, text } => {
                (Channel::Reasoning, *index, text.as_str())
            }
            StreamEvent::ToolCallArgumentsDelta { index, json } => {
                (Channel::Arguments, *index, json.as_str())
            }
            // 以下事件没有可检测文本，只保证对应块已经建立。
            StreamEvent::ReasoningSignature { index, .. } => {
                return self.observe_delta(Channel::Reasoning, *index, "");
            }
            StreamEvent::ToolCallStart { index, .. } => {
                return self.observe_delta(Channel::Arguments, *index, "");
            }
            StreamEvent::ReasoningItem { index, .. } => {
                return self.observe_delta(Channel::Reasoning, *index, "");
            }
            StreamEvent::Done { .. } => {
                if self.config.loop_detection {
                    for (channel, _, detector) in &mut self.blocks {
                        if let Some(detail) = detector.flush() {
                            return Err(LoopViolation {
                                channel: channel.label(),
                                detail,
                            });
                        }
                    }
                }
                return Ok(());
            }
            // 元事件不参与文本相似度检测。
            _ => return Ok(()),
        };
        self.observe_delta(channel, index, text)
    }

    fn observe_delta(
        &mut self,
        channel: Channel,
        index: usize,
        text: &str,
    ) -> Result<(), LoopViolation> {
        let slot = match self
            .blocks
            .iter()
            .position(|(kind, block, _)| *kind == channel && *block == index)
        {
            Some(slot) => slot,
            None => {
                self.blocks.push((channel, index, Detector::new(channel)));
                self.blocks.len() - 1
            }
        };
        if self.config.loop_detection
            && let Some(detail) = self.blocks[slot].2.push(text)
        {
            return Err(LoopViolation {
                channel: channel.label(),
                detail,
            });
        }
        Ok(())
    }

    pub(crate) fn check_response(
        &mut self,
        response: &crate::ChatResponse,
    ) -> Result<(), LoopViolation> {
        for (index, part) in response.message.content.iter().enumerate() {
            match part {
                crate::ContentPart::Text(text) => self.observe_delta(Channel::Text, index, text)?,
                crate::ContentPart::Reasoning(crate::ReasoningBlock::Text { text, .. }) => {
                    self.observe_delta(Channel::Reasoning, index, text)?
                }
                crate::ContentPart::ToolCall(call) => {
                    self.observe_delta(Channel::Arguments, index, &call.arguments)?
                }
                _ => {}
            }
        }
        self.observe(&StreamEvent::Done {
            finish_reason: response.finish_reason.clone(),
        })
    }
}

struct Detector {
    channel: Channel,
    exact: ExactCycle,
    prose: Prose,
    json: JsonScope,
}

impl Detector {
    fn new(channel: Channel) -> Self {
        Self {
            channel,
            exact: ExactCycle::default(),
            prose: Prose::new(channel == Channel::Reasoning),
            json: JsonScope::default(),
        }
    }

    fn push(&mut self, delta: &str) -> Option<String> {
        for ch in delta.chars() {
            self.exact.push(ch);
            if self.channel == Channel::Arguments {
                self.json.push(ch);
                // 合法 JSON 的数组元素和字符串（包括待写代码）可以任意重复。
                // 只把多个顶层值拼接成的周期当成参数循环。
                if self.json.extra_root
                    && self.exact.ready()
                    && let Some(hit) = self.exact.detect(false)
                {
                    return Some(hit);
                }
            } else {
                if !self.prose.fenced
                    && self.exact.ready()
                    && let Some(hit) = self.exact.detect(true)
                {
                    return Some(hit);
                }
                if let Some(hit) = self.prose.push(ch) {
                    return Some(hit);
                }
            }
        }
        None
    }

    fn flush(&mut self) -> Option<String> {
        if self.channel == Channel::Arguments {
            return self
                .json
                .extra_root
                .then(|| self.exact.detect(false))
                .flatten();
        }
        if !self.prose.fenced
            && let Some(hit) = self.exact.detect(true)
        {
            return Some(hit);
        }
        self.prose.flush()
    }
}

/// Unicode 字符环；长度至多 4096，Z 数组至多 1025，按固定步长扫描。
#[derive(Default)]
struct ExactCycle {
    tail: Vec<char>,
    next: usize,
    cadence: usize,
    z: Vec<u16>,
}

impl ExactCycle {
    fn push(&mut self, ch: char) {
        if self.tail.is_empty() {
            self.tail.reserve_exact(TAIL);
        }
        if self.tail.len() < TAIL {
            self.tail.push(ch);
        } else {
            self.tail[self.next] = ch;
        }
        self.next = (self.next + 1) % TAIL;
        self.cadence = (self.cadence + 1) % STRIDE;
    }

    fn ready(&self) -> bool {
        self.cadence == 0
    }

    fn reversed(&self, index: usize) -> char {
        self.tail[(self.next + TAIL - 1 - index) % TAIL]
    }

    fn detect(&mut self, prose: bool) -> Option<String> {
        let n = self.tail.len();
        if n < 512 {
            return None;
        }
        debug_assert!(n <= TAIL);
        let max_period = MAX_PERIOD.min(n / 4);
        self.z.resize(max_period + 1, 0);
        let (mut left, mut right) = (0, 0);
        for i in 1..=max_period {
            let mut len = if i <= right {
                (right - i + 1).min(usize::from(self.z[i - left]))
            } else {
                0
            };
            while i + len < n && self.reversed(len) == self.reversed(i + len) {
                len += 1;
            }
            self.z[i] = len as u16;
            if i + len > right + 1 {
                left = i;
                right = i + len - 1;
            }
        }
        let mut first_letter = None;
        let mut different_letters = false;
        let mut syntax = false;
        // 周期前缀特征一次累计；数字／代码的多周期候选也只需 O(窗口长度)。
        for period in 1..=max_period {
            let ch = self.reversed(period - 1);
            syntax |= "{}[]();=<>`\"".contains(ch);
            if ch.is_alphabetic() {
                if let Some(first) = first_letter {
                    different_letters |= first != ch;
                } else {
                    first_letter = Some(ch);
                }
            }
            if period < 2 || first_letter.is_none() || (prose && (syntax || !different_letters)) {
                continue;
            }
            let count = 1 + usize::from(self.z[period]) / period;
            let (repeats, chars) = if period <= 64 { (8, 512) } else { (4, 2048) };
            if count < repeats || period * count < chars {
                continue;
            }
            return Some(format!("同一段 {period} 字符内容连续重复 {count} 次"));
        }
        None
    }
}

#[derive(Default)]
struct JsonScope {
    depth: usize,
    string: bool,
    escaped: bool,
    started: bool,
    completed: bool,
    extra_root: bool,
}

impl JsonScope {
    fn push(&mut self, ch: char) {
        if self.completed && !ch.is_whitespace() {
            self.extra_root = true;
        }
        if self.string {
            if self.escaped {
                self.escaped = false;
            } else if ch == '\\' {
                self.escaped = true;
            } else if ch == '"' {
                self.string = false;
            }
            return;
        }
        match ch {
            '"' => self.string = true,
            '{' | '[' => {
                self.started = true;
                self.depth += 1;
            }
            '}' | ']' if self.depth > 0 => {
                self.depth -= 1;
                if self.depth == 0 && self.started {
                    self.completed = true;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_unicode_cycles_independently_of_delta_boundaries() {
        let text = "我需要重新检查结果，再次确认是否可以继续执行。".repeat(100);
        for chunk in [1, 7, 128, 4096] {
            let mut detector = Detector::new(Channel::Reasoning);
            let chars: Vec<_> = text.chars().collect();
            let hit = chars
                .chunks(chunk)
                .find_map(|chars| detector.push(&chars.iter().collect::<String>()));
            assert!(hit.is_some(), "分片大小 {chunk}");
        }
    }

    #[test]
    fn normal_code_numeric_padding_and_json_arrays_are_not_loops() {
        for text in [
            "let value = compute();\n".repeat(200),
            "0000 1234\n".repeat(200),
            "a".repeat(4096),
        ] {
            let mut detector = Detector::new(Channel::Text);
            assert_eq!(detector.push(&text), None);
            assert_eq!(detector.flush(), None);
        }
        let text = serde_json::json!({"content": "fn main() {}\n".repeat(200), "values": vec!["same"; 300]}).to_string();
        let mut detector = Detector::new(Channel::Arguments);
        assert_eq!(detector.push(&text), None);
        assert_eq!(detector.flush(), None);
        let mut broken = Detector::new(Channel::Arguments);
        assert!(broken.push(&"{\"path\":\"a.txt\"}".repeat(200)).is_some());
    }

    #[test]
    fn semantic_rewording_detects_chinese_but_spares_new_targets() {
        let paragraph = "仍然需要继续仔细检查现有情况并确认当前工作是否已经完成，在没有得到新的证据之前应当继续保持注意力然后再次检查所有已有结果以确保当前判断可靠，并继续确认接下来应该执行的动作是否正确。这些检查仍然需要围绕现有情况继续进行并保持当前方向，随后再次确认现有结果。";
        let mut detector = Detector::new(Channel::Reasoning);
        let mut hit = None;
        for i in 0..20 {
            hit = detector.push(&format!("第{i}轮\n\n{paragraph}\n\n"));
            if hit.is_some() {
                break;
            }
        }
        assert!(hit.is_some());
        let mut progressing = Detector::new(Channel::Reasoning);
        for i in 0..20 {
            assert_eq!(
                progressing.push(&format!("检查 `src/file_{i}.rs` 的结果。{paragraph}\n\n")),
                None
            );
        }
    }

    #[test]
    fn channels_blocks_and_disabled_detection_do_not_cross_contaminate() {
        let mut guard = StreamGuard::new(StreamGuardConfig::default());
        for index in 0..4 {
            for _ in 0..3 {
                guard
                    .observe(&StreamEvent::ReasoningDelta {
                        index,
                        text: "检查已有结果并确认当前判断可靠。".into(),
                    })
                    .unwrap();
                guard
                    .observe(&StreamEvent::TextDelta {
                        index,
                        text: "检查已有结果并确认当前判断可靠。".into(),
                    })
                    .unwrap();
            }
        }
        let repeated = "反复检查现有情况，然后再次确认当前结果。".repeat(150);
        let mut disabled = StreamGuard::new(StreamGuardConfig {
            loop_detection: false,
        });
        assert!(
            disabled
                .observe(&StreamEvent::TextDelta {
                    index: 0,
                    text: repeated,
                })
                .is_ok()
        );
    }
}
