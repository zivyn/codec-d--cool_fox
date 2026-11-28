/// The trail keeps the newest things, and the report made when the
/// host device is removed says them and the pipeline library's state
/// (no card is asked for any of it).
#[test]
fn the_removal_report_says_the_newest_things_done() {
    for n in 0..super::TRAIL_MOST + 5 {
        super::trail(|| format!("thing {n} done"));
    }
    let text = super::removal_context();
    assert!(text.contains("the submitting thread was"));
    assert!(text.contains("pipeline library: no call running"));
    assert!(text.contains(&format!("thing {} done", super::TRAIL_MOST + 4)));
    assert!(!text.contains("thing 4 done"), "the oldest entries are dropped");
}
