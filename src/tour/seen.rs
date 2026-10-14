//! When you last toured each review, for the inbox to tell what is new since
//! you looked. Kept in `.git/stk/tours/seen.json` as review id to seconds
//! since the epoch.

use std::collections::BTreeMap;

use crate::git;

fn path() -> Option<std::path::PathBuf> {
    git::git_common_path_absolute("stk/tours/seen.json").ok()
}

/// When each review was last toured.
pub fn read() -> BTreeMap<String, i64> {
    path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Note that `ids` were toured now. Best effort: a tour that cannot note it
/// is still a tour.
pub fn record<'a>(ids: impl IntoIterator<Item = &'a str>) {
    let Some(path) = path() else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    let mut seen = read();
    for id in ids {
        seen.insert(id.to_owned(), now);
    }
    if let (Some(dir), Ok(text)) = (path.parent(), serde_json::to_string(&seen)) {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(path, text);
    }
}
