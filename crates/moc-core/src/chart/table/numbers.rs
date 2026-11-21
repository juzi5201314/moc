use super::Dimension;
use super::Number;
use super::SLASH_LIST_MAX;
use super::SLASH_LIST_MIN;
use super::Scanner;
use super::UnitHit;
use super::units::is_alnum;
use super::units::rate_unit;
use super::units::unit_token;

/// 按分隔符切成 2–3 段数字；段数不符或出现非数字段时返回 `None`。
pub(super) fn split_numeric(text: &str, sep: char) -> Option<(&str, &str, Option<&str>)> {
    let mut parts = text.split(sep);
    let first = parts.next()?;
    let second = parts.next()?;
    let third = parts.next();
    if parts.next().is_some() {
        return None;
    }
    if !is_digits(first) || !is_digits(second) {
        return None;
    }
    if let Some(third) = third
        && !is_digits(third)
    {
        return None;
    }
    Some((first, second, third))
}

/// 是否非空的纯 ASCII 数字串。
pub(super) fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// 数字串的数值；用于范围判断，溢出时按最大值处理（日期/时间的段不会溢出）。
pub(super) fn count(text: &str) -> u32 {
    text.parse().unwrap_or(u32::MAX)
}

impl<'a> Scanner<'a> {
    /// 剩余文本。
    pub(super) fn rest(&self) -> &'a str {
        &self.text[self.pos..]
    }

    /// 跳过空白。
    pub(super) fn skip_spaces(&mut self) {
        while let Some(ch) = self.rest().chars().next() {
            if ch.is_whitespace() {
                self.pos += ch.len_utf8();
            } else {
                break;
            }
        }
    }

    /// 近似记号（`~ ≈ < <= > >= ≤ ≥ 约`）；返回是否出现。
    pub(super) fn take_approx(&mut self) -> bool {
        let mut seen = false;
        loop {
            let rest = self.rest();
            let len = if rest.starts_with("<=") || rest.starts_with(">=") {
                2
            } else {
                match rest.chars().next() {
                    Some(mark @ ('~' | '≈' | '<' | '>' | '≤' | '≥' | '约')) => {
                        mark.len_utf8()
                    }
                    _ => 0,
                }
            };
            if len == 0 {
                return seen;
            }
            self.pos += len;
            seen = true;
            self.skip_spaces();
        }
    }

    /// 符号：返回 `(是否负号, 是否出现显式符号)`；`±` 记正号但算显式符号。
    pub(super) fn take_sign(&mut self) -> (bool, bool) {
        let rest = self.rest();
        if rest.starts_with('+') {
            self.pos += 1;
            (false, true)
        } else if rest.starts_with('−') {
            self.pos += '−'.len_utf8();
            (true, true)
        } else if rest.starts_with('-') {
            self.pos += 1;
            (true, true)
        } else if rest.starts_with('±') {
            self.pos += '±'.len_utf8();
            (false, true)
        } else {
            (false, false)
        }
    }

    /// 货币符号。
    pub(super) fn take_currency(&mut self) -> Option<&'static str> {
        let currency = match self.rest().chars().next()? {
            '$' => "$",
            '€' => "€",
            '£' => "£",
            '¥' => "¥",
            _ => return None,
        };
        self.pos += currency.len();
        Some(currency)
    }

    /// 单位：速率（`tok/s`）、`%`、词表单位、倍数记号（`×`/`x`）。
    pub(super) fn take_unit(&mut self) -> Option<UnitHit<'a>> {
        let rest = self.rest();
        // 速率必须先于普通单位判定，否则 `MB/s` 只会吃到 `MB`
        if let Some((text, len)) = rate_unit(rest) {
            self.pos += len;
            return Some(UnitHit {
                text,
                dim: Dimension::Rate,
                factor: 1.0,
                mult: 1.0,
            });
        }
        if rest.starts_with('%') {
            self.pos += 1;
            return Some(UnitHit {
                text: "%",
                dim: Dimension::Percent,
                factor: 1.0,
                mult: 1.0,
            });
        }
        if let Some(hit) = unit_token(rest) {
            self.pos += hit.text.len();
            return Some(hit);
        }
        if rest.starts_with('×') {
            self.pos += '×'.len_utf8();
            return Some(UnitHit {
                text: "×",
                dim: Dimension::Ratio,
                factor: 1.0,
                mult: 1.0,
            });
        }
        if rest.starts_with('x') && !is_alnum(rest[1..].chars().next()) {
            self.pos += 1;
            return Some(UnitHit {
                text: "x",
                dim: Dimension::Ratio,
                factor: 1.0,
                mult: 1.0,
            });
        }
        None
    }

    /// 「数字 + 可选单位」，数值以基本单位返回；失败时位置保持在调用前。
    pub(super) fn take_number(&mut self) -> Option<(f64, Option<UnitHit<'a>>)> {
        let save = self.pos;
        self.skip_spaces();
        let Some((magnitude, end)) = scan_decimal(self.text, self.pos) else {
            self.pos = save;
            return None;
        };
        self.pos = end;
        let after_mantissa = self.pos;
        self.skip_spaces();
        let hit = match self.take_unit() {
            Some(hit) => Some(hit),
            None => {
                self.pos = after_mantissa;
                None
            }
        };
        Some((magnitude, hit))
    }

    /// 区间右端：`a–b`、`a—b`（可带空格）与 `a-b`（ASCII 连字符必须紧贴数字）。
    pub(super) fn take_range(&mut self) -> Option<(f64, Option<UnitHit<'a>>)> {
        let save = self.pos;
        self.skip_spaces();
        let rest = self.rest();
        let dash = if rest.starts_with('–') || rest.starts_with('—') {
            '–'.len_utf8()
        } else if rest.starts_with('-')
            && self
                .text
                .as_bytes()
                .get(self.pos + 1)
                .is_some_and(u8::is_ascii_digit)
        {
            1
        } else {
            self.pos = save;
            return None;
        };
        self.pos += dash;
        match self.take_number() {
            Some(pair) => Some(pair),
            None => {
                self.pos = save;
                None
            }
        }
    }

    /// 箭头右端：`→`、`->`、`⇒`。
    pub(super) fn take_arrow(&mut self) -> Option<(f64, Option<UnitHit<'a>>)> {
        let save = self.pos;
        self.skip_spaces();
        let rest = self.rest();
        let len = if rest.starts_with('→') {
            '→'.len_utf8()
        } else if rest.starts_with("->") {
            2
        } else if rest.starts_with('⇒') {
            '⇒'.len_utf8()
        } else {
            self.pos = save;
            return None;
        };
        self.pos += len;
        match self.take_number() {
            Some(pair) => Some(pair),
            None => {
                self.pos = save;
                None
            }
        }
    }

    /// 紧贴的分数 `a/b`：返回分母（分子是已解析的 `value`）；`12 / 12` 这种带空格的形式不算。
    pub(super) fn take_fraction(&mut self) -> Option<f64> {
        if !self.rest().starts_with('/') {
            return None;
        }
        let save = self.pos;
        self.pos += 1;
        match scan_decimal(self.text, self.pos) {
            Some((denominator, end)) => {
                self.pos = end;
                Some(denominator)
            }
            None => {
                self.pos = save;
                None
            }
        }
    }

    /// 空格斜杠列表（`7648 / 176`、`a / b / c`）：剩余部分恰好是 1–3 段「/ 数值(单位)」。
    /// 值取第一项（已解析），其余各项只用于确认「这是一组并列数值」，不做换算。
    pub(super) fn take_slash_list(&mut self) -> bool {
        let start = self.pos;
        let mut items = 1;
        loop {
            let save = self.pos;
            self.skip_spaces();
            if !self.rest().starts_with('/') {
                self.pos = save;
                break;
            }
            self.pos += 1;
            match self.take_number() {
                Some(_) => items += 1,
                None => {
                    self.pos = save;
                    break;
                }
            }
            if items > SLASH_LIST_MAX {
                self.pos = start;
                return false;
            }
        }
        if items < SLASH_LIST_MIN {
            self.pos = start;
            return false;
        }
        let tail = self.pos;
        self.skip_spaces();
        if self.pos == self.text.len() {
            true
        } else {
            self.pos = tail;
            false
        }
    }
}

/// 扫描一个数值单元格；返回 `None` 表示该文本不是数值。
pub(super) fn scan_number(text: &str, hint: Option<UnitHit<'_>>) -> Option<Number> {
    let mut scanner = Scanner { text, pos: 0 };
    scanner.skip_spaces();
    let approx = scanner.take_approx();
    scanner.skip_spaces();
    let (negative, signed) = scanner.take_sign();
    scanner.skip_spaces();
    let currency = scanner.take_currency();
    scanner.skip_spaces();
    let (mantissa, end) = scan_decimal(text, scanner.pos)?;
    scanner.pos = end;

    // 单位紧跟数字或隔一个空格；没匹配到就把位置退回数字之后，尾部留给后缀规则
    let after_mantissa = scanner.pos;
    scanner.skip_spaces();
    let first_unit = scanner.take_unit();
    if first_unit.is_none() {
        scanner.pos = after_mantissa;
    }

    // 区间与箭头互斥；右端带单位时左端可以继承（`24–72 h`）
    let mut upper_input = None;
    let mut to_input = None;
    if let Some(pair) = scanner.take_range() {
        upper_input = Some(pair);
    } else if let Some(pair) = scanner.take_arrow() {
        to_input = Some(pair);
    }
    let right_unit = upper_input
        .as_ref()
        .and_then(|(_, hit)| *hit)
        .or_else(|| to_input.as_ref().and_then(|(_, hit)| *hit));
    let inherited = first_unit.or(right_unit);
    let (mut dim, mut factor, mult, mut unit) = match inherited {
        Some(hit) => (hit.dim, hit.factor, hit.mult, hit.text),
        None => (Dimension::Count, 1.0, 1.0, ""),
    };

    let mut value = mantissa * mult;
    // 复合时长（`1m 56s`）累加到秒：展示单位统一成 `s`，`1m 56s` 才和 `45s` 同组可比。
    if dim == Dimension::Duration {
        let mut compound = false;
        loop {
            let save = scanner.pos;
            match scanner.take_number() {
                Some((part, Some(hit))) if hit.dim == Dimension::Duration => {
                    value += part * hit.mult;
                    compound = true;
                }
                _ => {
                    scanner.pos = save;
                    break;
                }
            }
        }
        if compound {
            factor = 1.0;
            unit = "s";
        }
    }

    let mut upper = upper_input.map(|(magnitude, hit)| in_base(magnitude, hit, mult));
    let mut to = to_input.map(|(magnitude, hit)| in_base(magnitude, hit, mult));

    // 分数与斜杠列表：`12/12` 记分母，`7648 / 176` 只取第一项
    let mut denominator = None;
    if upper.is_none() && to.is_none() {
        match scanner.take_fraction() {
            Some(parsed) => {
                denominator = Some(parsed);
                dim = Dimension::Fraction;
                factor = 1.0;
                unit = "";
            }
            None => {
                scanner.take_slash_list();
            }
        }
    }
    if let Some(currency) = currency {
        dim = Dimension::Currency;
        unit = currency;
    }
    // 表头标注的单位由裸数字继承（列单位只在单元格自己没写单位时生效）
    if let Some(hint) = hint
        && dim == Dimension::Count
        && unit.is_empty()
    {
        dim = hint.dim;
        factor = hint.factor;
        unit = hint.text;
        value *= hint.mult;
        upper = upper.map(|bound| bound * hint.mult);
        to = to.map(|target| target * hint.mult);
    }

    let has_unit = inherited.is_some() || currency.is_some() || denominator.is_some();
    if !valid_numeric_tail(scanner.rest(), has_unit) {
        return None;
    }

    Some(Number {
        value: if negative { -value } else { value },
        factor,
        dim,
        unit: unit.to_owned(),
        figure: text.trim().to_owned(),
        approx,
        signed,
        upper,
        to,
        denominator,
    })
}

/// 校验数字（及可选单位）之后剩余文本是否符合数值单元格的尾部规则。
///
/// 放行空白、公差/空格连字符（`± 5`、`12 - 15`）、末尾括号注释（`127 ms (33 块)`），
/// 以及无已知单位时的单个简短自定义单位词（`12 samples`、`100 条`）；拒绝
/// `100 条普通消息`、`1. 安装依赖`、`10k plain messages` 等描述性文本。
fn valid_numeric_tail(rest: &str, has_unit: bool) -> bool {
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        return true;
    }
    if let Some(after) = strip_tolerance_prefix(trimmed) {
        return valid_tolerance_tail(after);
    }
    let Some(core) = strip_trailing_parens(trimmed) else {
        return false;
    };
    if core.is_empty() {
        return true;
    }
    if has_unit {
        return false;
    }
    is_custom_unit_word(core, !rest.starts_with(char::is_whitespace))
}

/// 剥离公差或空格连字符前缀（`±` 或 `- `），返回剩余切片。
fn strip_tolerance_prefix(text: &str) -> Option<&str> {
    if let Some(rest) = text.strip_prefix('±') {
        Some(rest)
    } else {
        text.strip_prefix("- ")
    }
}

/// 校验 `±` / `- ` 之后的公差部分：数字 + 可选已登记单位 + 可选末尾括号注释。
fn valid_tolerance_tail(text: &str) -> bool {
    let mut scanner = Scanner { text, pos: 0 };
    scanner.skip_spaces();
    let Some((_, end)) = scan_decimal(text, scanner.pos) else {
        return false;
    };
    scanner.pos = end;
    let after_num = scanner.pos;
    scanner.skip_spaces();
    if scanner.take_unit().is_none() {
        scanner.pos = after_num;
    }
    matches!(strip_trailing_parens(scanner.rest().trim()), Some(""))
}

/// 从末尾剥除一层或多层配对括号注释（`(...)` / `（...）`）；括号不配对返回 `None`。
fn strip_trailing_parens(mut text: &str) -> Option<&str> {
    loop {
        text = text.trim();
        let close = text.chars().next_back();
        if !matches!(close, Some(')' | '）')) {
            break;
        }
        let mut depth = 0usize;
        let mut open_idx = None;
        for (idx, ch) in text.char_indices().rev() {
            match ch {
                ')' | '）' => depth += 1,
                '(' | '（' => {
                    depth -= 1;
                    if depth == 0 {
                        open_idx = Some(idx);
                        break;
                    }
                }
                _ => {}
            }
        }
        let idx = open_idx?;
        text = &text[..idx];
    }
    if text.contains(['(', ')', '（', '）']) {
        return None;
    }
    Some(text.trim())
}

/// 判断无已知单位的尾部是否为单个简短自定义单位词（如 `samples`、`req/min`、`条`）。
fn is_custom_unit_word(word: &str, attached: bool) -> bool {
    if word.chars().any(char::is_whitespace) {
        return false;
    }
    if word.is_ascii() {
        if word.len() > 12 {
            return false;
        }
        let mut chars = word.chars();
        if !chars.next().is_some_and(|ch| ch.is_ascii_alphabetic()) {
            return false;
        }
        if !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '/' | '-')) {
            return false;
        }
        if attached
            && matches!(
                word.to_ascii_lowercase().as_str(),
                "st" | "nd" | "rd" | "th"
            )
        {
            return false;
        }
        return true;
    }
    let mut chars = word.chars();
    let Some(ch) = chars.next() else {
        return false;
    };
    chars.next().is_none() && is_cjk_quantifier(ch)
}

/// 是否为汉字量词字符（排除标点与空白）。
fn is_cjk_quantifier(ch: char) -> bool {
    matches!(
        ch,
        '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}'
    )
}

/// 把书写数值折算到基本单位：右端自带单位就用右端的，否则继承左端单位。
pub(super) fn in_base(magnitude: f64, hit: Option<UnitHit<'_>>, fallback: f64) -> f64 {
    magnitude * hit.map_or(fallback, |hit| hit.mult)
}

/// 从 `start` 起解析十进制字面量（千分位 `,`、小数、`e/E` 指数），返回 `(值, 结束位置)`。
pub(super) fn scan_decimal(text: &str, start: usize) -> Option<(f64, usize)> {
    let bytes = text.as_bytes();
    let mut pos = start;
    let mut mantissa = 0.0;
    let mut digits = 0;
    while pos < bytes.len() {
        let byte = bytes[pos];
        if byte.is_ascii_digit() {
            mantissa = mantissa * 10.0 + f64::from(byte - b'0');
            digits += 1;
            pos += 1;
        } else if byte == b',' && bytes.get(pos + 1).is_some_and(u8::is_ascii_digit) {
            pos += 1;
        } else {
            break;
        }
    }
    if digits == 0 {
        return None;
    }
    let mut value = mantissa;
    if bytes.get(pos) == Some(&b'.') && bytes.get(pos + 1).is_some_and(u8::is_ascii_digit) {
        pos += 1;
        let mut scale = 0.1;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            value += f64::from(bytes[pos] - b'0') * scale;
            scale *= 0.1;
            pos += 1;
        }
    }
    if matches!(bytes.get(pos), Some(b'e' | b'E')) {
        let mut cursor = pos + 1;
        let mut negative = false;
        if matches!(bytes.get(cursor), Some(b'+' | b'-')) {
            negative = bytes[cursor] == b'-';
            cursor += 1;
        }
        let exp_start = cursor;
        let mut exponent = 0i32;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            exponent = (exponent * 10 + i32::from(bytes[cursor] - b'0')).min(1_000);
            cursor += 1;
        }
        if cursor > exp_start {
            value *= 10f64.powi(if negative { -exponent } else { exponent });
            pos = cursor;
        }
    }
    Some((value, pos))
}
