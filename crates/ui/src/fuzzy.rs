/// Score `text` against a fuzzy `query` (case-insensitive subsequence match).
/// Returns `None` when `query` is not a subsequence of `text`; a higher score
/// is a better match (consecutive and earlier hits score more). An empty query
/// matches everything with score 0.
pub(crate) fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let mut qi = 0usize;
    let mut score = 0i32;
    let mut last: Option<usize> = None;
    for (ti, tc) in t.iter().enumerate() {
        if qi < q.len() && *tc == q[qi] {
            match last {
                Some(prev) if ti == prev + 1 => score += 10, // consecutive
                Some(_) => score += 1,
                None => score += 5 - (ti.min(5) as i32), // earlier first hit
            }
            last = Some(ti);
            qi += 1;
        }
    }
    (qi == q.len()).then_some(score)
}
