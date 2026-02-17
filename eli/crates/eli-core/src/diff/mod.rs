use crate::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

pub mod engine;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub usize);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UndoNode<T> {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    pub created_at: DateTime<Utc>,
    pub label: String,
    pub data: Option<T>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UndoTree<T> {
    nodes: Vec<UndoNode<T>>,
    root: NodeId,
    current: NodeId,
}

impl<T> UndoTree<T> {
    pub fn new() -> Self {
        let root = UndoNode {
            id: NodeId(0),
            parent: None,
            children: Vec::new(),
            created_at: Utc::now(),
            label: "root".to_string(),
            data: None,
        };
        Self {
            nodes: vec![root],
            root: NodeId(0),
            current: NodeId(0),
        }
    }

    pub fn current(&self) -> NodeId {
        self.current
    }

    pub fn root(&self) -> NodeId {
        self.root
    }

    pub fn node(&self, id: NodeId) -> Option<&UndoNode<T>> {
        self.nodes.get(id.0)
    }

    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut UndoNode<T>> {
        self.nodes.get_mut(id.0)
    }

    pub fn apply(&mut self, label: impl Into<String>, data: T) -> NodeId {
        let id = NodeId(self.nodes.len());
        let node = UndoNode {
            id,
            parent: Some(self.current),
            children: Vec::new(),
            created_at: Utc::now(),
            label: label.into(),
            data: Some(data),
        };
        self.nodes.push(node);
        if let Some(parent) = self.node_mut(self.current) {
            parent.children.push(id);
        }
        self.current = id;
        id
    }

    pub fn can_undo(&self) -> bool {
        self.current != self.root
    }

    pub fn undo(&mut self) -> Option<NodeId> {
        let parent = self.node(self.current)?.parent?;
        self.current = parent;
        Some(self.current)
    }

    pub fn redo_children(&self) -> &[NodeId] {
        let Some(node) = self.node(self.current) else {
            return &[];
        };
        node.children.as_slice()
    }

    pub fn redo(&mut self, child_index: usize) -> Option<NodeId> {
        let next = *self.node(self.current)?.children.get(child_index)?;
        self.current = next;
        Some(self.current)
    }
}

impl<T> Default for UndoTree<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FileOp {
    Create { path: String, content: String },
    Replace { path: String, content: String },
    Delete { path: String },
    Patch { path: String, diff: String },
}

pub fn safe_join(root: &Path, rel: &Path) -> Result<PathBuf> {
    if rel.is_absolute() {
        return Err(Error::InvalidInput(format!(
            "path must be relative (got '{}')",
            rel.display()
        )));
    }

    let mut out = PathBuf::from(root);
    let root_len = out.components().count();

    for component in rel.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => out.push(part),
            Component::ParentDir => {
                if out.components().count() <= root_len {
                    return Err(Error::InvalidInput(format!(
                        "path escapes workspace root (got '{}')",
                        rel.display()
                    )));
                }
                out.pop();
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::InvalidInput(format!(
                    "path must be relative (got '{}')",
                    rel.display()
                )))
            }
        }
    }

    Ok(out)
}
