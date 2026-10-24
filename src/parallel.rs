/// Requests in flight at once when fetching for several items.
const BATCH: usize = 8;

/// Apply `f` to every item a few at a time on scoped threads, returning results in item order.
pub fn map<I: Sync, T: Send>(items: &[I], f: impl Fn(&I) -> T + Sync) -> Vec<T> {
    let mut out = Vec::with_capacity(items.len());
    for batch in items.chunks(BATCH) {
        std::thread::scope(|s| {
            let handles: Vec<_> = batch
                .iter()
                .map(|item| {
                    let f = &f;
                    s.spawn(move || f(item))
                })
                .collect();
            out.extend(handles.into_iter().map(|h| h.join().unwrap()));
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_item_order_across_batches() {
        let items: Vec<u32> = (0..20).collect();
        assert_eq!(
            map(&items, |n| n * 2),
            (0..20).map(|n| n * 2).collect::<Vec<_>>()
        );
        assert!(map(&[] as &[u32], |n| *n).is_empty());
    }
}
