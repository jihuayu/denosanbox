#![no_main]
use denosanbox_policy::{Frame, Policy, VirtualPath};
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let _ = Frame::decode(data, 4096);
    if let Ok(path) = std::str::from_utf8(data) {
        let _ = VirtualPath::parse(path);
    }
    let roots = ["/", "/a", "/a/b", "/a/bc", "/private", "/tmp"];
    let mut a = Policy::default();
    let mut b = Policy::default();
    for (index, byte) in data.iter().take(roots.len()).enumerate() {
        if byte & 1 != 0 {
            a = a.allow_read(roots[index]).unwrap();
        }
        if byte & 2 != 0 {
            b = b.allow_read(roots[index]).unwrap();
        }
    }
    let result = a.intersect(&b);
    assert_eq!(result, b.intersect(&a));
    assert!(result.is_subset_of(&a) && result.is_subset_of(&b));
});
