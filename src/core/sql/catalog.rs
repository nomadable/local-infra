//! PostgreSQL catalog discovery and completion candidates.

use crate::core::error::{Error, Result};
use crate::core::sql::connection::SqlSession;
use crate::core::sql::statement;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

const CATALOG_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    Table,
    PartitionedTable,
    View,
    MaterializedView,
    Sequence,
}

impl RelationKind {
    fn from_relkind(value: &str) -> Option<Self> {
        match value {
            "r" => Some(Self::Table),
            "p" => Some(Self::PartitionedTable),
            "v" => Some(Self::View),
            "m" => Some(Self::MaterializedView),
            "S" => Some(Self::Sequence),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub primary_key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKey {
    pub name: String,
    pub columns: Vec<String>,
    pub referenced_schema: String,
    pub referenced_relation: String,
    pub referenced_columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogRelation {
    pub schema: String,
    pub name: String,
    pub kind: RelationKind,
    pub system: bool,
    pub columns: Vec<CatalogColumn>,
    pub primary_key: Vec<String>,
    pub foreign_keys: Vec<ForeignKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    pub database: String,
    pub relations: Vec<CatalogRelation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionKind {
    Keyword,
    Schema,
    Relation,
    Column,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionItem {
    pub value: String,
    pub label: String,
    pub kind: CompletionKind,
}

impl Catalog {
    pub fn schemas(&self) -> Vec<&str> {
        let mut schemas = BTreeSet::new();
        for relation in &self.relations {
            schemas.insert(relation.schema.as_str());
        }
        schemas.into_iter().collect()
    }

    pub fn completions(&self, prefix: &str) -> Vec<CompletionItem> {
        let prefix_lower = prefix.trim_matches('"').to_ascii_lowercase();
        let mut values = BTreeMap::<String, CompletionItem>::new();
        for keyword in statement::keyword_completions(prefix) {
            values.insert(
                keyword.clone(),
                CompletionItem {
                    value: keyword.clone(),
                    label: keyword,
                    kind: CompletionKind::Keyword,
                },
            );
        }
        for schema in self.schemas() {
            add_completion(
                &mut values,
                schema,
                quote_identifier(schema),
                CompletionKind::Schema,
                &prefix_lower,
            );
        }
        for relation in &self.relations {
            let qualified = format!(
                "{}.{}",
                quote_identifier(&relation.schema),
                quote_identifier(&relation.name)
            );
            add_completion(
                &mut values,
                &relation.name,
                quote_identifier(&relation.name),
                CompletionKind::Relation,
                &prefix_lower,
            );
            add_completion(
                &mut values,
                &format!("{}.{}", relation.schema, relation.name),
                qualified,
                CompletionKind::Relation,
                &prefix_lower,
            );
            for column in &relation.columns {
                add_completion(
                    &mut values,
                    &column.name,
                    quote_identifier(&column.name),
                    CompletionKind::Column,
                    &prefix_lower,
                );
            }
        }
        values.into_values().take(100).collect()
    }
}

fn add_completion(
    values: &mut BTreeMap<String, CompletionItem>,
    candidate: &str,
    value: String,
    kind: CompletionKind,
    prefix: &str,
) {
    if !candidate.to_ascii_lowercase().starts_with(prefix) {
        return;
    }
    values
        .entry(value.clone())
        .or_insert_with(|| CompletionItem {
            value,
            label: candidate.to_string(),
            kind,
        });
}

pub fn quote_identifier(identifier: &str) -> String {
    let simple = identifier.chars().enumerate().all(|(index, ch)| {
        (index == 0 && (ch == '_' || ch.is_ascii_lowercase()))
            || (index > 0
                && (ch == '_' || ch == '$' || ch.is_ascii_lowercase() || ch.is_ascii_digit()))
    });
    if simple && !statement::is_keyword(identifier) {
        identifier.to_string()
    } else {
        format!("\"{}\"", identifier.replace('"', "\"\""))
    }
}

pub async fn refresh(session: &SqlSession) -> Result<Catalog> {
    tokio::time::timeout(CATALOG_TIMEOUT, load(session))
        .await
        .map_err(|_| {
            Error::failed(
                "catalog refresh 시간이 초과되었습니다",
                "PostgreSQL catalog query가 30초 안에 끝나지 않았습니다.",
                "query session은 유지됩니다. 권한과 server 부하를 확인한 뒤 다시 refresh하세요.",
            )
        })?
}

async fn load(session: &SqlSession) -> Result<Catalog> {
    let relation_rows = session
        .client()
        .query(
            "SELECT n.nspname, c.relname, c.relkind::text
             FROM pg_catalog.pg_class c
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relkind IN ('r','p','v','m','S')
             ORDER BY n.nspname, c.relname",
            &[],
        )
        .await
        .map_err(|error| catalog_error(&error))?;
    let mut relations = Vec::with_capacity(relation_rows.len());
    let mut indexes = BTreeMap::new();
    for row in relation_rows {
        let schema: String = row.get(0);
        let name: String = row.get(1);
        let relkind: String = row.get(2);
        let Some(kind) = RelationKind::from_relkind(&relkind) else {
            continue;
        };
        let index = relations.len();
        indexes.insert((schema.clone(), name.clone()), index);
        relations.push(CatalogRelation {
            system: schema == "pg_catalog"
                || schema == "information_schema"
                || schema.starts_with("pg_toast"),
            schema,
            name,
            kind,
            columns: Vec::new(),
            primary_key: Vec::new(),
            foreign_keys: Vec::new(),
        });
    }

    let column_rows = session
        .client()
        .query(
            "SELECT n.nspname, c.relname, a.attname,
                    pg_catalog.format_type(a.atttypid, a.atttypmod),
                    NOT a.attnotnull AS nullable,
                    pg_catalog.pg_get_expr(ad.adbin, ad.adrelid)
             FROM pg_catalog.pg_attribute a
             JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             LEFT JOIN pg_catalog.pg_attrdef ad
                    ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
             WHERE c.relkind IN ('r','p','v','m','S')
               AND a.attnum > 0 AND NOT a.attisdropped
             ORDER BY n.nspname, c.relname, a.attnum",
            &[],
        )
        .await
        .map_err(|error| catalog_error(&error))?;
    for row in column_rows {
        let key = (row.get::<_, String>(0), row.get::<_, String>(1));
        if let Some(index) = indexes.get(&key).copied() {
            relations[index].columns.push(CatalogColumn {
                name: row.get(2),
                data_type: row.get(3),
                nullable: row.get(4),
                default: row.get(5),
                primary_key: false,
            });
        }
    }

    let key_rows = session
        .client()
        .query(
            "SELECT ns.nspname, rel.relname, con.conname, con.contype::text,
                    string_agg(att.attname, E'\\x1f' ORDER BY ord.ordinality),
                    fns.nspname, frel.relname,
                    string_agg(fatt.attname, E'\\x1f' ORDER BY ord.ordinality)
             FROM pg_catalog.pg_constraint con
             JOIN pg_catalog.pg_class rel ON rel.oid = con.conrelid
             JOIN pg_catalog.pg_namespace ns ON ns.oid = rel.relnamespace
             JOIN LATERAL unnest(con.conkey) WITH ORDINALITY ord(attnum, ordinality) ON true
             JOIN pg_catalog.pg_attribute att
                  ON att.attrelid = rel.oid AND att.attnum = ord.attnum
             LEFT JOIN pg_catalog.pg_class frel ON frel.oid = con.confrelid
             LEFT JOIN pg_catalog.pg_namespace fns ON fns.oid = frel.relnamespace
             LEFT JOIN pg_catalog.pg_attribute fatt
                  ON fatt.attrelid = frel.oid
                 AND fatt.attnum = con.confkey[ord.ordinality]
             WHERE con.contype IN ('p','f')
             GROUP BY ns.nspname, rel.relname, con.conname, con.contype,
                      fns.nspname, frel.relname
             ORDER BY ns.nspname, rel.relname, con.conname",
            &[],
        )
        .await
        .map_err(|error| catalog_error(&error))?;
    for row in key_rows {
        let key = (row.get::<_, String>(0), row.get::<_, String>(1));
        let Some(index) = indexes.get(&key).copied() else {
            continue;
        };
        let columns = split_names(row.get::<_, String>(4));
        let kind: String = row.get(3);
        if kind == "p" {
            relations[index].primary_key = columns.clone();
            for column in &mut relations[index].columns {
                column.primary_key = columns.contains(&column.name);
            }
        } else {
            relations[index].foreign_keys.push(ForeignKey {
                name: row.get(2),
                columns,
                referenced_schema: row.get::<_, Option<String>>(5).unwrap_or_default(),
                referenced_relation: row.get::<_, Option<String>>(6).unwrap_or_default(),
                referenced_columns: row
                    .get::<_, Option<String>>(7)
                    .map(split_names)
                    .unwrap_or_default(),
            });
        }
    }

    Ok(Catalog {
        database: session.endpoint.database.clone(),
        relations,
    })
}

fn split_names(value: String) -> Vec<String> {
    value.split('\u{1f}').map(ToString::to_string).collect()
}

fn catalog_error(error: &tokio_postgres::Error) -> Error {
    crate::core::sql::connection::postgres_failure("PostgreSQL catalog를 읽을 수 없습니다", error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_only_when_postgres_needs_it() {
        assert_eq!(quote_identifier("users"), "users");
        assert_eq!(quote_identifier("select"), "\"select\"");
        assert_eq!(quote_identifier("User Name"), "\"User Name\"");
        assert_eq!(quote_identifier("a\"b"), "\"a\"\"b\"");
    }
}
