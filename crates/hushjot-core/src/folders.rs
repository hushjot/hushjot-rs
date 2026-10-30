//! The folder tree (the app's `src/core/notes/folders.ts`).
//!
//! Folders nest to any depth. Two devices moving folders concurrently can
//! produce a parent cycle after sync — the merge is last-write-wins per folder
//! and sees no tree — so every traversal here tolerates cycles rather than
//! assuming they cannot happen, and a folder whose parent is missing surfaces
//! at the top level rather than disappearing.

use std::collections::{HashMap, HashSet};

use crate::collate;
use crate::js;

/// A decrypted folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    /// The folder id (a UUID for folders made by HushJot clients).
    pub id: String,
    /// Its name.
    pub name: String,
    /// Its parent, or `None` for the top level.
    pub parent_id: Option<String>,
    /// When it was created (epoch ms).
    pub created_at: i64,
    /// Its last change (epoch ms). Folders have no revision; this decides which
    /// side of a two-device disagreement wins.
    pub updated_at: i64,
}

/// A folder placed in the display tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderNode {
    /// The folder.
    pub folder: Folder,
    /// 0 at the top level.
    pub depth: usize,
    /// Its children, ordered by name.
    pub children: Vec<FolderNode>,
}

/// The display tree: children under parents, siblings ordered by name, orphans
/// and cycle members promoted to the top level so every folder is reachable.
pub fn build_tree(folders: &[Folder]) -> Vec<FolderNode> {
    let ids: HashSet<&str> = folders.iter().map(|f| f.id.as_str()).collect();
    let mut children: HashMap<&str, Vec<&Folder>> = HashMap::new();
    let mut roots: Vec<&Folder> = Vec::new();
    for f in folders {
        match f.parent_id.as_deref() {
            Some(p) if p != f.id && ids.contains(p) => children.entry(p).or_default().push(f),
            _ => roots.push(f),
        }
    }

    // A parent cycle leaves every member owned by another member, so none of
    // them is a root. Promote one member per cycle and cut its back edge.
    let mut reachable: HashSet<&str> = HashSet::new();
    fn mark<'a>(id: &'a str, children: &HashMap<&'a str, Vec<&'a Folder>>, reachable: &mut HashSet<&'a str>) {
        if !reachable.insert(id) {
            return;
        }
        for child in children.get(id).into_iter().flatten() {
            mark(&child.id, children, reachable);
        }
    }
    for root in &roots {
        mark(&root.id, &children, &mut reachable);
    }
    for f in folders {
        if reachable.contains(f.id.as_str()) {
            continue;
        }
        if let Some(p) = f.parent_id.as_deref() {
            if let Some(siblings) = children.get_mut(p) {
                siblings.retain(|c| c.id != f.id);
            }
        }
        roots.push(f);
        mark(&f.id, &children, &mut reachable);
    }

    fn build<'a>(list: &[&'a Folder], depth: usize, children: &HashMap<&'a str, Vec<&'a Folder>>) -> Vec<FolderNode> {
        let mut sorted: Vec<&Folder> = list.to_vec();
        sorted.sort_by(|a, b| collate::compare(&a.name, &b.name));
        sorted
            .into_iter()
            .map(|f| FolderNode {
                folder: f.clone(),
                depth,
                children: build(children.get(f.id.as_str()).map(Vec::as_slice).unwrap_or(&[]), depth + 1, children),
            })
            .collect()
    }
    build(&roots, 0, &children)
}

/// The tree flattened depth-first, as a sidebar renders it.
pub fn flatten_tree(roots: &[FolderNode]) -> Vec<&FolderNode> {
    let mut out = Vec::new();
    fn walk<'a>(nodes: &'a [FolderNode], out: &mut Vec<&'a FolderNode>) {
        for n in nodes {
            out.push(n);
            walk(&n.children, out);
        }
    }
    walk(roots, &mut out);
    out
}

/// Every folder in the subtree rooted at `root_id`, that folder included;
/// empty when it does not exist. Deleting a folder deletes exactly these.
pub fn subtree_ids(folders: &[Folder], root_id: &str) -> Vec<String> {
    if !folders.iter().any(|f| f.id == root_id) {
        return Vec::new();
    }
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for f in folders {
        if let Some(p) = f.parent_id.as_deref() {
            if p != f.id {
                children.entry(p).or_default().push(&f.id);
            }
        }
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![root_id];
    // Depth-first, pre-order, children in input order: the app's recursion.
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        out.push(id.to_string());
        if let Some(kids) = children.get(id) {
            for kid in kids.iter().rev() {
                stack.push(kid);
            }
        }
    }
    out
}

/// The ids above `id`, nearest first. Stops at a missing parent or a cycle.
pub fn ancestor_ids(folders: &[Folder], id: &str) -> Vec<String> {
    let by_id: HashMap<&str, &Folder> = folders.iter().map(|f| (f.id.as_str(), f)).collect();
    let mut out = Vec::new();
    let mut seen: HashSet<&str> = HashSet::from([id]);
    let mut parent = by_id.get(id).and_then(|f| f.parent_id.as_deref());
    while let Some(p) = parent {
        if seen.contains(p) || !by_id.contains_key(p) {
            break;
        }
        out.push(p.to_string());
        seen.insert(p);
        parent = by_id[p].parent_id.as_deref();
    }
    out
}

/// Whether `id` may move under `new_parent` (`None` is the top level): refused
/// for an unknown folder or target, and for a target inside its own subtree.
pub fn can_move_folder(folders: &[Folder], id: &str, new_parent: Option<&str>) -> bool {
    if !folders.iter().any(|f| f.id == id) {
        return false;
    }
    let Some(target) = new_parent else { return true };
    if !folders.iter().any(|f| f.id == target) {
        return false;
    }
    !subtree_ids(folders, id).iter().any(|s| s == target)
}

/// The names from the top level down to `id`, joined by `separator`.
pub fn folder_path(folders: &[Folder], id: &str, separator: &str) -> String {
    let by_id: HashMap<&str, &Folder> = folders.iter().map(|f| (f.id.as_str(), f)).collect();
    let Some(this) = by_id.get(id) else { return String::new() };
    let mut names: Vec<&str> =
        ancestor_ids(folders, id).iter().rev().map(|a| by_id[a.as_str()].name.as_str()).collect();
    names.push(&this.name);
    names.join(separator)
}

/// The folders a typed string names: an exact id, else a full `/` path, else a
/// plain name — case-insensitively, the first kind that matches anything
/// winning. With `prefix`, anything being typed towards matches as a last
/// resort. Ambiguity is returned, never resolved.
pub fn folders_matching(folders: &[Folder], wanted: &str, prefix: bool) -> Vec<String> {
    let needle = js::to_lower(js::trim(wanted));
    if needle.is_empty() {
        return Vec::new();
    }
    let ids =
        |pred: &dyn Fn(&Folder) -> bool| folders.iter().filter(|f| pred(f)).map(|f| f.id.clone()).collect::<Vec<_>>();

    let by_id = ids(&|f| js::to_lower(&f.id) == needle);
    if !by_id.is_empty() {
        return by_id;
    }
    let path_of = |f: &Folder| js::to_lower(&folder_path(folders, &f.id, "/"));
    let by_path = ids(&|f| path_of(f) == needle);
    if !by_path.is_empty() {
        return by_path;
    }
    let by_name = ids(&|f| js::to_lower(&f.name) == needle);
    if !by_name.is_empty() || !prefix {
        return by_name;
    }
    ids(&|f| js::to_lower(&f.name).starts_with(&needle) || path_of(f).starts_with(&needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(id: &str, name: &str, parent: Option<&str>) -> Folder {
        Folder { id: id.into(), name: name.into(), parent_id: parent.map(Into::into), created_at: 0, updated_at: 0 }
    }

    fn sample() -> Vec<Folder> {
        vec![f("w", "Work", None), f("db", "DB", Some("w")), f("pg", "Postgres", Some("db")), f("h", "Home", None)]
    }

    #[test]
    fn subtrees_and_ancestors() {
        let folders = sample();
        assert_eq!(subtree_ids(&folders, "w"), ["w", "db", "pg"]);
        assert_eq!(ancestor_ids(&folders, "pg"), ["db", "w"]);
        assert_eq!(folder_path(&folders, "pg", " / "), "Work / DB / Postgres");
        assert!(subtree_ids(&folders, "nope").is_empty());
    }

    #[test]
    fn moves_that_would_make_a_cycle_are_refused() {
        let folders = sample();
        assert!(!can_move_folder(&folders, "w", Some("pg")));
        assert!(!can_move_folder(&folders, "w", Some("w")));
        assert!(can_move_folder(&folders, "pg", None));
        assert!(can_move_folder(&folders, "h", Some("db")));
        assert!(!can_move_folder(&folders, "h", Some("missing")));
    }

    #[test]
    fn cycles_and_orphans_still_reach_the_tree() {
        let folders = vec![f("a", "A", Some("b")), f("b", "B", Some("a")), f("o", "Orphan", Some("gone"))];
        let tree = build_tree(&folders);
        assert_eq!(flatten_tree(&tree).len(), 3);
        assert_eq!(subtree_ids(&folders, "a"), ["a", "b"]);
        assert_eq!(ancestor_ids(&folders, "a"), ["b"]);
    }

    #[test]
    fn matching_prefers_id_then_path_then_name() {
        let mut folders = sample();
        folders.push(f("x", "db", Some("h")));
        assert_eq!(folders_matching(&folders, "W", false), ["w"]);
        assert_eq!(folders_matching(&folders, "work/db", false), ["db"]);
        assert_eq!(folders_matching(&folders, "DB", false), ["db"]);
        assert_eq!(folders_matching(&folders, "home/db", false), ["x"]);
        assert!(folders_matching(&folders, "Wor", false).is_empty());
        // A path prefix reaches the whole subtree, as in the app.
        assert_eq!(folders_matching(&folders, "Wor", true), ["w", "db", "pg"]);
        assert!(folders_matching(&folders, "  ", true).is_empty());
    }

    #[test]
    fn the_tree_orders_siblings_by_name() {
        let tree = build_tree(&sample());
        let names: Vec<_> = flatten_tree(&tree).iter().map(|n| (n.folder.name.as_str(), n.depth)).collect();
        assert_eq!(names, [("Home", 0), ("Work", 0), ("DB", 1), ("Postgres", 2)]);
    }
}
