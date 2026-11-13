//! Matching what's typed against apps: whole-word and prefix matches on the name count most,
//! then matches in the generic name, keywords and program, then loose in-order letters.

use crate::apps::App;

/// How well `query` matches `app`, or None if it doesn't. Every word of the query has to
/// match somewhere.
pub fn score(query: &str, app: &App) -> Option<i32> {
    let name = app.name.to_lowercase();
    let generic = app.generic.to_lowercase();
    let comment = app.comment.to_lowercase();
    let program = app
        .exec
        .split_whitespace()
        .find(|w| *w != "env" && !w.contains('='))
        .and_then(|p| p.rsplit('/').next())
        .unwrap_or_default()
        .to_lowercase();
    let keywords: Vec<String> = app.keywords.iter().map(|k| k.to_lowercase()).collect();
    let mut total = 0;
    for word in query.split_whitespace() {
        let w = word.to_lowercase();
        let s = if name == w {
            1000
        } else if name.starts_with(&w) {
            800
        } else if name
            .split(|c: char| !c.is_alphanumeric())
            .any(|part| part.starts_with(&w))
        {
            600
        } else if name.contains(&w) {
            450
        } else if program.starts_with(&w) {
            350
        } else if generic.split_whitespace().any(|g| g.starts_with(&w)) || keywords.iter().any(|k| k.starts_with(&w)) {
            300
        } else if let Some(s) = subsequence(&w, &name) {
            s
        } else if generic.contains(&w) || keywords.iter().any(|k| k.contains(&w)) || program.contains(&w) {
            150
        } else if w.len() >= 3 && comment.contains(&w) {
            60
        } else {
            return None;
        };
        total += s;
    }
    Some(total)
}

/// The letters of `w` in order in `s` ("ffx" in "firefox"): 100-250, tighter is better.
fn subsequence(w: &str, s: &str) -> Option<i32> {
    if w.chars().count() < 2 {
        return None;
    }
    let mut it = s.char_indices();
    let mut first = None;
    let mut last = 0;
    for c in w.chars() {
        let (i, _) = it.find(|(_, x)| *x == c)?;
        first.get_or_insert(i);
        last = i;
    }
    let span = (last - first.unwrap_or(0)) as i32 + 1;
    let gaps = span - w.chars().count() as i32;
    Some((250 - gaps * 15).max(100))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, exec: &str, keywords: &[&str]) -> App {
        App {
            id: format!("{name}.desktop"),
            name: name.into(),
            generic: String::new(),
            comment: String::new(),
            keywords: keywords.iter().map(|k| k.to_string()).collect(),
            exec: exec.into(),
            icon: String::new(),
            terminal: false,
        }
    }

    #[test]
    fn ranking() {
        let ff = app("Firefox Web Browser", "firefox %u", &["internet", "www"]);
        let files = app("Files", "nautilus", &["folder"]);
        assert!(score("fire", &ff) > score("www", &ff));
        assert!(score("web", &ff).is_some());
        assert_eq!(score("nautilus", &files), Some(350));
        assert!(score("ffx", &ff).is_some());
        assert!(score("zzz", &ff).is_none());
        // Every word must match.
        assert!(score("fire folder", &ff).is_none());
        assert!(score("files", &files) > score("fi", &files).map(|s| s - 1));
    }
}
