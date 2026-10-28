//! Bounds-checked byte reading (shared by codec and btc). All functions return `None` when out of bounds and never
//! panic.

/// Takes `n` bytes from the head of `input` and advances it; returns `None` without changing `input` when fewer than
/// `n` bytes remain.
pub(crate) fn take<'a>(input: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    let (head, tail) = input.split_at_checked(n)?;
    *input = tail;
    Some(head)
}

/// Takes a fixed-length array from the head of `input`.
pub(crate) fn take_array<const N: usize>(input: &mut &[u8]) -> Option<[u8; N]> {
    take(input, N)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_is_bounds_checked() {
        let data = [1u8, 2, 3];
        let mut input = &data[..];
        assert_eq!(take(&mut input, 4), None);
        assert_eq!(input, &data[..], "failed take must not consume");
        assert_eq!(take(&mut input, 2), Some(&data[..2]));
        assert_eq!(take_array::<2>(&mut input), None);
        assert_eq!(take_array::<1>(&mut input), Some([3]));
        assert_eq!(take(&mut input, 0), Some(&[][..]));
        assert!(input.is_empty());
    }
}
