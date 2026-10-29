use super::*;
use rivu_protocol::model::Page;
use std::time::Duration;

fn search(ui: &UiState) -> &LibrarySearch {
    let View::Search(search) = &ui.view else {
        panic!("expected search view");
    };
    search
}

fn search_mut(ui: &mut UiState) -> &mut LibrarySearch {
    let View::Search(search) = &mut ui.view else {
        panic!("expected search view");
    };
    search
}

fn edit(ui: &mut UiState, query: &str, now: Instant) {
    let search = search_mut(ui);
    search.query = query.into();
    if search.filter(now) {
        ui.cancel_query();
    }
}

fn apply_pending(ui: &mut UiState, now: Instant) -> bool {
    let changed = search_mut(ui).apply_pending(now);
    if changed {
        ui.cancel_query();
    }
    changed
}

fn page(request: &QueryRequest, first_id: i64) -> QueryResult {
    let rows = (0..request.limit)
        .map(|index| LibraryRow {
            path: Default::default(),
            id: first_id + index as i64,
            title: format!("track-{}", first_id + index as i64),
            artist: "artist".into(),
            album: "album".into(),
            duration: None,
            codec: "flac".into(),
            channels: 2,
            sample_rate: 44100,
            bits_per_sample: None,
            cue_number: None,
            bitrate_bps: None,
            bitrate_exact: false,
            favorite: false,
            missing: false,
            play_count: 0,
        })
        .collect();
    QueryResult {
        revisions: request.stamp.revisions(),
        view: ViewResponse::LibraryPage(Page { total: 12, rows }),
    }
}

fn fixture() -> (UiState, TuiSnapshot, Instant) {
    let mut state = TuiSnapshot::default();
    state.system.page_size = 2;
    let mut ui = UiState {
        view: View::Search(LibrarySearch::default()),
        ..UiState::default()
    };
    // Requests use the real clock. A future edit clock keeps every request below
    // its debounce deadline without sleeping or depending on test execution speed.
    let now = Instant::now() + Duration::from_secs(86_400);
    edit(&mut ui, "active", now);
    assert!(apply_pending(
        &mut ui,
        now + rivu_protocol::model::SEARCH_DEBOUNCE
    ));
    let request = ui.query_request(&state).unwrap();
    assert_eq!(request.kind, QueryKind::Search("active".into()));
    assert!(ui.apply_query(&state, &request, Ok(page(&request, 100))));
    (ui, state, now + Duration::from_secs(1))
}

fn assert_row(ui: &UiState, index: usize, id: i64) {
    let row = search(ui).results.get(index).expect("expected loaded row");
    assert_eq!(row.id, id);
    assert_eq!(row.title, format!("track-{id}"));
}

#[test]
fn pending_draft_retains_rows_and_accepts_active_query_pages() {
    let (mut ui, state, now) = fixture();
    let active_page = ui.query_request(&state).unwrap();
    assert_eq!(active_page.offset, 2);
    ui.requested = Some(active_page.clone());

    edit(&mut ui, "replacement", now);
    assert_row(&ui, 0, 100);
    assert_eq!(ui.query_request(&state), Some(active_page.clone()));
    assert!(ui.apply_query(&state, &active_page, Ok(page(&active_page, 102))));
    assert_row(&ui, 0, 100);
    assert_row(&ui, 2, 102);

    assert!(apply_pending(
        &mut ui,
        now + rivu_protocol::model::SEARCH_DEBOUNCE
    ));
    assert!(search(&ui).results.get(0).is_none());
    let replacement = ui.query_request(&state).unwrap();
    assert_eq!(replacement.kind, QueryKind::Search("replacement".into()));
    assert_eq!(replacement.offset, 0);
    assert!(ui.apply_query(&state, &replacement, Ok(page(&replacement, 200))));
    assert_row(&ui, 0, 200);
}

#[test]
fn nonempty_edits_restart_the_tail_deadline_and_trim_the_applied_query() {
    let (mut ui, state, now) = fixture();
    let debounce = rivu_protocol::model::SEARCH_DEBOUNCE;
    edit(&mut ui, "next", now);
    let second_edit = now + debounce / 2;
    edit(&mut ui, "  next album\t", second_edit);

    assert!(!apply_pending(&mut ui, now + debounce));
    assert_row(&ui, 0, 100);
    assert_eq!(
        ui.query_request(&state).unwrap().kind,
        QueryKind::Search("active".into())
    );
    assert!(!apply_pending(
        &mut ui,
        second_edit + debounce - Duration::from_nanos(1)
    ));
    assert!(apply_pending(&mut ui, second_edit + debounce));
    let request = ui.query_request(&state).unwrap();
    assert_eq!(request.kind, QueryKind::Search("next album".into()));
    assert_eq!(request.offset, 0);
    assert!(ui.apply_query(&state, &request, Ok(page(&request, 300))));
    assert_row(&ui, 0, 300);
}

#[test]
fn clear_and_whitespace_immediately_apply_empty_and_cancel_pending_drafts() {
    for cleared in ["", " \t\u{2003}\n"] {
        let (mut ui, state, now) = fixture();
        let old = ui.query_request(&state).unwrap();
        edit(&mut ui, "pending", now);
        edit(&mut ui, cleared, now + Duration::from_millis(1));
        let empty = ui.query_request(&state).unwrap();
        assert_eq!(empty.kind, QueryKind::Search(String::new()));
        assert_eq!(empty.offset, 0);
        assert!(search(&ui).results.get(0).is_none());
        assert!(!ui.apply_query(&state, &old, Ok(page(&old, 102))));
        assert!(ui.apply_query(&state, &empty, Ok(page(&empty, 400))));
        assert_row(&ui, 0, 400);
        assert!(!apply_pending(&mut ui, now + Duration::from_secs(1)));
        assert_row(&ui, 0, 400);

        // An already empty effective query must keep its loaded browse results.
        edit(
            &mut ui,
            "another pending draft",
            now + Duration::from_secs(2),
        );
        edit(&mut ui, cleared, now + Duration::from_secs(2));
        assert_row(&ui, 0, 400);
        assert!(!apply_pending(&mut ui, now + Duration::from_secs(3)));
        assert_row(&ui, 0, 400);
    }
}

#[test]
fn editing_back_to_active_query_keeps_rows_and_active_completion_valid() {
    let (mut ui, state, now) = fixture();
    let active_page = ui.query_request(&state).unwrap();
    edit(&mut ui, "different", now);
    edit(&mut ui, "  active  ", now + Duration::from_millis(50));
    assert!(!apply_pending(&mut ui, now + Duration::from_secs(1)));
    assert_row(&ui, 0, 100);
    assert_eq!(ui.query_request(&state), Some(active_page.clone()));
    assert!(ui.apply_query(&state, &active_page, Ok(page(&active_page, 102))));
    assert_row(&ui, 2, 102);
}

#[test]
fn applied_query_change_rejects_old_completion_even_after_returning_to_same_text() {
    let (mut ui, state, now) = fixture();
    let old = ui.query_request(&state).unwrap();
    edit(&mut ui, "other", now);
    assert!(apply_pending(
        &mut ui,
        now + rivu_protocol::model::SEARCH_DEBOUNCE
    ));
    assert!(!ui.apply_query(&state, &old, Ok(page(&old, 900))));

    let later = now + Duration::from_secs(1);
    edit(&mut ui, "active", later);
    assert!(apply_pending(
        &mut ui,
        later + rivu_protocol::model::SEARCH_DEBOUNCE
    ));
    let first = ui.query_request(&state).unwrap();
    assert!(ui.apply_query(&state, &first, Ok(page(&first, 500))));
    let fresh = ui.query_request(&state).unwrap();
    assert_eq!(fresh.kind, old.kind);
    assert_eq!(fresh.offset, old.offset);
    assert_ne!(fresh.generation, old.generation);
    assert!(!ui.apply_query(&state, &old, Ok(page(&old, 900))));
    assert_row(&ui, 0, 500);
    assert!(search(&ui).results.get(2).is_none());
    assert!(ui.apply_query(&state, &fresh, Ok(page(&fresh, 502))));
    assert_row(&ui, 2, 502);
}

#[test]
fn pending_draft_does_not_allow_stale_library_revision_results() {
    let (mut ui, mut state, now) = fixture();
    let old = ui.query_request(&state).unwrap();
    edit(&mut ui, "pending", now);
    state.library.revision += 1;
    assert!(!ui.apply_query(&state, &old, Ok(page(&old, 900))));
    assert_row(&ui, 0, 100);
    assert!(search(&ui).results.get(2).is_none());
    let fresh = ui.query_request(&state).unwrap();
    assert_eq!(fresh.kind, QueryKind::Search("active".into()));
    assert!(ui.apply_query(&state, &fresh, Ok(page(&fresh, 102))));
    assert_row(&ui, 2, 102);
}
