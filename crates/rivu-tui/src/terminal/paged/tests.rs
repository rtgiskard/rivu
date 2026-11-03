use super::*;

#[test]
fn viewport_navigation_and_retention_do_not_depend_on_batch_size() {
    for fetch_size in [1, 3, 32] {
        let mut list = PagedList::default();
        list.set_viewport(10);
        for down in [true, false] {
            for _ in 0..200 {
                let mut requests = 0;
                while let Some(offset) = list.next_offset(fetch_size) {
                    let rows = (offset..(offset + fetch_size).min(10_000)).collect();
                    list.insert(offset, fetch_size, 10_000, rows);
                    requests += 1;
                    assert!(requests <= 10usize.div_ceil(fetch_size) + 4);
                }
                assert_eq!(list.total, 10_000);
                assert!(list.pages.len() <= 10usize.div_ceil(fetch_size) + 3);
                assert!(
                    (list.selection.offset()..list.visible_end())
                        .all(|index| list.get(index) == Some(&index))
                );
                let before = list.selection.selected().unwrap();
                list.navigate_action(
                    if down {
                        Action::PageDown
                    } else {
                        Action::PageUp
                    },
                    10,
                );
                assert_eq!(
                    list.selection.selected(),
                    Some(if down {
                        before + 10
                    } else {
                        before.saturating_sub(10)
                    })
                );
            }
        }
    }
}
