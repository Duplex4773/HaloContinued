//! Bounded byte formatting without per-byte temporary strings.
pub(crate) fn encode(bytes: &[u8], uppercase: bool, spaced: bool) -> String {
    let digits = if uppercase {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut text = String::with_capacity(
        bytes.len() * 2
            + if spaced {
                bytes.len().saturating_sub(1)
            } else {
                0
            },
    );
    for (index, byte) in bytes.iter().copied().enumerate() {
        if spaced && index != 0 {
            text.push(' ');
        }
        text.push(digits[usize::from(byte >> 4)] as char);
        text.push(digits[usize::from(byte & 15)] as char);
    }
    text
}
