//! 降级输出：类型区域空白化 + facade 调用点插入，行号与源 1:1。

use oxc_span::Span;

/// 应用两类源编辑：
/// - `blank`：区域内每个非换行字节替换为空格（不动字节数与行号）；
/// - `inserts`：(offset, tag, 文本) 在 offset 处插入文本（同 offset 多段按插入序）。
///
/// 最后在文件头注入 `const {args, run} = __wf;`（用户已自行声明的名字跳过），
/// 注入在同一行内，不引入新行。
pub(super) fn lower(
    source: &str,
    blank: &[Span],
    inserts: &[(u32, &'static str, String)],
    user_owns: &[bool; 2],
) -> String {
    let mut bytes = source.as_bytes().to_vec();
    let mut blank = blank.to_vec();
    blank.sort_by_key(|s| s.start);
    for span in &blank {
        for b in &mut bytes[span.start as usize..span.end as usize] {
            if *b != b'\n' && *b != b'\r' {
                *b = b' ';
            }
        }
    }
    let mut inserts = inserts.to_vec();
    inserts.sort_by_key(|(pos, _, _)| *pos);
    let mut prefix = String::new();
    if !user_owns[0] && !user_owns[1] {
        prefix.push_str("const {args, run} = __wf;");
    } else if !user_owns[0] {
        prefix.push_str("const args = __wf.args;");
    } else if !user_owns[1] {
        prefix.push_str("const run = __wf.run;");
    }
    let mut out = String::with_capacity(bytes.len() + prefix.len() + 256);
    out.push_str(&prefix);
    let mut cursor = 0usize;
    for (pos, _tag, text) in &inserts {
        let pos = *pos as usize;
        out.push_str(std::str::from_utf8(&bytes[cursor..pos]).unwrap_or_default());
        out.push_str(text);
        cursor = pos;
    }
    out.push_str(std::str::from_utf8(&bytes[cursor..]).unwrap_or_default());
    out
}
