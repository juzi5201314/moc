use super::super::PROSE_INDENT;
use super::super::image::ImageKey;
use super::super::image::ImagePlacement;
use super::super::image::ImageSourceSpec;
use super::super::image::ImageState;
use super::super::image::layout as image_layout;
use super::MdRenderer;
use super::resources::file_key;
use super::resources::file_url_path;
use crate::render::markdown::is_mermaid_fence_info;
use crate::render::markdown::is_svg_fence_info;
use crate::render::markdown::is_widget_fence_info;
use crate::render::markdown::widget_fence_title;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use rust_i18n::t;
use std::path::PathBuf;

impl<'a> MdRenderer<'a> {
    /// 图片落定：能出图就留占位行，否则回落 alt 文本（改动前的内联行为）。
    ///
    /// 只有真正出图时才把前面的正文结算成一行——图片必须独占块，段落在这里
    /// 被劈开是合理的；画不出图时 alt 直接回到 inline 缓冲，段落保持原样。
    pub(super) fn finish_image(&mut self) {
        let Some(image) = self.image.take() else {
            return;
        };
        if !self.place_image(&image.dest) {
            let style = self.current_style();
            self.push_inline_text(&image.alt, style);
        }
    }

    /// 为本地图片留出占位行；返回 `false` 表示这一步画不出图。
    pub(super) fn place_image(&mut self, dest: &str) -> bool {
        // 引用块 / 列表 / 表格单元格里的图片不贴：没有独立命中区，宽度也不够
        if self.quote_depth > 0 || !self.list_stack.is_empty() || self.in_table_cell {
            return false;
        }
        // 终端画不了像素图就别解码：解出来只会回落成 alt 文本，
        // 白白占几十 MB 解码缓冲与缓存预算。
        if self.views.graphics.is_none() {
            return false;
        }
        let Some(path) = self.local_path(dest) else {
            return false;
        };
        let Some(key) = file_key(&path) else {
            return false;
        };
        let Some(store) = self.views.images else {
            return false;
        };
        let state = if self.views.spawn_images {
            let source = path;
            store.state(&key, move || ImageSourceSpec::File(source))
        } else {
            match store.peek(&key) {
                Some(state) => state,
                None => return false,
            }
        };
        self.emit_placeholder(&key, state)
    }

    /// mermaid 代码块：画成字符图。引用、列表、未闭合的流式围栏和认不出的图种留下代码块。
    pub(super) fn place_mermaid(&mut self, info: &str, code: &str) -> bool {
        if self.quote_depth > 0 || !self.list_stack.is_empty() {
            return false;
        }
        if !is_mermaid_fence_info(info) {
            return false;
        }
        if self.views.streaming && !self.code_closed {
            return false;
        }
        let Some(lines) = super::super::mermaid::render(code, self.width, self.theme) else {
            return false;
        };
        self.out.extend(lines);
        self.out.push(Line::from(String::new()));
        true
    }

    /// SVG 代码块：整块换成位图占位行（比一屏坐标文本有用得多）。
    pub(super) fn place_svg(&mut self, info: &str, code: &str) -> bool {
        if self.quote_depth > 0 || !self.list_stack.is_empty() {
            return false;
        }
        if !is_svg_fence_info(info) || code.trim().is_empty() {
            return false;
        }
        // 终端画不了像素图就别解码：解出来也只会回落成代码块，
        // 白白占几十 MB 解码缓冲与缓存预算。
        if self.views.graphics.is_none() {
            return false;
        }
        let Some(store) = self.views.images else {
            return false;
        };
        let key = ImageKey::from_svg(code);
        let state = if self.views.spawn_images {
            let text = code.to_owned().into_boxed_str();
            store.state(&key, move || ImageSourceSpec::Svg(text))
        } else {
            match store.peek(&key) {
                Some(state) => state,
                None => return false,
            }
        };
        self.emit_placeholder(&key, state)
    }

    /// WebUI 交互组件围栏：TUI 中折叠为单行次级色占位文本，避免满屏源码噪音。
    pub(super) fn place_widget(&mut self, info: &str, code: &str) -> bool {
        if self.quote_depth > 0 || !self.list_stack.is_empty() || !is_widget_fence_info(info) {
            return false;
        }
        let lines = code.lines().count().max(1);
        let label = match widget_fence_title(info) {
            Some(title) => t!("tui.md.widget_placeholder", title = title, lines = lines).into_owned(),
            None => t!("tui.md.widget_placeholder_untitled", lines = lines).into_owned(),
        };
        let clipped = super::super::wrap::truncate(label, self.width.saturating_sub(2).max(1));
        self.out.push(Line::from(Span::styled(
            clipped.into_owned(),
            Style::new().fg(self.theme.gray_dim),
        )));
        self.out.push(Line::from(String::new()));
        true
    }

    /// 解码就绪就按版式留空行并登记落位；其余状态返回 `false`（回落文本）。
    pub(super) fn emit_placeholder(&mut self, key: &ImageKey, state: ImageState) -> bool {
        let (ImageState::Ready(prepared), Some(graphics)) = (state, self.views.graphics) else {
            return false;
        };
        let Some(layout) = image_layout(prepared.size(), graphics.cell, self.cols()) else {
            return false;
        };
        self.flush_inline();
        self.ensure_block_gap();
        let row = self.out.len();
        // 占位行只放空格：像素由 `graphics::emit` 在帧后贴上去
        for _ in 0..layout.rows {
            self.out.push(Line::from(" ".repeat(self.width)));
        }
        self.images.push(ImagePlacement {
            row,
            x: u16::try_from(PROSE_INDENT).unwrap_or(0),
            cols: layout.cols,
            rows: layout.rows,
            px: layout.px,
            key: key.clone(),
        });
        self.out.push(Line::from(String::new()));
        true
    }

    /// markdown 链接目标 → 本地路径。只认本地文件：`http(s)://` 与 `data:`
    /// 既不联网也不内联解码（那是另一条信任边界）。
    ///
    /// 不限制路径范围：`..`、绝对路径、`~` 都照常解析。图片是给人看的，
    /// 而会话里的模型本来就有 shell 权限，在这里加目录围栏挡不住任何东西，
    /// 只会让 `/tmp` 与 `$HOME` 下的合法图片打不开。
    pub(super) fn local_path(&self, dest: &str) -> Option<std::path::PathBuf> {
        let dest = dest.trim();
        if dest.is_empty() {
            return None;
        }
        let raw = match dest.strip_prefix("file://").or_else(|| {
            // scheme 大小写不敏感（RFC 3986），`FILE://` 与 `file://` 等价
            dest.get(..7)
                .filter(|prefix| prefix.eq_ignore_ascii_case("file://"))
                .map(|prefix| &dest[prefix.len()..])
        }) {
            Some(rest) => file_url_path(rest)?,
            None if dest.contains("://") => return None,
            None => PathBuf::from(dest),
        };
        let path = match raw.to_str().and_then(|text| text.strip_prefix("~/")) {
            Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
            None => raw,
        };
        if path.is_absolute() {
            Some(path)
        } else {
            Some(self.views.cwd?.join(path))
        }
    }
}
