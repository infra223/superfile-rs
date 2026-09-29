//! fzf-style fuzzy subsequence scoring used by the search bar and sidebar.
//!
//! Returns 0 when the query does not appear as a subsequence, otherwise a
//! positive-ish score where larger means better (word-boundary matches,
//! consecutive matches and camelCase boundaries are rewarded).

pub fn score(query: &str, text: &str) -> i32 {
    let q: Vec<char> = query.chars().collect();
    let t: Vec<char> = text.chars().collect();
    if q.is_empty() {
        return 0;
    }
    if t.len() < q.len() {
        return 0;
    }
    let mut qi = 0usize;
    let mut total: i32 = 0;
    let mut prev: Option<usize> = None;
    for (ti, &tc) in t.iter().enumerate() {
        if qi >= q.len() {
            break;
        }
        if tc.to_ascii_lowercase() == q[qi].to_ascii_lowercase()
            || tc == q[qi]
        {
            let bonus = if is_boundary(&t, ti) {
                8
            } else if is_camel(&t, ti) {
                6
            } else if prev == Some(ti.wrapping_sub(1)) {
                4
            } else {
                1
            };
            let gap: i32 = match prev {
                Some(p) => (ti - p).saturating_sub(1) as i32,
                None => ti as i32,
            };
            total += bonus - (gap / 2).max(0);
            prev = Some(ti);
            qi += 1;
        }
    }
    if qi < q.len() {
        return 0;
    }
    total
}

fn is_boundary(t: &[char], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let p = t[i - 1];
    p == '_' || p == '-' || p == '.' || p == '/' || p == ' ' || p == '(' || p == ')'
}

fn is_camel(t: &[char], i: usize) -> bool {
    if i == 0 || i + 1 >= t.len() {
        return false;
    }
    t[i - 1].is_ascii_lowercase() && t[i].is_ascii_uppercase()
}

/// Keep items whose fuzzy score is > 0, ordered by score (desc), then name (asc).
pub fn filter_scored<'a, T>(query: &str, items: &'a [T], name: &dyn Fn(&T) -> String) -> Vec<&'a T> {
    if query.is_empty() {
        return items.iter().collect();
    }
    let mut scored: Vec<(i32, usize, &T)> = items
        .iter()
        .enumerate()
        .map(|(i, it)| (score(query, &name(it)), i, it))
        .filter(|(s, _, _)| *s > 0)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, _, it)| it).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_subsequence() {
        assert!(score("ab", "abc") > 0);
        assert_eq!(score("xyz", "abc"), 0);
        assert!(score("abc", "abc") > score("abc", "axbxc"));
    }

    #[test]
    fn word_boundary_bonus() {
        assert!(score("f", "file manager") > score("f", "affair"));
    }
}
