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

fn loaded_list() -> PagedList<usize> {
    let mut list = PagedList::default();
    list.set_viewport(4);
    list.insert(0, 2, 10, vec![0, 1]);
    list.insert(2, 2, 10, vec![2, 3]);
    list.insert(4, 2, 10, vec![4, 5]);
    list
}

#[test]
fn invalidation_moves_non_clone_rows_into_display_without_authority() {
    struct Row(usize);

    let mut list = PagedList::default();
    list.set_viewport(2);
    list.insert(0, 2, 4, vec![Row(7), Row(8)]);
    list.insert(2, 2, 4, vec![Row(9), Row(10)]);
    let address = list.get(0).unwrap() as *const Row;
    list.invalidate();

    assert!(list.pages.is_empty());
    assert_eq!(list.total, 0);
    assert!(!list.loaded);
    assert!(list.get(0).is_none());
    assert!(list.selected().is_none());
    assert_eq!(list.next_offset(2), Some(0));
    assert_eq!(list.display(0).map(|row| row.0), Some(7));
    assert_eq!(list.display(0).unwrap() as *const Row, address);
    assert!(list.display(2).is_none());
    assert_eq!(list.display_total(), 4);
    assert!(list.display_loaded());
    assert_eq!(list.display_visible_end(), 2);
    assert_eq!(
        list.display_iter().map(|row| row.0).collect::<Vec<_>>(),
        vec![7, 8]
    );
    assert_eq!(list.retained.as_ref().unwrap().pages.len(), 1);
}

#[test]
fn repeated_invalidation_never_extends_a_continuous_stale_interval() {
    let mut list = loaded_list();
    list.invalidate();
    let deadline = list.refresh_deadline().unwrap();
    assert!(!list.expire_refresh(deadline - Duration::from_nanos(1)));
    list.invalidate();
    assert_eq!(list.refresh_deadline(), Some(deadline));
    list.insert(0, 2, 10, vec![10, 11]);
    assert_eq!(list.display(0), Some(&10));
    assert_eq!(list.display(2), Some(&2));
    list.invalidate();
    assert_eq!(list.refresh_deadline(), Some(deadline));
    assert_eq!(list.display(0), Some(&10));
    assert_eq!(list.display(2), Some(&2));

    assert!(list.expire_refresh(deadline));
    assert!(!list.expire_refresh(deadline + REFRESH_RETENTION));
    assert_eq!(list.refresh_deadline(), None);
    assert_eq!(list.display(0), None);
    assert_eq!(list.display_total(), 0);
    assert!(!list.display_loaded());
    list.insert(0, 2, 10, vec![10, 11]);
    list.invalidate();
    assert_eq!(list.refresh_deadline(), None);
    assert_eq!(list.display(0), None);

    list.insert(0, 2, 10, vec![10, 11]);
    list.insert(2, 2, 10, vec![2, 3]);
    assert!(list.retained.is_none());
    list.invalidate();
    assert!(list.refresh_deadline().is_some());
    assert_eq!(list.display(0), Some(&10));
}

#[test]
fn fresh_pages_replace_only_their_stale_page_and_equal_replies_restore_after_expiry() {
    let mut list = loaded_list();
    list.invalidate_pages();
    assert!(list.loaded);
    assert_eq!(list.total, 10);
    assert_eq!(list.get(2), None);
    assert_eq!(list.selected(), None);
    assert_eq!(list.next_offset(2), Some(0));
    let deadline = list.refresh_deadline().unwrap();

    list.insert(0, 2, 10, vec![10, 11]);
    assert_eq!(list.get(0), Some(&10));
    assert_eq!(list.selected(), Some(&10));
    assert_eq!(list.display(0), Some(&10));
    assert_eq!(list.display(2), Some(&2));
    assert_eq!(list.get(2), None);
    assert_eq!(list.next_offset(2), Some(2));
    assert_eq!(list.refresh_deadline(), Some(deadline));
    let mut rows = list.display_iter().copied().collect::<Vec<_>>();
    rows.sort_unstable();
    assert_eq!(rows, vec![2, 3, 10, 11]);

    assert!(list.expire_refresh(deadline));
    assert_eq!(list.display(0), Some(&10));
    assert_eq!(list.display(2), None);
    list.insert(2, 2, 10, vec![2, 3]);
    assert_eq!(list.display(2), Some(&2));
    assert_eq!(list.get(2), Some(&2));
    assert!(list.retained.is_none());
}

#[test]
fn retention_is_bounded_to_visible_pages_and_the_visible_slice() {
    let mut list = PagedList::default();
    list.set_viewport(2);
    list.selection.select(Some(3));
    *list.selection.offset_mut() = 3;
    list.insert(2, 2, 10, vec![2, 3]);
    list.insert(4, 2, 10, vec![4, 5]);
    list.insert(0, 2, 10, vec![0, 1]);
    list.insert(6, 2, 10, vec![6, 7]);
    list.invalidate();

    assert_eq!(list.retained.as_ref().unwrap().pages.len(), 2);
    assert_eq!(list.display(2), None);
    assert_eq!(list.display(3), Some(&3));
    assert_eq!(list.display(4), Some(&4));
    assert_eq!(list.display(5), None);
    let mut rows = list.display_iter().copied().collect::<Vec<_>>();
    rows.sort_unstable();
    assert_eq!(rows, vec![3, 4]);
}

#[test]
fn destructive_resets_and_explicit_discard_remove_retained_display() {
    let mut list = loaded_list();
    list.invalidate_pages();
    list.clear_pages();
    assert_eq!(list.display(0), None);
    assert_eq!(list.refresh_deadline(), None);
    assert_eq!(list.total, 10);
    assert!(list.loaded);

    let mut list = loaded_list();
    list.invalidate();
    list.clear();
    assert_eq!(list.display_total(), 0);
    assert!(!list.display_loaded());
    assert_eq!(list.fetch_size, 0);
    assert_eq!(list.refresh_deadline(), None);

    let mut list = loaded_list();
    list.invalidate_pages();
    list.insert(0, 2, 10, vec![10, 11]);
    list.discard_refresh();
    assert_eq!(list.get(0), Some(&10));
    assert_eq!(list.display(0), Some(&10));
    assert_eq!(list.display(2), None);
    assert_eq!(list.refresh_deadline(), None);
}

#[test]
fn navigation_and_viewport_or_page_size_changes_discard_retention() {
    let mut list = loaded_list();
    list.invalidate_pages();
    assert!(list.navigate_action(Action::Down, 4));
    assert_eq!(list.display(0), None);
    assert_eq!(list.refresh_deadline(), None);

    let mut list = loaded_list();
    list.invalidate();
    assert!(list.navigate_action(Action::Down, 4));
    assert_eq!(list.display(0), None);
    assert_eq!(list.refresh_deadline(), None);

    let mut list = loaded_list();
    list.invalidate();
    list.set_viewport(4);
    assert!(list.refresh_deadline().is_some());
    list.set_viewport(3);
    assert_eq!(list.refresh_deadline(), None);

    let mut list = loaded_list();
    list.invalidate();
    list.selection.select(Some(2));
    *list.selection.offset_mut() = 2;
    list.set_viewport(4);
    assert_eq!(list.refresh_deadline(), None);

    let mut list = loaded_list();
    list.invalidate();
    list.insert(0, 3, 10, vec![10, 11, 12]);
    assert_eq!(list.refresh_deadline(), None);
    assert_eq!(list.display(2), Some(&12));
    assert_eq!(list.display(3), None);
}

fn identity_refresh() -> PagedList<usize> {
    let mut list = PagedList::default();
    list.set_viewport(2);
    list.selection.select(Some(8));
    *list.selection.offset_mut() = 8;
    list.insert(8, 2, 12, vec![8, 9]);
    list.invalidate_identity(Some(ContainerIdentity::OtherTracks));
    list
}

#[test]
fn identity_scan_keeps_display_coordinates_without_authorizing_the_scan_selection() {
    let mut list = identity_refresh();
    let deadline = list.refresh_deadline().unwrap();
    assert_eq!(list.selection.selected(), Some(0));
    assert_eq!(list.selection.offset(), 0);
    assert_eq!(list.display_selected(), Some(8));
    assert_eq!(list.display_offset(), 8);
    assert_eq!(list.display_visible_end(), 10);
    assert_eq!(list.next_offset(2), Some(0));
    assert!(list.resolve_identity(0, 12, &[0, 1], |item, _| *item == 8));
    list.insert(0, 2, 12, vec![0, 1]);
    assert_eq!(list.get(0), Some(&0));
    assert_eq!(list.selected(), None);
    assert_eq!(list.display_selected(), Some(8));
    assert_eq!(list.display_offset(), 8);
    assert_eq!(list.display(8), Some(&8));
    list.invalidate_identity(Some(ContainerIdentity::OtherTracks));
    assert_eq!(list.refresh_deadline(), Some(deadline));
    assert_eq!(list.display_offset(), 8);
    assert_eq!(list.display(8), Some(&8));
    assert!(list.resolve_identity(0, 12, &[0, 1], |item, _| *item == 8));
    list.insert(0, 2, 12, vec![0, 1]);

    assert!(list.expire_refresh(deadline));
    assert_eq!(list.display_selected(), Some(0));
    assert_eq!(list.display_offset(), 0);
    assert_eq!(list.display(0), Some(&0));
    assert_eq!(list.selected(), Some(&0));
}

#[test]
fn resolved_identity_and_destructive_identity_changes_release_old_display() {
    let mut list = identity_refresh();
    assert!(list.resolve_identity(8, 12, &[8, 9], |item, _| *item == 8));
    list.insert(8, 2, 12, vec![8, 9]);
    assert_eq!(list.display_offset(), 8);
    assert_eq!(list.display_selected(), Some(8));
    assert_eq!(list.selected(), Some(&8));
    assert_eq!(list.refresh_deadline(), None);

    let mut list = identity_refresh();
    list.retain_identity(Some(ContainerIdentity::OtherTracks));
    assert_eq!(list.display(8), None);
    assert_eq!(list.refresh_deadline(), None);
    assert!(!list.display_loaded());
}

#[test]
fn a_fresh_empty_reply_immediately_replaces_retained_rows_and_total() {
    let mut list = loaded_list();
    list.invalidate();
    list.insert(0, 2, 0, Vec::new());
    assert!(list.loaded);
    assert!(list.display_loaded());
    assert_eq!(list.total, 0);
    assert_eq!(list.display_total(), 0);
    assert_eq!(list.display_visible_end(), 0);
    assert_eq!(list.display(0), None);
    assert_eq!(list.selected(), None);
    assert_eq!(list.next_offset(2), None);
    assert_eq!(list.refresh_deadline(), None);
}
