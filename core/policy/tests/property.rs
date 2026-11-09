//! Deterministic adversarial corpus: seed and iteration counts are reproducible.
use denosanbox_policy::{Frame, FrameKind, Policy, VirtualPath};

#[test]
fn malformed_paths_and_frames_never_panic_or_accept_trailing_bytes() {
    let mut seed = 0x6d2b79f5u64;
    for length in 0..128 {
        for _ in 0..32 {
            let bytes: Vec<u8> = (0..length)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed as u8
                })
                .collect();
            let path = String::from_utf8_lossy(&bytes);
            let _ = VirtualPath::parse(&path);
            let _ = Frame::decode(&bytes, 256);
        }
    }
    let encoded = Frame::new(FrameKind::Request, 7, vec![1, 2, 3])
        .encode(256)
        .unwrap();
    for end in 0..encoded.len() {
        assert!(Frame::decode(&encoded[..end], 256).is_err());
    }
    let mut trailing = encoded;
    trailing.push(0);
    assert!(Frame::decode(&trailing, 256).is_err());
}

#[test]
fn policy_intersection_is_commutative_and_cannot_add_a_permission() {
    let paths = ["/", "/a", "/a/b", "/a/bc", "/b", "/b/c"];
    for first in 0u32..64 {
        for second in 0u32..64 {
            let mut a = Policy::default();
            let mut b = Policy::default();
            for (index, path) in paths.iter().enumerate() {
                if first & (1 << index) != 0 {
                    a = a.allow_read(path).unwrap();
                }
                if second & (1 << index) != 0 {
                    b = b.allow_read(path).unwrap();
                }
            }
            let intersection = a.intersect(&b);
            assert_eq!(intersection, b.intersect(&a));
            assert!(intersection.is_subset_of(&a));
            assert!(intersection.is_subset_of(&b));
            for path in ["/a/b/file", "/a/bc/file", "/b/c/file", "/private"] {
                if intersection.check_read_str(path).is_ok() {
                    assert!(a.check_read_str(path).is_ok() && b.check_read_str(path).is_ok());
                }
            }
        }
    }
}
