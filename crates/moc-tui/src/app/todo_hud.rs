//! 待办浮层的拖动、定位与命中状态。

use super::{TODO_DRAG_THRESHOLD, TodoHudPress, TodoHudState};
use ratatui::layout::Rect;

impl TodoHudState {
    /// 每帧绘制开头：清命中区与帧标志，保留交互状态。
    pub fn begin_frame(&mut self) {
        self.chip_area = Rect::default();
        self.card_area = Rect::default();
        self.card_visible = false;
        self.marquee_active = false;
    }

    /// 模态、设置页或空间不足时收起；保留折叠状态与记忆位置。
    pub fn on_hidden(&mut self) {
        self.press = None;
        self.chip_area = Rect::default();
        self.card_area = Rect::default();
        self.card_visible = false;
        self.marquee_active = false;
    }

    /// 空清单或切换会话：全部复位，回到默认展开态。
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn on_resize(&mut self) {
        self.press = None;
        self.chip_area = Rect::default();
        self.card_area = Rect::default();
        self.card_visible = false;
        self.marquee_key.clear();
        self.marquee_start = None;
        self.marquee_active = false;
    }

    pub fn on_focus_lost(&mut self) {
        self.press = None;
        self.marquee_active = false;
    }

    /// 展开态显示卡片。
    pub fn is_open(&self) -> bool {
        !self.collapsed
    }

    /// 命中当前可见的 HUD 部件；胶囊与卡片互斥，命中区不会同时存在。
    pub fn hit(&self, pos: (u16, u16)) -> bool {
        self.chip_area.contains(pos.into()) || self.card_area.contains(pos.into())
    }

    /// 鼠标移动：返回是否落在 HUD 上（按住卡片期间始终算命中）。
    pub fn pointer_moved(&mut self, pos: (u16, u16)) -> bool {
        // 按住卡片时卡片随光标移动，旧命中区不能拿来判定离开。
        if self.press.is_some_and(|press| press.from_card) {
            return true;
        }
        self.hit(pos)
    }

    /// 左键按下：命中当前部件则记录单击/拖动起点并消费。
    pub fn pointer_down(&mut self, pos: (u16, u16)) -> bool {
        let from_card = self.is_open();
        let area = if from_card {
            self.card_area
        } else {
            self.chip_area
        };
        if !area.contains(pos.into()) {
            return false;
        }
        self.press = Some(TodoHudPress {
            start: pos,
            card_origin: (self.card_area.x, self.card_area.y),
            from_card,
            dragging: false,
        });
        true
    }

    /// 左键拖动：卡片按下后越过阈值即移动卡片。
    pub fn pointer_drag(&mut self, pos: (u16, u16), area: Rect) -> bool {
        let Some(press) = self.press.as_mut() else {
            return false;
        };
        if !press.from_card {
            return false;
        }
        if !press.dragging {
            let dx = pos.0.abs_diff(press.start.0);
            let dy = pos.1.abs_diff(press.start.1);
            if dx.max(dy) <= TODO_DRAG_THRESHOLD {
                return true;
            }
            press.dragging = true;
        }
        let dx = i32::from(pos.0) - i32::from(press.start.0);
        let dy = i32::from(pos.1) - i32::from(press.start.1);
        let origin = (
            i32::from(press.card_origin.0) + dx,
            i32::from(press.card_origin.1) + dy,
        );
        let size = (self.card_area.width, self.card_area.height);
        self.drag_origin = Some(clamp_todo_origin(origin, size, area));
        true
    }

    /// 左键抬起：拖动结束，或单击在展开卡片与折叠胶囊间切换。
    pub fn pointer_up(&mut self, pos: (u16, u16)) -> bool {
        let Some(press) = self.press.take() else {
            return false;
        };
        if press.dragging {
            return true;
        }
        // 按下后移出 HUD 再松开 = 取消本次点击，不切换。
        if !self.hit(pos) {
            return true;
        }
        if press.from_card {
            self.collapsed = true;
            self.card_area = Rect::default();
        } else {
            self.collapsed = false;
        }
        true
    }
}

/// 把卡片原点夹在转写区内；尺寸大于区域时退回区域原点。
pub(super) fn clamp_todo_origin(origin: (i32, i32), size: (u16, u16), area: Rect) -> (u16, u16) {
    let max_x = area.right().saturating_sub(size.0).max(area.x);
    let max_y = area.bottom().saturating_sub(size.1).max(area.y);
    let x = origin.0.clamp(i32::from(area.x), i32::from(max_x));
    let y = origin.1.clamp(i32::from(area.y), i32::from(max_y));
    (
        u16::try_from(x).unwrap_or(area.x),
        u16::try_from(y).unwrap_or(area.y),
    )
}
