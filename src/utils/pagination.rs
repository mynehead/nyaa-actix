use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Pagination {
    pub current: i64,
    pub total_pages: i64,
    pub total_items: i64,
    pub per_page: i64,
    pub has_prev: bool,
    pub has_next: bool,
    pub pages: Vec<PageItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PageItem {
    pub num: i64,
    pub is_current: bool,
    pub is_ellipsis: bool,
}

impl Pagination {
    pub fn new(current: i64, total_items: i64, per_page: i64) -> Self {
        let total_pages = ((total_items as f64) / (per_page as f64)).ceil() as i64;
        let total_pages = total_pages.max(1);
        let current = current.clamp(1, total_pages);

        let pages = build_pages(current, total_pages);

        Pagination {
            current,
            total_pages,
            total_items,
            per_page,
            has_prev: current > 1,
            has_next: current < total_pages,
            pages,
        }
    }

    pub fn offset(&self) -> i64 {
        (self.current - 1) * self.per_page
    }
}

fn build_pages(current: i64, total: i64) -> Vec<PageItem> {
    let mut pages = Vec::new();
    if total <= 9 {
        for i in 1..=total {
            pages.push(PageItem { num: i, is_current: i == current, is_ellipsis: false });
        }
        return pages;
    }
    // Always show first, last, and window around current
    let mut nums: Vec<i64> = vec![1, total];
    for i in (current - 2).max(2)..=(current + 2).min(total - 1) {
        nums.push(i);
    }
    nums.sort_unstable();
    nums.dedup();

    let mut prev = 0i64;
    for n in nums {
        if prev > 0 && n - prev > 1 {
            pages.push(PageItem { num: 0, is_current: false, is_ellipsis: true });
        }
        pages.push(PageItem { num: n, is_current: n == current, is_ellipsis: false });
        prev = n;
    }
    pages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nums(p: &Pagination) -> Vec<i64> {
        p.pages.iter().map(|i| i.num).collect()
    }

    #[test]
    fn empty_result_is_one_page() {
        let p = Pagination::new(1, 0, 75);
        assert_eq!(p.total_pages, 1);
        assert!(!p.has_prev && !p.has_next);
    }

    #[test]
    fn clamps_current_page_and_computes_offset() {
        let p = Pagination::new(99, 150, 75);
        assert_eq!((p.current, p.total_pages, p.offset()), (2, 2, 75));
        assert_eq!(Pagination::new(-3, 150, 75).current, 1);
    }

    #[test]
    fn short_lists_show_every_page() {
        assert_eq!(nums(&Pagination::new(3, 9 * 10, 10)), (1..=9).collect::<Vec<_>>());
    }

    #[test]
    fn long_lists_use_ellipses_around_a_window() {
        // 0 marks an ellipsis
        assert_eq!(nums(&Pagination::new(10, 200, 10)), vec![1, 0, 8, 9, 10, 11, 12, 0, 20]);
        assert_eq!(nums(&Pagination::new(1, 200, 10)), vec![1, 2, 3, 0, 20]);
        assert_eq!(nums(&Pagination::new(20, 200, 10)), vec![1, 0, 18, 19, 20]);
    }
}
