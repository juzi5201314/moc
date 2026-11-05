//! 只读文件代码页的空闲回收；不释放对象，也不修改堆、栈或重定位页。
use std::time::{Duration, Instant};

/// 每段空闲只执行一次，交互后至少等 30 秒，两次回收至少相隔 60 秒。
/// 启动后首次静置 2 秒可回收初始化路径；持续任务期间由前端暂停。
pub struct IdleReclaim {
    deadline: Option<Instant>,
    last: Option<Instant>,
}

impl IdleReclaim {
    pub fn new(now: Instant) -> Self {
        Self {
            deadline: Some(now + Duration::from_secs(2)),
            last: None,
        }
    }

    pub fn activity(&mut self, now: Instant) {
        let delay = if self.last.is_some() { 30 } else { 2 };
        let deadline = now + Duration::from_secs(delay);
        self.deadline = Some(self.last.map_or(deadline, |last| {
            deadline.max(last + Duration::from_secs(60))
        }));
    }

    pub fn deadline(&self) -> Option<Instant> {
        if cfg!(target_os = "linux") {
            self.deadline
        } else {
            None
        }
    }

    fn take_due(&mut self, now: Instant) -> bool {
        if !self.deadline.is_some_and(|deadline| now >= deadline) {
            return false;
        }
        self.deadline = None;
        self.last = Some(now);
        true
    }

    pub fn reclaim(&mut self, now: Instant) {
        if self.take_due(now) {
            #[cfg(target_os = "linux")]
            linux::reclaim();
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ops::Range;

    // 只选加载时就不可写的 PT_LOAD。RELRO 即使当前只读也必须排除，
    // 因其加载段带 PF_W，包含不能由原文件恢复的重定位数据。
    fn file_pages(base: usize, address: usize, bytes: usize, page: usize) -> Option<Range<usize>> {
        let start = base.checked_add(address)?.checked_add(page - 1)? / page * page;
        let end = base.checked_add(address)?.checked_add(bytes)? / page * page;
        (start < end).then_some(start..end)
    }

    pub(super) fn reclaim() {
        // ptrace 调试器可绕过只读保护写入断点；此时不能丢弃其 COW 页面。
        let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
            return;
        };
        if !status.lines().any(|line| {
            line.strip_prefix("TracerPid:")
                .is_some_and(|pid| pid.trim() == "0")
        }) {
            return;
        }
        // SAFETY: 回调只在 dl_iterate_phdr 调用期间访问加载器拥有的头部，
        // 不保存其指针，不加载/卸载动态库；主可执行映射在进程生命周期内保持有效。
        unsafe {
            libc::dl_iterate_phdr(Some(visit), std::ptr::null_mut());
        }
    }

    unsafe extern "C" fn visit(
        info: *mut libc::dl_phdr_info,
        _size: libc::size_t,
        _data: *mut libc::c_void,
    ) -> libc::c_int {
        // SAFETY: 加载器保证 info、dlpi_name 和 phdr 数组在回调期间有效。
        let info = unsafe { &*info };
        if !info.dlpi_name.is_null() && unsafe { *info.dlpi_name } != 0 {
            return 0;
        }
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page <= 0 {
            return 1;
        }
        let headers = unsafe { std::slice::from_raw_parts(info.dlpi_phdr, info.dlpi_phnum.into()) };
        for header in headers {
            if header.p_type != libc::PT_LOAD || header.p_flags & libc::PF_W != 0 {
                continue;
            }
            if let Some(range) = file_pages(
                info.dlpi_addr as usize,
                header.p_vaddr as usize,
                header.p_filesz as usize,
                page as usize,
            ) {
                // SAFETY: 完整页均在不可写、文件支撑的主程序 PT_LOAD 内；
                // DONTNEED 保留映射，随后执行/读取由内核重新缺页映射原文件内容。
                // 不触碰匿名尾部、可写加载段或 RELRO。失败仅表示内核未回收。
                unsafe {
                    libc::madvise(
                        range.start as *mut libc::c_void,
                        range.len(),
                        libc::MADV_DONTNEED,
                    );
                }
            }
        }
        1
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn excludes_partial_pages_and_overflow() {
            assert_eq!(file_pages(0x10000, 100, 9000, 4096), Some(0x11000..0x12000));
            assert_eq!(file_pages(0x10000, 100, 4000, 4096), None);
            assert_eq!(file_pages(usize::MAX, 1, 4096, 4096), None);
            assert_eq!(file_pages(0x10000, 0, 4096, 4096), Some(0x10000..0x11000));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_period_is_one_shot_and_activity_respects_cooldown() {
        let now = Instant::now();
        let mut idle = IdleReclaim::new(now);
        assert_eq!(idle.deadline, Some(now + Duration::from_secs(2)));
        idle.activity(now + Duration::from_secs(1));
        assert_eq!(idle.deadline, Some(now + Duration::from_secs(3)));
        assert!(!idle.take_due(now + Duration::from_secs(2)));
        assert!(idle.take_due(now + Duration::from_secs(3)));
        assert!(!idle.take_due(now + Duration::from_secs(4)));
        assert_eq!(idle.deadline, None);
        idle.activity(now + Duration::from_secs(4));
        assert_eq!(idle.deadline, Some(now + Duration::from_secs(63)));
        idle.activity(now + Duration::from_secs(90));
        assert_eq!(idle.deadline, Some(now + Duration::from_secs(120)));
    }
}
