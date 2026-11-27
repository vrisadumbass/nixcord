//! Store folding of poll votes (MESSAGE_POLL_VOTE_ADD / _REMOVE) and poll updates.
//! Invented data.

use super::*;
use nixcord_model::{MessageUpdate, Poll, PollAnswer, Ready};

const ME: u64 = 10;
const CH: u64 = 20;
const MSG: u64 = 100;

fn sf(n: u64) -> Snowflake {
    Snowflake(n)
}

/// A two-answer poll. `results`: `None` = no `results` object, else (counts, me_voted)
/// per answer and whether it is finalized.
fn poll(results: Option<([(u32, bool); 2], bool)>) -> Box<Poll> {
    let (c, finalized) = results.unwrap_or_default();
    let answers = ["Tea", "Coffee"]
        .iter()
        .zip(c)
        .enumerate()
        .map(|(i, (text, (count, me_voted)))| PollAnswer {
            id: i as u32 + 1,
            text: (*text).into(),
            emoji: None,
            count,
            me_voted,
        })
        .collect();
    Box::new(Poll {
        question: "Tea or coffee?".into(),
        answers,
        finalized,
        has_results: results.is_some(),
        ..Default::default()
    })
}

fn store(results: Option<([(u32, bool); 2], bool)>) -> Store {
    let mut s = Store::new();
    s.apply(&GatewayEvent::Ready(Box::new(Ready {
        user: User {
            id: sf(ME),
            username: "me".into(),
            ..Default::default()
        },
        session_id: "s".into(),
        ..Default::default()
    })));
    let m = Message {
        id: sf(MSG),
        channel_id: sf(CH),
        author: User {
            id: sf(9),
            username: "asker".into(),
            ..Default::default()
        },
        timestamp: "2024-01-01T00:00:00Z".into(),
        poll: Some(poll(results)),
        ..Default::default()
    };
    s.insert_history(sf(CH), vec![m]);
    s
}

fn vote(s: &mut Store, user: u64, answer: u32, add: bool) {
    let (channel_id, message_id, user_id, answer_id) = (sf(CH), sf(MSG), sf(user), answer);
    s.apply(&if add {
        GatewayEvent::PollVoteAdd {
            channel_id,
            message_id,
            user_id,
            answer_id,
        }
    } else {
        GatewayEvent::PollVoteRemove {
            channel_id,
            message_id,
            user_id,
            answer_id,
        }
    });
}

fn counts(s: &Store) -> Vec<(u32, bool)> {
    s.messages(sf(CH))[0]
        .poll
        .as_ref()
        .unwrap()
        .answers
        .iter()
        .map(|a| (a.count, a.me_voted))
        .collect()
}

#[test]
fn votes_fold_into_counts_and_me_voted() {
    let mut s = store(None);
    vote(&mut s, 9, 1, true);
    vote(&mut s, 11, 2, true);
    assert_eq!(counts(&s), vec![(1, false), (1, false)]);
    vote(&mut s, ME, 2, true);
    assert_eq!(counts(&s), vec![(1, false), (2, true)]);
    // Echo of my own vote (e.g. after a refetch already counted it).
    vote(&mut s, ME, 2, true);
    assert_eq!(counts(&s), vec![(1, false), (2, true)]);
    vote(&mut s, ME, 2, false);
    assert_eq!(counts(&s), vec![(1, false), (1, false)]);
    vote(&mut s, 9, 1, false);
    vote(&mut s, 9, 1, false); // clamps at zero
    assert_eq!(counts(&s), vec![(0, false), (1, false)]);
    vote(&mut s, 9, 42, true); // unknown answer
    assert_eq!(counts(&s), vec![(0, false), (1, false)]);
}

#[test]
fn votes_for_messages_not_in_memory_are_ignored() {
    let mut s = store(None);
    s.apply(&GatewayEvent::PollVoteAdd {
        channel_id: sf(CH),
        message_id: sf(MSG + 1),
        user_id: sf(9),
        answer_id: 1,
    });
    s.apply(&GatewayEvent::PollVoteAdd {
        channel_id: sf(CH + 1),
        message_id: sf(MSG),
        user_id: sf(9),
        answer_id: 1,
    });
    assert_eq!(counts(&s), vec![(0, false), (0, false)]);
    assert!(s.messages(sf(CH + 1)).is_empty());
}

#[test]
fn message_update_finalizes_and_keeps_my_vote() {
    let mut s = store(Some(([(2, true), (0, false)], false)));
    // A broadcast update may not say which answers I voted for.
    s.apply(&GatewayEvent::MessageUpdate(Box::new(MessageUpdate {
        id: sf(MSG),
        channel_id: sf(CH),
        poll: Some(poll(Some(([(3, false), (1, false)], true)))),
        ..Default::default()
    })));
    let p = s.messages(sf(CH))[0].poll.clone().unwrap();
    assert!(p.finalized);
    assert_eq!(counts(&s), vec![(3, true), (1, false)]);
    // An update without results keeps the counts.
    s.apply(&GatewayEvent::MessageUpdate(Box::new(MessageUpdate {
        id: sf(MSG),
        channel_id: sf(CH),
        poll: Some(poll(None)),
        ..Default::default()
    })));
    assert_eq!(counts(&s), vec![(3, true), (1, false)]);
    assert!(s.messages(sf(CH))[0].poll.as_ref().unwrap().finalized);
}
