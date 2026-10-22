//! Pins the tunnel's media/QUIC rule to the core's. The tunnel does not depend on
//! `punktfunk-core` (its `shared` module needs the whole quinn/aws-lc stack behind the `quic`
//! feature), so this test reads the core's source and fails if the one-liner it copied changes.

use pf_usb_tunnel::frame::is_media;

#[test]
fn core_is_media_is_still_the_rule_we_copied() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../core/punktfunk-core/src/transport/shared.rs"
    );
    let src = std::fs::read_to_string(path).expect("core shared.rs");
    let at = src
        .find("pub fn is_media(first_byte: u8) -> bool {")
        .expect("is_media in core");
    let body: String = src[at..].lines().take(3).collect::<Vec<_>>().join("\n");
    assert!(
        body.contains("first_byte & 0xC0 == 0"),
        "core changed is_media:\n{body}"
    );
}

#[test]
fn every_first_byte_is_classified_by_its_top_two_bits() {
    for b in 0..=255u8 {
        assert_eq!(is_media(b), (b >> 6) == 0, "byte {b:#04x}");
    }
}
