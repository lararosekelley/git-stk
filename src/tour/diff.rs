//! A parsed `git diff` patch: files, their hunks, and each hunk's lines.

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
}

impl FileStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::Modified => "modified",
            Self::Renamed => "renamed",
            Self::Copied => "copied",
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FileDiff {
    pub old_path: String,
    pub new_path: String,
    pub status: FileStatus,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// The path the file has after the change, or before it for a deletion.
    pub fn path(&self) -> &str {
        match self.status {
            FileStatus::Deleted => &self.old_path,
            _ => &self.new_path,
        }
    }

    pub fn additions(&self) -> usize {
        self.count(LineKind::Added)
    }

    pub fn deletions(&self) -> usize {
        self.count(LineKind::Deleted)
    }

    fn count(&self, kind: LineKind) -> usize {
        self.hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .filter(|line| line.kind == kind)
            .count()
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// The function or section name git prints after the range, if any.
    pub section: String,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum LineKind {
    Context,
    Added,
    Deleted,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Line {
    pub kind: LineKind,
    pub text: String,
}

/// Parse a patch from `git diff` run with `a/`/`b/` prefixes and
/// `core.quotePath=false`. Paths come from `---`/`+++` and `rename`/`copy`
/// lines where present, since the `diff --git` header is ambiguous for paths
/// containing spaces.
pub fn parse(patch: &str) -> Vec<FileDiff> {
    let mut files = Vec::new();
    let mut current: Option<FileDiff> = None;

    for line in patch.lines() {
        if let Some(header) = line.strip_prefix("diff --git ") {
            files.extend(current.take());
            let path = header_path(header).unwrap_or_default();
            current = Some(FileDiff {
                old_path: path.clone(),
                new_path: path,
                status: FileStatus::Modified,
                binary: false,
                hunks: Vec::new(),
            });
            continue;
        }
        let Some(file) = current.as_mut() else {
            continue;
        };

        if let Some(hunk) = file.hunks.last_mut()
            && let Some(kind) = line_kind(line)
        {
            hunk.lines.push(Line {
                kind,
                text: line.get(1..).unwrap_or_default().to_owned(),
            });
            continue;
        }

        if let Some(range) = line.strip_prefix("@@ ") {
            if let Some(hunk) = parse_hunk_header(range) {
                file.hunks.push(hunk);
            }
        } else if line.starts_with("new file mode") {
            file.status = FileStatus::Added;
        } else if line.starts_with("deleted file mode") {
            file.status = FileStatus::Deleted;
        } else if let Some(path) = line.strip_prefix("rename from ") {
            file.status = FileStatus::Renamed;
            file.old_path = unquote(path);
        } else if let Some(path) = line.strip_prefix("rename to ") {
            file.new_path = unquote(path);
        } else if let Some(path) = line.strip_prefix("copy from ") {
            file.status = FileStatus::Copied;
            file.old_path = unquote(path);
        } else if let Some(path) = line.strip_prefix("copy to ") {
            file.new_path = unquote(path);
        } else if let Some(path) = line.strip_prefix("--- ") {
            if let Some(path) = side_path(path, "a/") {
                file.old_path = path;
            }
        } else if let Some(path) = line.strip_prefix("+++ ") {
            if let Some(path) = side_path(path, "b/") {
                file.new_path = path;
            }
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            file.binary = true;
        }
    }
    files.extend(current);
    files
}

/// A body line's kind. `\ No newline at end of file` is dropped.
fn line_kind(line: &str) -> Option<LineKind> {
    match line.as_bytes().first() {
        Some(b' ') => Some(LineKind::Context),
        Some(b'+') => Some(LineKind::Added),
        Some(b'-') => Some(LineKind::Deleted),
        // An empty context line can lose its leading space in transit.
        None => Some(LineKind::Context),
        _ => None,
    }
}

/// `-12,3 +14,5 @@ fn name` into a hunk with no lines yet.
fn parse_hunk_header(range: &str) -> Option<Hunk> {
    let (ranges, section) = range.split_once(" @@").unwrap_or((range, ""));
    let (old, new) = ranges.split_once(' ')?;
    let (old_start, old_lines) = parse_range(old.strip_prefix('-')?)?;
    let (new_start, new_lines) = parse_range(new.strip_prefix('+')?)?;
    Some(Hunk {
        old_start,
        old_lines,
        new_start,
        new_lines,
        section: section.trim().to_owned(),
        lines: Vec::new(),
    })
}

/// `12,3` or `12` (one line).
fn parse_range(range: &str) -> Option<(u32, u32)> {
    match range.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((range.parse().ok()?, 1)),
    }
}

/// The path in a `diff --git a/P b/P` header when both sides name the same
/// path - the only case it can be read unambiguously. Other cases are covered
/// by the `---`/`+++` and `rename` lines that follow.
fn header_path(header: &str) -> Option<String> {
    if header.starts_with('"') {
        let (old, rest) = split_quoted(header)?;
        return old.strip_prefix("a/").map(str::to_owned).or_else(|| {
            unquote(rest.trim_start())
                .strip_prefix("b/")
                .map(str::to_owned)
        });
    }
    let inner = header.strip_prefix("a/")?;
    // "a/" + P + " b/" + P
    let half = inner.len().checked_sub(3)? / 2;
    let (path, rest) = (inner.get(..half)?, inner.get(half..)?);
    (rest.strip_prefix(" b/") == Some(path)).then(|| path.to_owned())
}

/// A `---`/`+++` path without its side prefix, or None for `/dev/null`.
fn side_path(raw: &str, prefix: &str) -> Option<String> {
    // Git appends a tab after paths containing spaces.
    let path = unquote(raw.strip_suffix('\t').unwrap_or(raw));
    path.strip_prefix(prefix).map(str::to_owned)
}

/// Split a leading C-quoted string from the rest of `text`.
fn split_quoted(text: &str) -> Option<(String, &str)> {
    let mut escaped = false;
    for (index, ch) in text.char_indices().skip(1) {
        match ch {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => return Some((unquote(&text[..=index]), &text[index + 1..])),
            _ => escaped = false,
        }
    }
    None
}

/// Undo git's C-style quoting of a path (`"a\tb"`), leaving an unquoted path
/// as is.
fn unquote(text: &str) -> String {
    let Some(inner) = text
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return text.to_owned();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let mut chars = inner.bytes().peekable();
    while let Some(byte) = chars.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        match chars.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'r') => bytes.push(b'\r'),
            Some(b'a') => bytes.push(0x07),
            Some(b'b') => bytes.push(0x08),
            Some(b'f') => bytes.push(0x0c),
            Some(b'v') => bytes.push(0x0b),
            Some(digit @ b'0'..=b'7') => {
                let mut value = u32::from(digit - b'0');
                for _ in 0..2 {
                    match chars.peek() {
                        Some(next @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(next - b'0');
                            chars.next();
                        }
                        _ => break,
                    }
                }
                bytes.push(u8::try_from(value).unwrap_or(b'?'));
            }
            Some(other) => bytes.push(other),
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_modified_file_with_hunks() {
        let patch = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@ fn main() {
 one
-two
+deux
+three

\\ No newline at end of file
@@ -10 +11,0 @@
-gone
";
        let files = parse(patch);
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!(file.path(), "src/lib.rs");
        assert_eq!(file.status, FileStatus::Modified);
        assert_eq!(file.hunks.len(), 2);
        let first = &file.hunks[0];
        assert_eq!(
            (
                first.old_start,
                first.old_lines,
                first.new_start,
                first.new_lines
            ),
            (1, 3, 1, 4)
        );
        assert_eq!(first.section, "fn main() {");
        let kinds: Vec<_> = first.lines.iter().map(|line| line.kind).collect();
        assert_eq!(
            kinds,
            [
                LineKind::Context,
                LineKind::Deleted,
                LineKind::Added,
                LineKind::Added,
                LineKind::Context,
            ]
        );
        assert_eq!(first.lines[2].text, "deux");
        let second = &file.hunks[1];
        assert_eq!((second.old_start, second.old_lines), (10, 1));
        assert_eq!((second.new_start, second.new_lines), (11, 0));
        assert_eq!((file.additions(), file.deletions()), (2, 2));
    }

    #[test]
    fn parses_added_deleted_and_binary_files() {
        let patch = "\
diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+hello
diff --git a/old.txt b/old.txt
deleted file mode 100644
index 3333333..0000000
--- a/old.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/logo.png b/logo.png
index 4444444..5555555 100644
Binary files a/logo.png and b/logo.png differ
";
        let files = parse(patch);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].status, FileStatus::Added);
        assert_eq!(files[0].path(), "new.txt");
        assert_eq!(files[1].status, FileStatus::Deleted);
        assert_eq!(files[1].path(), "old.txt");
        assert!(files[2].binary);
        assert_eq!(files[2].path(), "logo.png");
        assert!(files[2].hunks.is_empty());
    }

    #[test]
    fn parses_a_pure_rename_and_paths_with_spaces() {
        let patch = "\
diff --git a/old name.rs b/new name.rs
similarity index 100%
rename from old name.rs
rename to new name.rs
diff --git a/my file.txt b/my file.txt
index 1111111..2222222 100644
--- a/my file.txt\t
+++ b/my file.txt\t
@@ -1 +1 @@
-a
+b
";
        let files = parse(patch);
        assert_eq!(files[0].status, FileStatus::Renamed);
        assert_eq!(files[0].old_path, "old name.rs");
        assert_eq!(files[0].new_path, "new name.rs");
        assert_eq!(files[1].path(), "my file.txt");
        assert_eq!(files[1].old_path, "my file.txt");
    }

    #[test]
    fn unquotes_c_style_paths() {
        assert_eq!(unquote("\"tab\\there\""), "tab\there");
        assert_eq!(unquote("\"quote\\\"d\""), "quote\"d");
        assert_eq!(unquote("\"caf\\303\\251\""), "café");
        assert_eq!(unquote("plain"), "plain");

        let patch = "\
diff --git \"a/we\\\"ird\" \"b/we\\\"ird\"
deleted file mode 100644
--- \"a/we\\\"ird\"
+++ /dev/null
@@ -1 +0,0 @@
-x
";
        let files = parse(patch);
        assert_eq!(files[0].path(), "we\"ird");
    }

    #[test]
    fn header_path_reads_only_symmetric_headers() {
        assert_eq!(header_path("a/x y b/x y").as_deref(), Some("x y"));
        assert_eq!(header_path("a/one b/two"), None);
    }
}
