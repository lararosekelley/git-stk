//! Group a review's files into tour steps by the role each file plays, read
//! in the order that builds context: what the change is configured and shaped
//! by, then what it does, then how it is tested and described.

use super::diff::FileDiff;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub enum StepKind {
    Config,
    Schema,
    Source,
    Tests,
    Docs,
    Generated,
}

impl StepKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Config => "Configuration and build",
            Self::Schema => "Schemas and types",
            Self::Source => "Implementation",
            Self::Tests => "Tests",
            Self::Docs => "Documentation",
            Self::Generated => "Generated and vendored files",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Schema => "schema",
            Self::Source => "source",
            Self::Tests => "tests",
            Self::Docs => "docs",
            Self::Generated => "generated",
        }
    }
}

pub struct Step {
    pub kind: StepKind,
    pub files: Vec<FileDiff>,
    /// What a tour plan calls the step, in place of its kind's name.
    pub label: Option<StepLabel>,
}

pub struct StepLabel {
    /// Stable for a plan, for remembering the step as reviewed.
    pub id: String,
    pub title: String,
    /// Markdown on what the step's files do.
    pub summary: String,
}

impl Step {
    pub fn id(&self) -> &str {
        self.label
            .as_ref()
            .map_or(self.kind.id(), |label| label.id.as_str())
    }

    pub fn title(&self) -> &str {
        self.label
            .as_ref()
            .map_or(self.kind.title(), |label| label.title.as_str())
    }
}

/// Files in step order, each step sorted by path. `is_generated` reports a
/// path marked `linguist-generated` in `.gitattributes`. Empty steps are left
/// out.
pub fn group(files: Vec<FileDiff>, is_generated: impl Fn(&str) -> bool) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    let mut files: Vec<_> = files
        .into_iter()
        .map(|file| {
            let kind = if is_generated(file.path()) {
                StepKind::Generated
            } else {
                classify(file.path())
            };
            (kind, file)
        })
        .collect();
    files.sort_by(|(a_kind, a), (b_kind, b)| a_kind.cmp(b_kind).then(a.path().cmp(b.path())));
    for (kind, file) in files {
        match steps.last_mut() {
            Some(step) if step.kind == kind => step.files.push(file),
            _ => steps.push(Step {
                kind,
                files: vec![file],
                label: None,
            }),
        }
    }
    steps
}

const LOCKFILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "bun.lock",
    "poetry.lock",
    "uv.lock",
    "Pipfile.lock",
    "Gemfile.lock",
    "composer.lock",
    "go.sum",
    "flake.lock",
    "mix.lock",
    "pubspec.lock",
    "Package.resolved",
];

const CONFIG_FILES: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "requirements.txt",
    "Pipfile",
    "Gemfile",
    "go.mod",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "pom.xml",
    "Makefile",
    "makefile",
    "justfile",
    "Justfile",
    "CMakeLists.txt",
    "Dockerfile",
    "Containerfile",
    "flake.nix",
    "rust-toolchain",
    "rust-toolchain.toml",
];

const CONFIG_EXTENSIONS: &[&str] = &["toml", "yml", "yaml", "ini", "cfg", "conf", "properties"];

const SCHEMA_EXTENSIONS: &[&str] = &[
    "sql", "proto", "graphql", "gql", "prisma", "avsc", "thrift", "xsd",
];

/// Directories of third-party code, shown collapsed like generated files.
const VENDOR_DIRS: &[&str] = &[
    "vendor",
    "vendored",
    "third_party",
    "third-party",
    "thirdparty",
    "node_modules",
];

/// Words that name a license or notice file, in the capitals such files use.
const LEGAL_NAMES: &[&str] = &[
    "LICENSE",
    "LICENCE",
    "COPYING",
    "NOTICE",
    "AUTHORS",
    "CONTRIBUTORS",
    "PATENTS",
    "COPYRIGHT",
];

const DOC_EXTENSIONS: &[&str] = &["md", "mdx", "markdown", "rst", "adoc", "txt"];

const TEST_DIRS: &[&str] = &[
    "test",
    "tests",
    "__tests__",
    "spec",
    "specs",
    "testdata",
    "fixtures",
    "__snapshots__",
];

fn classify(path: &str) -> StepKind {
    let name = path.rsplit('/').next().unwrap_or(path);
    let dirs: Vec<&str> = path.split('/').rev().skip(1).collect();
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, extension),
        _ => (name, ""),
    };
    let extension = extension.to_ascii_lowercase();

    if LOCKFILES.contains(&name)
        || dirs.iter().any(|dir| VENDOR_DIRS.contains(dir))
        || stem.ends_with(".min")
        || stem.ends_with(".pb")
        || stem.ends_with("_pb2")
        || stem.ends_with(".generated")
        || stem.ends_with(".g")
    {
        return StepKind::Generated;
    }
    if is_test(name, stem, &dirs) {
        return StepKind::Tests;
    }
    if DOC_EXTENSIONS.contains(&extension.as_str())
        || dirs.iter().any(|dir| matches!(*dir, "docs" | "doc"))
        // LICENSE-MIT, highlight.js-LICENSE, NOTICE.txt
        || name
            .split(['-', '.', '_'])
            .any(|word| LEGAL_NAMES.contains(&word))
    {
        return StepKind::Docs;
    }
    if SCHEMA_EXTENSIONS.contains(&extension.as_str())
        || name.ends_with(".d.ts")
        || matches!(stem, "schema" | "types" | "openapi" | "swagger")
        || dirs.iter().any(|dir| {
            matches!(
                *dir,
                "migrations" | "migrate" | "schema" | "schemas" | "types"
            )
        })
    {
        return StepKind::Schema;
    }
    if CONFIG_FILES.contains(&name)
        || CONFIG_EXTENSIONS.contains(&extension.as_str())
        // vite.config.ts, commitlint.config.cjs, jest.config.js
        || stem.ends_with(".config")
        || name.starts_with('.')
        || dirs.iter().any(|dir| dir.starts_with('.'))
        || (extension == "json" && dirs.is_empty())
    {
        return StepKind::Config;
    }
    StepKind::Source
}

fn is_test(name: &str, stem: &str, dirs: &[&str]) -> bool {
    dirs.iter().any(|dir| TEST_DIRS.contains(dir))
        || stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with("_spec")
        || stem.ends_with(".test")
        || stem.ends_with(".spec")
        || stem.ends_with("Test")
        || stem.ends_with("Tests")
        || name.ends_with(".snap")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tour::diff::FileStatus;

    #[test]
    fn classify_sorts_paths_by_role() {
        for (path, kind) in [
            ("Cargo.toml", StepKind::Config),
            (".github/workflows/ci.yml", StepKind::Config),
            ("tsconfig.json", StepKind::Config),
            ("config/app.yaml", StepKind::Config),
            ("commitlint.config.cjs", StepKind::Config),
            ("web/vite.config.ts", StepKind::Config),
            ("db/migrations/001_init.sql", StepKind::Schema),
            ("api/user.proto", StepKind::Schema),
            ("src/types.ts", StepKind::Schema),
            ("index.d.ts", StepKind::Schema),
            ("src/main.rs", StepKind::Source),
            ("src/data/seed.json", StepKind::Source),
            ("tests/reviews.rs", StepKind::Tests),
            ("pkg/server_test.go", StepKind::Tests),
            ("test_api.py", StepKind::Tests),
            ("src/app.test.tsx", StepKind::Tests),
            ("src/__snapshots__/app.snap", StepKind::Tests),
            ("UserServiceTest.java", StepKind::Tests),
            ("README.md", StepKind::Docs),
            ("docs/guide.html", StepKind::Docs),
            ("LICENSE-MIT", StepKind::Docs),
            ("docs/highlight.js-LICENSE", StepKind::Docs),
            ("NOTICE", StepKind::Docs),
            ("AUTHORS.txt", StepKind::Docs),
            ("src/license.rs", StepKind::Source),
            (
                "vendor/github.com/pkg/errors/errors.go",
                StepKind::Generated,
            ),
            ("src/tour/vendor/highlight.js-LICENSE", StepKind::Generated),
            ("third_party/zlib/zlib.h", StepKind::Generated),
            ("Cargo.lock", StepKind::Generated),
            ("web/pnpm-lock.yaml", StepKind::Generated),
            ("static/app.min.js", StepKind::Generated),
            ("api/user.pb.go", StepKind::Generated),
        ] {
            assert_eq!(classify(path), kind, "{path}");
        }
    }

    fn file(path: &str) -> FileDiff {
        FileDiff {
            old_path: path.to_owned(),
            new_path: path.to_owned(),
            status: FileStatus::Modified,
            binary: false,
            header: Vec::new(),
            hunks: Vec::new(),
            old_text: None,
            new_text: None,
        }
    }

    #[test]
    fn group_orders_steps_and_files_and_skips_empty_steps() {
        let steps = group(
            vec![
                file("tests/b.rs"),
                file("src/z.rs"),
                file("src/a.rs"),
                file("Cargo.toml"),
                file("tests/a.rs"),
                file("src/gen.rs"),
            ],
            |path| path == "src/gen.rs",
        );
        let shape: Vec<(StepKind, Vec<&str>)> = steps
            .iter()
            .map(|step| (step.kind, step.files.iter().map(FileDiff::path).collect()))
            .collect();
        assert_eq!(
            shape,
            [
                (StepKind::Config, vec!["Cargo.toml"]),
                (StepKind::Source, vec!["src/a.rs", "src/z.rs"]),
                (StepKind::Tests, vec!["tests/a.rs", "tests/b.rs"]),
                (StepKind::Generated, vec!["src/gen.rs"]),
            ]
        );
    }
}
