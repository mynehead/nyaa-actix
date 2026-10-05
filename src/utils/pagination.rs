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
}

/// Page numbers as upstream shows them: Flask-SQLAlchemy's
/// `iter_pages(left_edge=2, left_current=6, right_current=6, right_edge=0)`,
/// with an ellipsis for each gap.
fn build_pages(current: i64, total: i64) -> Vec<PageItem> {
    const LEFT_EDGE: i64 = 2;
    const LEFT_CURRENT: i64 = 6;
    const RIGHT_CURRENT: i64 = 6;
    const RIGHT_EDGE: i64 = 0;

    let mut pages = Vec::new();
    let mut last = 0;
    for num in 1..=total {
        if num <= LEFT_EDGE
            || (num > current - LEFT_CURRENT - 1 && num < current + RIGHT_CURRENT)
            || num > total - RIGHT_EDGE
        {
            if last + 1 != num {
                pages.push(PageItem { num: 0, is_current: false, is_ellipsis: true });
            }
            pages.push(PageItem { num, is_current: num == current, is_ellipsis: false });
            last = num;
        }
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
    fn clamps_current_page() {
        let p = Pagination::new(99, 150, 75);
        assert_eq!((p.current, p.total_pages), (2, 2));
        assert_eq!(Pagination::new(-3, 150, 75).current, 1);
    }

    #[test]
    fn short_lists_show_pages_near_current() {
        assert_eq!(nums(&Pagination::new(3, 8 * 10, 10)), (1..=8).collect::<Vec<_>>());
        // No right edge: like upstream, the last page only shows once it is within reach
        assert_eq!(nums(&Pagination::new(3, 9 * 10, 10)), (1..=8).collect::<Vec<_>>());
    }

    #[test]
    fn long_lists_match_upstream_window() {
        // 0 marks an ellipsis; two edge pages, 5 before and 5 after the current one, no right edge
        assert_eq!(nums(&Pagination::new(10, 200, 10)), vec![1, 2, 0, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        assert_eq!(nums(&Pagination::new(1, 200, 10)), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(nums(&Pagination::new(20, 200, 10)), vec![1, 2, 0, 14, 15, 16, 17, 18, 19, 20]);
    }
}
