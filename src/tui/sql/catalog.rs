//! Expandable catalog tree state.

use crate::core::sql::catalog::{Catalog, RelationKind};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum NodeKey {
    Schema(String),
    Relation(String, String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogNodeKind {
    Schema,
    Relation(RelationKind),
    Column,
    PrimaryKey,
    ForeignKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogNode {
    pub depth: usize,
    pub label: String,
    pub insert: Option<String>,
    pub kind: CatalogNodeKind,
    pub expanded: bool,
    pub expandable: bool,
    key: Option<NodeKey>,
}

#[derive(Debug, Default)]
pub struct CatalogState {
    pub catalog: Option<Catalog>,
    pub loading: bool,
    pub error: Option<String>,
    pub cursor: usize,
    pub offset: usize,
    expanded: BTreeSet<NodeKey>,
}

impl CatalogState {
    pub fn begin_refresh(&mut self) {
        self.loading = true;
        self.error = None;
    }

    pub fn set_catalog(&mut self, catalog: Catalog) {
        self.catalog = Some(catalog);
        self.loading = false;
        self.error = None;
        self.clamp();
    }

    pub fn set_error(&mut self, error: String) {
        self.loading = false;
        self.error = Some(error);
    }

    pub fn nodes(&self) -> Vec<CatalogNode> {
        let Some(catalog) = &self.catalog else {
            return Vec::new();
        };
        let mut nodes = Vec::new();
        for schema in catalog.schemas() {
            let schema_key = NodeKey::Schema(schema.to_string());
            let schema_expanded = self.expanded.contains(&schema_key);
            nodes.push(CatalogNode {
                depth: 0,
                label: schema.to_string(),
                insert: Some(crate::core::sql::catalog::quote_identifier(schema)),
                kind: CatalogNodeKind::Schema,
                expanded: schema_expanded,
                expandable: true,
                key: Some(schema_key),
            });
            if !schema_expanded {
                continue;
            }
            for relation in catalog
                .relations
                .iter()
                .filter(|relation| relation.schema == schema)
            {
                let relation_key = NodeKey::Relation(schema.to_string(), relation.name.clone());
                let relation_expanded = self.expanded.contains(&relation_key);
                nodes.push(CatalogNode {
                    depth: 1,
                    label: relation.name.clone(),
                    insert: Some(crate::core::sql::catalog::quote_identifier(&relation.name)),
                    kind: CatalogNodeKind::Relation(relation.kind),
                    expanded: relation_expanded,
                    expandable: !relation.columns.is_empty()
                        || !relation.primary_key.is_empty()
                        || !relation.foreign_keys.is_empty(),
                    key: Some(relation_key),
                });
                if !relation_expanded {
                    continue;
                }
                for column in &relation.columns {
                    nodes.push(CatalogNode {
                        depth: 2,
                        label: format!(
                            "{} · {}{}{}",
                            column.name,
                            column.data_type,
                            if column.nullable { "" } else { " · NOT NULL" },
                            if column.default.is_some() {
                                " · DEFAULT"
                            } else {
                                ""
                            }
                        ),
                        insert: Some(crate::core::sql::catalog::quote_identifier(&column.name)),
                        kind: CatalogNodeKind::Column,
                        expanded: false,
                        expandable: false,
                        key: None,
                    });
                }
                if !relation.primary_key.is_empty() {
                    nodes.push(CatalogNode {
                        depth: 2,
                        label: format!("PK ({})", relation.primary_key.join(", ")),
                        insert: None,
                        kind: CatalogNodeKind::PrimaryKey,
                        expanded: false,
                        expandable: false,
                        key: None,
                    });
                }
                for foreign_key in &relation.foreign_keys {
                    nodes.push(CatalogNode {
                        depth: 2,
                        label: format!(
                            "FK {} ({}) → {}.{} ({})",
                            foreign_key.name,
                            foreign_key.columns.join(", "),
                            foreign_key.referenced_schema,
                            foreign_key.referenced_relation,
                            foreign_key.referenced_columns.join(", ")
                        ),
                        insert: None,
                        kind: CatalogNodeKind::ForeignKey,
                        expanded: false,
                        expandable: false,
                        key: None,
                    });
                }
            }
        }
        nodes
    }

    pub fn selected(&self) -> Option<CatalogNode> {
        self.nodes().get(self.cursor).cloned()
    }

    pub fn move_by(&mut self, delta: isize, viewport: usize) {
        self.cursor = self.cursor.saturating_add_signed(delta);
        self.clamp();
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if viewport > 0 && self.cursor >= self.offset + viewport {
            self.offset = self.cursor + 1 - viewport;
        }
    }

    pub fn toggle(&mut self) {
        let Some(node) = self.selected() else { return };
        if !node.expandable {
            return;
        }
        let Some(key) = node.key else { return };
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        self.clamp();
    }

    pub fn expand_defaults(&mut self) {
        let Some(catalog) = &self.catalog else { return };
        for schema in catalog.schemas() {
            if schema != "pg_catalog"
                && schema != "information_schema"
                && !schema.starts_with("pg_toast")
            {
                self.expanded.insert(NodeKey::Schema(schema.to_string()));
            }
        }
    }

    fn clamp(&mut self) {
        self.cursor = self.cursor.min(self.nodes().len().saturating_sub(1));
    }
}
