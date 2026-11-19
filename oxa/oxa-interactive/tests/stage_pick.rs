//! The shuffle bag, the key fallbacks and the sticky pick (animations design 3.2, steps 2 to 5).
//! Public API only; no clock, no terminal.
use oxa_interactive::skin::stage::pick::{first_with_members, key_chain, Bag, Sticky};

type Member = (&'static str, u32);

fn pool() -> Vec<Member> {
    vec![("a", 1), ("b", 1), ("c", 1), ("d", 1)]
}

fn draw(bag: &mut Bag<&'static str>, members: &[Member], n: usize) -> Vec<&'static str> {
    (0..n)
        .map(|_| bag.next(members).expect("the pool has members"))
        .collect()
}

#[test]
fn nothing_repeats_until_the_pool_is_spent() {
    let members = pool();
    let mut bag = Bag::new("session-1", "think");
    for _ in 0..3 {
        let mut round = draw(&mut bag, &members, 4);
        round.sort();
        assert_eq!(round, vec!["a", "b", "c", "d"]);
    }
}

#[test]
fn a_refill_never_starts_with_the_last_pick() {
    let members = pool();
    for session in ["s1", "s2", "s3", "s4", "s5", "s6", "s7", "s8"] {
        let mut bag = Bag::new(session, "tool");
        let picks = draw(&mut bag, &members, 40);
        for round in 1..10 {
            assert_ne!(
                picks[round * 4],
                picks[round * 4 - 1],
                "session {session}: refill {round} starts with the last pick"
            );
        }
    }
}

#[test]
fn same_seed_same_sequence() {
    let members = pool();
    let mut first = Bag::new("session-7", "think");
    let mut second = Bag::new("session-7", "think");
    assert_eq!(draw(&mut first, &members, 40), draw(&mut second, &members, 40));
}

#[test]
fn fallback_from_tool_bash_to_tool() {
    assert_eq!(key_chain("tool:bash"), vec!["tool:bash", "tool"]);
    assert_eq!(key_chain("wait:ci"), vec!["wait:ci", "wait", "tool"]);
    assert_eq!(
        key_chain("ci:green_after_red"),
        vec!["ci:green_after_red", "ci:green", "done"]
    );
    assert_eq!(key_chain("think"), vec!["think"]);
    let pools = |key: &str| key == "tool";
    assert_eq!(first_with_members("tool:bash", pools).as_deref(), Some("tool"));
    assert_eq!(first_with_members("think", |_| false), None);
}

#[test]
fn weights_put_classics_in_twice() {
    let members: Vec<Member> = vec![("classic", 2), ("new", 1), ("other", 1)];
    let mut bag = Bag::new("session-3", "think");
    let round = draw(&mut bag, &members, 4);
    assert_eq!(round.iter().filter(|&&x| x == "classic").count(), 2);
    assert_eq!(round.iter().filter(|&&x| x == "new").count(), 1);
    assert_eq!(round.iter().filter(|&&x| x == "other").count(), 1);
}

#[test]
fn weighted_picks_avoid_adjacency_inside_and_across_refills() {
    let members = [("classic", 2), ("new", 1), ("other", 1)];
    let mut bag = Bag::new("session-1", "think");
    let picks = draw(&mut bag, &members, 40);
    assert!(picks.windows(2).all(|pair| pair[0] != pair[1]), "{picks:?}");
    for round in picks.chunks_exact(4) {
        assert_eq!(round.iter().filter(|&&item| item == "classic").count(), 2);
        assert!(round.contains(&"new") && round.contains(&"other"));
    }
}

#[test]
fn unavoidable_repeats_keep_all_weighted_copies() {
    let members = [("classic", 1), ("new", 1), ("classic", 2)];
    let mut bag = Bag::new("session-1", "think");
    assert_eq!(
        draw(&mut bag, &members, 4),
        ["classic", "new", "classic", "classic"]
    );
}

#[test]
fn an_empty_pool_gives_no_pick() {
    let members: Vec<Member> = vec![("gone", 0)];
    let mut bag = Bag::new("session-4", "think");
    assert_eq!(bag.next(&members), None);
}

#[test]
fn a_key_keeps_its_pick_until_its_budget_is_spent() {
    let mut sticky: Sticky<u32> = Sticky::default();
    assert_eq!(sticky.hold(1, "think", false, || 10), 10);
    assert_eq!(sticky.hold(1, "think", false, || 99), 10);
    assert_eq!(sticky.hold(1, "think", true, || 99), 99);
    assert_eq!(sticky.hold(2, "think", false, || 5), 5);
}
