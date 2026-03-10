use archive_core::ArchiveEntry;

/// A virtual folder tree built from the flat list of archive entries.
///
/// # Example
///
/// Entries `["a/b/file.txt", "a/c/other.txt", "readme.md"]` become:
/// ```text
/// root
/// ├── a/
/// │   ├── b/
/// │   │   └── file.txt
/// │   └── c/
/// │       └── other.txt
/// └── readme.md
/// ```
pub struct EntryTree {
    pub root: TreeNode,
}

/// A node in the entry tree — either a real archive entry or a synthetic directory.
pub struct TreeNode {
    /// Segment name (not the full path), e.g. `"b"` or `"file.txt"`.
    pub name: String,
    pub children: Vec<TreeNode>,
    /// Index into the `entries` slice passed to [`EntryTree::build`], if this
    /// node corresponds to an actual archive entry. `None` for synthetic
    /// intermediate directories created to fill gaps in the tree.
    pub entry_index: Option<usize>,
}

impl TreeNode {
    fn new_synthetic(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            children: Vec::new(),
            entry_index: None,
        }
    }

    /// Find or create a direct child with the given name, returning a mutable ref.
    fn get_or_insert_child(&mut self, name: &str) -> &mut TreeNode {
        // Avoid borrow-checker issues with a positional find-then-insert pattern.
        if let Some(pos) = self.children.iter().position(|c| c.name == name) {
            return &mut self.children[pos];
        }
        self.children.push(TreeNode::new_synthetic(name));
        self.children.last_mut().unwrap()
    }
}

impl EntryTree {
    /// Build a tree from a flat list of archive entries.
    ///
    /// Each entry's `name` is split on `/`. Intermediate directories that don't
    /// correspond to an explicit entry get `entry_index: None`. Real entries
    /// (files and explicit directory entries) get `entry_index: Some(i)`.
    pub fn build(entries: &[ArchiveEntry]) -> Self {
        let mut root = TreeNode {
            name: String::new(),
            children: Vec::new(),
            entry_index: None,
        };

        for (i, entry) in entries.iter().enumerate() {
            // Strip a trailing slash so directory entries ("photos/") are
            // treated the same as intermediate path segments.
            let name = entry.name.trim_end_matches('/');
            if name.is_empty() {
                continue;
            }

            let segments: Vec<&str> = name.split('/').collect();
            let mut node = &mut root;

            for (depth, segment) in segments.iter().enumerate() {
                let is_last = depth == segments.len() - 1;
                node = node.get_or_insert_child(segment);
                if is_last {
                    // Mark the final node as the real entry.
                    node.entry_index = Some(i);
                }
            }
        }

        EntryTree { root }
    }

    /// Return the direct children of the node at `path`.
    ///
    /// `path` uses `/` as separator (e.g. `"a/b"`). An empty string or `"/"`
    /// returns the root's children.
    ///
    /// Returns an empty slice if the path does not exist in the tree.
    pub fn children_at(&self, path: &str) -> Vec<&TreeNode> {
        let path = path.trim_matches('/');
        let node = if path.is_empty() {
            &self.root
        } else {
            let mut current = &self.root;
            let mut found = true;
            for segment in path.split('/') {
                match current.children.iter().find(|c| c.name == segment) {
                    Some(child) => current = child,
                    None => {
                        found = false;
                        break;
                    }
                }
            }
            if !found {
                return Vec::new();
            }
            current
        };
        node.children.iter().collect()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> ArchiveEntry {
        ArchiveEntry::new_test(name, name.ends_with('/'))
    }

    #[test]
    fn flat_files_appear_at_root() {
        let entries = vec![entry("a.txt"), entry("b.txt")];
        let tree = EntryTree::build(&entries);
        let names: Vec<&str> = tree.root.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a.txt", "b.txt"]);
    }

    #[test]
    fn nested_files_build_correct_tree() {
        let entries = vec![
            entry("photos/cat.jpg"),
            entry("photos/dog.jpg"),
            entry("readme.md"),
        ];
        let tree = EntryTree::build(&entries);

        // Root has two children: photos/ and readme.md
        assert_eq!(tree.root.children.len(), 2);
        let photos = &tree.root.children[0];
        assert_eq!(photos.name, "photos");
        assert_eq!(photos.children.len(), 2);
        assert_eq!(photos.children[0].name, "cat.jpg");
        assert_eq!(photos.children[1].name, "dog.jpg");
    }

    #[test]
    fn explicit_directory_entry_gets_index() {
        let entries = vec![entry("docs/"), entry("docs/guide.txt")];
        let tree = EntryTree::build(&entries);
        let docs = &tree.root.children[0];
        assert_eq!(docs.name, "docs");
        assert_eq!(docs.entry_index, Some(0));
        assert_eq!(docs.children[0].entry_index, Some(1));
    }

    #[test]
    fn synthetic_intermediate_dir_has_no_index() {
        let entries = vec![entry("a/b/c.txt")];
        let tree = EntryTree::build(&entries);
        let a = &tree.root.children[0];
        assert_eq!(a.name, "a");
        assert_eq!(a.entry_index, None); // synthetic
        let b = &a.children[0];
        assert_eq!(b.entry_index, None); // synthetic
        assert_eq!(b.children[0].entry_index, Some(0)); // real
    }

    #[test]
    fn children_at_root() {
        let entries = vec![entry("a.txt"), entry("b/c.txt")];
        let tree = EntryTree::build(&entries);
        let children = tree.children_at("");
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn children_at_nested_path() {
        let entries = vec![entry("a/b/file1.txt"), entry("a/b/file2.txt")];
        let tree = EntryTree::build(&entries);
        let children = tree.children_at("a/b");
        let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["file1.txt", "file2.txt"]);
    }

    #[test]
    fn children_at_missing_path_returns_empty() {
        let entries = vec![entry("a.txt")];
        let tree = EntryTree::build(&entries);
        assert!(tree.children_at("nonexistent").is_empty());
    }

    #[test]
    fn empty_archive_builds_empty_tree() {
        let tree = EntryTree::build(&[]);
        assert!(tree.root.children.is_empty());
    }
}
