//! Characterisation tests (Phase 2 of the full review) of what the viewer sends to
//! the server as the device to connect to (`signal::target`, `signal::valid_target`)
//! and of the decoding of signalling messages from arbitrary bytes.

use nuvira_proto::v1::{ClientMessage, ServerMessage};
use nuvira_transport::signal::{target, valid_target};
use proptest::prelude::*;
use prost::Message;

#[test]
fn target_edge_cases() {
    assert_eq!(target(""), "");
    assert!(!valid_target(""));
    assert_eq!(target(" 123.456.789 "), "123456789");
    assert_eq!(target("- . -"), "");
    // Digits other than ASCII are not digits here: the text is kept, lower case.
    assert_eq!(target("١٢٣"), "١٢٣");
    assert!(!valid_target("١٢٣"));
    // Aliases: double dashes inside are allowed, at the ends not.
    assert!(valid_target("a--b"));
    assert!(valid_target("123-abc"));
    assert!(valid_target("abc"));
    assert!(!valid_target("ab"));
    assert!(valid_target(&"a".repeat(32)));
    assert!(!valid_target(&"a".repeat(33)));
    assert!(!valid_target("abc-"));
    assert!(!valid_target("ab c"));
    assert!(!valid_target("ABC"), "valid_target expects target()'s lower case");
    assert_eq!(target("ABC"), "abc");
    // Unicode lower case that is not ASCII: refused.
    assert!(!valid_target(&target("İstanbul")));
    // 10 digits: not an ID, and not an alias (no letter).
    assert!(!valid_target("1234567890"));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, ..ProptestConfig::default() })]

    #[test]
    fn target_never_panics(s in ".{0,60}") {
        let t = target(&s);
        let _ = valid_target(&t);
    }

    #[test]
    fn valid_targets_are_ids_or_ascii_aliases(s in ".{0,40}") {
        let t = target(&s);
        if valid_target(&t) {
            prop_assert!(t.is_ascii());
            let digits = t.bytes().all(|b| b.is_ascii_digit());
            prop_assert!((digits && t.len() == 9) || (!digits && (3..=32).contains(&t.len())));
        }
    }

    #[test]
    fn target_is_idempotent_on_ascii(s in "[ -~]{0,40}") {
        let t = target(&s);
        prop_assert_eq!(target(&t), t);
    }

    #[test]
    fn typed_ids_with_separators(d in "[0-9]{9}", seps in prop::collection::vec("[ .-]{0,2}", 9)) {
        let typed: String = d.chars().zip(seps).map(|(c, s)| format!("{c}{s}")).collect();
        prop_assert_eq!(target(&format!(" {typed} ")), d.clone());
        prop_assert!(valid_target(&d));
    }

    /// Signalling messages from arbitrary bytes: decoding never panics.
    #[test]
    fn signalling_messages_from_arbitrary_bytes(bytes in prop::collection::vec(any::<u8>(), 0..300)) {
        let _ = ServerMessage::decode(bytes.as_slice());
        let _ = ClientMessage::decode(bytes.as_slice());
    }
}
