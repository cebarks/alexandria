use anyhow::Result;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::{RecordId, SurrealValue, ToSql};

use crate::models::{Fact, RawRecord};
use crate::record_id_to_string;

/// A sortable column for [`MemoryRepo::list`].
///
/// SurrealQL cannot bind an `ORDER BY` column name — bind parameters carry *values* only — so the
/// ordering expression has to be written into the query string. That is an injection shape, and the
/// answer here is structural rather than defensive: every variant maps to one `&'static str` literal
/// that is written down in this file (see [`FactSort::order_expr`]), and no caller-supplied string
/// ever reaches the clause. A new variant means a new literal here; the expression must never become
/// a `format!` over a name passed in by a caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactSort {
    CreatedAt,
    Confidence,
    Content,
    /// The record key itself — already unique, so it needs no tie-break.
    Id,
    /// How many tags a fact carries, not the tag strings: an array has no natural order, so
    /// "sort by tags" is only meaningful as a count.
    TagCount,
}

impl FactSort {
    /// The ORDER BY expression. A closed set of literals — never interpolated from input.
    fn order_expr(self) -> &'static str {
        match self {
            Self::CreatedAt => "created_at",
            Self::Confidence => "confidence",
            Self::Content => "content",
            Self::Id => "id",
            // SurrealDB 3.2 will not parse `ORDER BY array::len(tags)` ("Unexpected token `::`,
            // expected Eof" — ORDER BY takes a field path, not an expression), so the count is
            // projected under an alias and ordered by that name. Same computed-field trick as
            // [`MemoryRepo::all_ids_and_content`], which projects `created_at` only so ORDER BY
            // has it. The alias is not a `Fact` field, and serde drops unknown fields on read.
            Self::TagCount => "tag_count",
        }
    }

    /// The extra projection [`Self::order_expr`] needs, empty for the plain column sorts.
    fn projection(self) -> &'static str {
        match self {
            Self::TagCount => ", array::len(tags) AS tag_count",
            _ => "",
        }
    }

    /// The complete ORDER BY clause, including the tie-break documented on [`MemoryRepo::list`].
    fn order_clause(self, dir: SortDir) -> String {
        let primary = format!("{} {}", self.order_expr(), dir.sql());
        if self == Self::Id {
            primary
        } else {
            format!("{primary}, id ASC")
        }
    }
}

/// Sort direction for [`FactSort`] — also a closed set of literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    fn sql(self) -> &'static str {
        match self {
            Self::Asc => "ASC",
            Self::Desc => "DESC",
        }
    }
}

/// The page size [`FactListQuery::default`] applies.
///
/// Named rather than inlined because it is the storage-side twin of the debug UI's `?limit=`
/// fallback (`debug/memories.rs`), and the two must not drift: a caller that omits `limit` should
/// get the same window the operator sees in the browser.
pub const DEFAULT_FACT_LIST_LIMIT: usize = 50;

// ponytail: fixed search breadth, the value SurrealDB's own HNSW tests use. Must stay above the
// largest `k` a caller asks for; make it a config key if recall ever measures short.
const HNSW_EF: usize = 150;

/// The numeric-ef form is the only one the planner serves from the HNSW index; a distance name
/// in that slot means brute force.
fn knn_sql(k: usize, indexed: bool) -> String {
    let arg = if indexed {
        HNSW_EF.to_string()
    } else {
        "COSINE".to_string()
    };
    format!("SELECT * FROM fact WHERE deleted = false AND embedding <|{k},{arg}|> $q")
}

/// Everything [`MemoryRepo::list`] takes, as one value.
///
/// This exists so `list` needs no clippy arity allow. The seven knobs
/// were already one cohesive thing — a filter set plus a page — and `..Default::default()` lets
/// each caller name only the axes it actually varies instead of padding out seven positional
/// arguments, which is both shorter at the call site and immune to a transposed `limit`/`offset`.
#[derive(Debug, Clone)]
pub struct FactListQuery<'a> {
    /// Case-insensitive substring of `content`.
    pub search: Option<&'a str>,
    /// Keep only facts carrying this tag.
    pub tag: Option<&'a str>,
    pub include_deleted: bool,
    pub sort: FactSort,
    pub dir: SortDir,
    pub limit: usize,
    pub offset: usize,
}

impl Default for FactListQuery<'_> {
    /// The view this query hardcoded before column sorting existed: live facts only, newest
    /// first, from the top of the result set.
    fn default() -> Self {
        Self {
            search: None,
            tag: None,
            include_deleted: false,
            sort: FactSort::CreatedAt,
            dir: SortDir::Desc,
            limit: DEFAULT_FACT_LIST_LIMIT,
            offset: 0,
        }
    }
}

pub struct MemoryRepo<'a> {
    db: &'a Surreal<Any>,
}

impl<'a> MemoryRepo<'a> {
    pub fn new(db: &'a Surreal<Any>) -> Self {
        Self { db }
    }

    pub async fn create_fact(
        &self,
        content: &str,
        confidence: f64,
        embedding: &[f32],
        tags: &[String],
    ) -> Result<String> {
        let mut response = self
            .db
            .query(
                "CREATE fact SET \
                 content = $content, \
                 confidence = $confidence, \
                 embedding = $embedding, \
                 tags = $tags, \
                 deleted = false",
            )
            .bind(("content", content.to_string()))
            .bind(("confidence", confidence))
            .bind(("embedding", embedding.to_vec()))
            .bind(("tags", tags.to_vec()))
            .await?;

        let created: Option<Fact> = response.take(0)?;
        let fact = created.ok_or_else(|| anyhow::anyhow!("Failed to create fact"))?;
        let id = fact
            .id
            .ok_or_else(|| anyhow::anyhow!("Created fact has no id"))?;
        Ok(id.to_sql())
    }

    /// Create a `raw` record holding a full source document for `import_document`.
    pub async fn create_raw(&self, content: &str) -> Result<String> {
        let mut response = self
            .db
            .query("CREATE raw SET content = $content, deleted = false")
            .bind(("content", content.to_string()))
            .await?;

        let created: Option<RawRecord> = response.take(0)?;
        let raw = created.ok_or_else(|| anyhow::anyhow!("Failed to create raw record"))?;
        let id = raw
            .id
            .ok_or_else(|| anyhow::anyhow!("Raw record has no id"))?;
        Ok(record_id_to_string(&id))
    }

    pub async fn get_fact(&self, id: &str) -> Result<Option<Fact>> {
        let mut response = self
            .db
            .query("SELECT * FROM type::record($id)")
            .bind(("id", id.to_string()))
            .await?;
        let fact: Option<Fact> = response.take(0)?;
        Ok(fact)
    }

    /// The `k` live facts nearest to `query` by cosine similarity, nearest first, by a
    /// brute-force scan inside the database. `<|k,COSINE|>` never consults the HNSW index,
    /// defined or not; this is the path for a database without one.
    pub async fn nearest(&self, query: &[f32], k: usize) -> Result<Vec<Fact>> {
        self.knn(knn_sql(k, false), query).await
    }

    /// Same as `nearest`, served from the HNSW index. Only valid once
    /// `schema::ensure_vector_index` has succeeded: without the index the planner strips
    /// `<|k,ef|>` to a plain scan.
    pub async fn nearest_indexed(&self, query: &[f32], k: usize) -> Result<Vec<Fact>> {
        self.knn(knn_sql(k, true), query).await
    }

    async fn knn(&self, sql: String, query: &[f32]) -> Result<Vec<Fact>> {
        let mut response = self.db.query(sql).bind(("q", query.to_vec())).await?;
        let facts: Vec<Fact> = response.take(0)?;
        Ok(facts)
    }

    pub async fn soft_delete_fact(&self, id: &str) -> Result<()> {
        self.db
            .query("UPDATE type::record($id) SET deleted = true")
            .bind(("id", id.to_string()))
            .await?
            .check()?;
        Ok(())
    }

    /// Update a fact's content and/or tags. Returns the updated fact.
    pub async fn update_fact(
        &self,
        id: &str,
        content: Option<&str>,
        tags: Option<&[String]>,
        confidence: Option<f64>,
        embedding: Option<&[f32]>,
    ) -> Result<Option<Fact>> {
        let mut parts = Vec::new();
        if content.is_some() {
            parts.push("content = $content");
        }
        if tags.is_some() {
            parts.push("tags = $tags");
        }
        if confidence.is_some() {
            parts.push("confidence = $confidence");
        }
        if embedding.is_some() {
            parts.push("embedding = $embedding");
        }

        if parts.is_empty() {
            return self.get_fact(id).await;
        }

        let set_clause = parts.join(", ");
        let query = format!("UPDATE type::record($id) SET {set_clause}");

        let mut q = self.db.query(&query).bind(("id", id.to_string()));
        if let Some(c) = content {
            q = q.bind(("content", c.to_string()));
        }
        if let Some(t) = tags {
            q = q.bind(("tags", t.to_vec()));
        }
        if let Some(conf) = confidence {
            q = q.bind(("confidence", conf));
        }
        if let Some(emb) = embedding {
            q = q.bind(("embedding", emb.to_vec()));
        }

        let mut response = q.await?;
        let updated: Option<Fact> = response.take(0)?;
        Ok(updated)
    }

    /// List facts with optional content search, tag filter, deleted-inclusion, column sorting
    /// and offset pagination.
    ///
    /// The sort is server-side on purpose: the caller pages through a corpus far larger than one
    /// page, so reordering only the rows already in hand would disagree with the header claiming
    /// the table is sorted by that column.
    ///
    /// The guaranteed ordering is `<column> <dir>, id ASC` — except for [`FactSort::Id`], whose
    /// primary key is already unique:
    /// 1. The requested column, in the requested direction.
    /// 2. Ties break on the record id ascending. Ids are unique, so the pair is a total order and
    ///    `LIMIT`/`START` cannot repeat or skip a row between pages.
    ///
    /// The secondary key is load-bearing, not cosmetic. `confidence` is `DEFAULT 0.5`
    /// (`v001_initial.surql`), so a confidence-ordered table is mostly one big tie group; sorting
    /// by it alone leaves that group in an unspecified relative order and page 2 can repeat a row
    /// page 1 showed while silently dropping another — the same bug already fixed for sessions in
    /// [`SessionRepo::list`](crate::repos::SessionRepo::list).
    ///
    /// (SurrealDB's `mem://` sort currently happens to be stable over a key-ascending scan, so an
    /// all-tied group comes back in id order with or without the tie-break; see
    /// `test_list_confidence_tiebreak_pages_without_gaps` for what that costs the tests.)
    ///
    /// `created_at` is nullable in the Rust model (`Option<DateTime<Utc>>`). Checked against
    /// SurrealDB 3.2: `ORDER BY ... NULLS LAST` and `ORDER BY type::coalesce(...)` both fail to
    /// parse in an ORDER BY, so there is nothing to fix here — nulls sort smaller than any
    /// datetime, i.e. first ascending and last descending, and the row is never dropped. Pinned by
    /// `test_list_sorts_null_created_at_without_dropping_the_row`.
    ///
    /// Behaviour-preserving defaults for the debug UI's current view: `FactSort::CreatedAt` with
    /// `SortDir::Desc` is what this query hardcoded before column sorting existed — see
    /// [`FactListQuery::default`].
    pub async fn list(
        &self,
        FactListQuery {
            search,
            tag,
            include_deleted,
            sort,
            dir,
            limit,
            offset,
        }: FactListQuery<'_>,
    ) -> Result<Vec<Fact>> {
        let query = Self::list_query(search, tag, include_deleted, sort, dir);

        let mut q = self
            .db
            .query(&query)
            .bind(("limit", limit as i64))
            .bind(("offset", offset as i64));
        if let Some(s) = search {
            q = q.bind(("search", s.to_string()));
        }
        if let Some(t) = tag {
            q = q.bind(("tag", t.to_string()));
        }

        let mut response = q.await?;
        let facts: Vec<Fact> = response.take(0)?;
        Ok(facts)
    }

    /// Builds the exact query string [`Self::list`] runs. Split out so a test can print what the
    /// engine is actually sent rather than what the reviewer guesses it is.
    ///
    /// Everything interpolated here is a literal chosen by this module: the WHERE fragments are
    /// fixed strings selected by `Option::is_some` (their *values* stay bound), and the ORDER BY
    /// clause comes from the closed [`FactSort::order_clause`] allowlist.
    fn list_query(
        search: Option<&str>,
        tag: Option<&str>,
        include_deleted: bool,
        sort: FactSort,
        dir: SortDir,
    ) -> String {
        let mut conditions = Vec::new();
        if !include_deleted {
            conditions.push("deleted = false".to_string());
        }
        if search.is_some() {
            conditions
                .push("string::lowercase(content) CONTAINS string::lowercase($search)".to_string());
        }
        if tag.is_some() {
            conditions.push("$tag IN tags".to_string());
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        format!(
            "SELECT *{} FROM fact {where_clause} ORDER BY {} LIMIT $limit START $offset",
            sort.projection(),
            sort.order_clause(dir),
        )
    }

    /// Count facts matching the same filters as `list` (ignoring limit/offset).
    pub async fn count(
        &self,
        search: Option<&str>,
        tag: Option<&str>,
        include_deleted: bool,
    ) -> Result<usize> {
        let mut conditions = Vec::new();
        if !include_deleted {
            conditions.push("deleted = false".to_string());
        }
        if search.is_some() {
            conditions
                .push("string::lowercase(content) CONTAINS string::lowercase($search)".to_string());
        }
        if tag.is_some() {
            conditions.push("$tag IN tags".to_string());
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        let query = format!("SELECT count() FROM fact {where_clause} GROUP ALL");

        let mut q = self.db.query(&query);
        if let Some(s) = search {
            q = q.bind(("search", s.to_string()));
        }
        if let Some(t) = tag {
            q = q.bind(("tag", t.to_string()));
        }

        #[derive(serde::Deserialize, surrealdb::types::SurrealValue)]
        struct CountRow {
            count: i64,
        }

        let mut response = q.await?;
        let rows: Vec<CountRow> = response.take(0)?;
        Ok(rows.first().map(|r| r.count as usize).unwrap_or(0))
    }

    /// Tag frequencies across live facts, highest first, capped at `limit`.
    ///
    /// The cap is the point: the debug dashboard shows a top-N bar chart and must not pay for
    /// a distinct-tag sweep it will not render. Ties break on the tag name so the same page
    /// renders the same order twice.
    ///
    /// SurrealDB 3.2 cannot group by a value unnested out of an array field — `GROUP BY` over
    /// a subquery's array column, and `$value` in that position, both collapse to one `NONE`
    /// group — so the flatten happens in SQL (one row, `array::group` then `array::flatten`)
    /// and only the tally happens here. That keeps the read to the `tags` column; content and
    /// embeddings are never pulled.
    pub async fn top_tags(&self, limit: usize) -> Result<Vec<(String, usize)>> {
        #[derive(serde::Deserialize, SurrealValue)]
        struct TagRow {
            tags: Vec<String>,
        }

        let mut response = self
            .db
            .query(
                "SELECT array::flatten(array::group(tags)) AS tags \
                 FROM fact WHERE deleted = false GROUP ALL",
            )
            .await?;
        let row: Option<TagRow> = response.take(0)?;

        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for tag in row.map(|row| row.tags).unwrap_or_default() {
            *counts.entry(tag).or_default() += 1;
        }

        let mut pairs: Vec<(String, usize)> = counts.into_iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        pairs.truncate(limit);
        Ok(pairs)
    }

    /// Find the cluster containing this fact, if any (reverse traversal of contains_memory).
    pub async fn cluster_for_fact(&self, fact_id: &str) -> Result<Option<crate::models::Cluster>> {
        let mut response = self
            .db
            .query("SELECT * FROM type::record($id)<-contains_memory<-cluster")
            .bind(("id", fact_id.to_string()))
            .await?;
        let clusters: Vec<crate::models::Cluster> = response.take(0)?;
        Ok(clusters.into_iter().next())
    }

    /// Every fact, deleted ones included, as (id, content). Used by the embedding
    /// migration, which must re-embed lineage snapshots too so they stay comparable.
    pub async fn all_ids_and_content(&self) -> Result<Vec<(String, String)>> {
        #[derive(serde::Deserialize, SurrealValue)]
        struct Row {
            id: RecordId,
            content: String,
            // Projected only so ORDER BY has it; not returned.
            #[allow(dead_code)]
            created_at: Option<chrono::DateTime<chrono::Utc>>,
        }
        let mut response = self
            .db
            .query("SELECT id, content, created_at FROM fact ORDER BY created_at")
            .await?;
        let rows: Vec<Row> = response.take(0)?;
        Ok(rows
            .into_iter()
            .map(|r| (record_id_to_string(&r.id), r.content))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::Database;

    #[tokio::test]
    async fn test_create_raw() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = MemoryRepo::new(db.inner());

        let id = repo.create_raw("full document").await.unwrap();
        assert!(id.starts_with("raw:"), "unexpected id {id}");

        let mut response = db
            .inner()
            .query("SELECT * FROM type::record($id)")
            .bind(("id", id.clone()))
            .await
            .unwrap();
        let raw: Option<RawRecord> = response.take(0).unwrap();
        let raw = raw.expect("raw record round-trips");
        assert_eq!(raw.content, "full document");
        assert!(!raw.deleted);
    }

    #[tokio::test]
    async fn test_list_and_count_facts() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = MemoryRepo::new(db.inner());

        repo.create_fact("alpha content", 0.5, &[0.1, 0.2], &["tag1".to_string()])
            .await
            .unwrap();
        repo.create_fact("beta content", 0.5, &[0.3, 0.4], &["tag2".to_string()])
            .await
            .unwrap();
        let deleted_id = repo
            .create_fact("gamma content", 0.5, &[0.5, 0.6], &[])
            .await
            .unwrap();
        repo.soft_delete_fact(&deleted_id).await.unwrap();

        // Default: excludes deleted
        let all = repo
            .list(FactListQuery {
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(all.len(), 2);

        // include_deleted = true picks up all 3
        let with_deleted = repo
            .list(FactListQuery {
                include_deleted: true,
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(with_deleted.len(), 3);

        // search filters by content substring
        let searched = repo
            .list(FactListQuery {
                search: Some("alpha"),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(searched.len(), 1);
        assert_eq!(searched[0].content, "alpha content");

        // tag filters
        let tagged = repo
            .list(FactListQuery {
                tag: Some("tag2"),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(tagged.len(), 1);
        assert_eq!(tagged[0].content, "beta content");

        // count matches list length for same filters
        let count = repo.count(None, None, false).await.unwrap();
        assert_eq!(count, 2);

        // limit/offset paginate
        let page1 = repo
            .list(FactListQuery {
                limit: 1,
                ..Default::default()
            })
            .await
            .unwrap();
        let page2 = repo
            .list(FactListQuery {
                limit: 1,
                offset: 1,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page1.len(), 1);
        assert_eq!(page2.len(), 1);
        assert_ne!(page1[0].content, page2[0].content);
    }

    #[tokio::test]
    async fn test_nearest_orders_by_similarity_and_skips_deleted() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        crate::schema::ensure_vector_index(db.inner(), 2)
            .await
            .unwrap();
        let repo = MemoryRepo::new(db.inner());

        let near = repo
            .create_fact("near", 0.5, &[1.0, 0.0], &[])
            .await
            .unwrap();
        let mid = repo
            .create_fact("mid", 0.5, &[0.7, 0.7], &[])
            .await
            .unwrap();
        repo.create_fact("far", 0.5, &[0.0, 1.0], &[])
            .await
            .unwrap();
        let gone = repo
            .create_fact("gone", 0.5, &[1.0, 0.1], &[])
            .await
            .unwrap();
        repo.soft_delete_fact(&gone).await.unwrap();

        let ids = |facts: Vec<Fact>| -> Vec<String> {
            facts
                .iter()
                .map(|f| record_id_to_string(f.id.as_ref().unwrap()))
                .collect()
        };
        let want = vec![near, mid];
        assert_eq!(ids(repo.nearest(&[0.9, 0.1], 2).await.unwrap()), want);
        assert_eq!(
            ids(repo.nearest_indexed(&[0.9, 0.1], 2).await.unwrap()),
            want
        );
    }

    /// Results cannot tell the two plans apart, so assert the plan: a SurrealDB upgrade that
    /// stops serving `<|k,ef|>` from the index fails here instead of silently scanning.
    #[tokio::test]
    async fn test_nearest_indexed_plan_uses_the_hnsw_index() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        crate::schema::ensure_vector_index(db.inner(), 2)
            .await
            .unwrap();

        let plan = |indexed: bool| {
            let db = db.inner().clone();
            async move {
                let mut response = db
                    .query(format!("EXPLAIN FORMAT JSON {}", knn_sql(2, indexed)))
                    .bind(("q", vec![0.9f32, 0.1]))
                    .await
                    .unwrap();
                let plan: surrealdb::types::Value = response.take(0).unwrap();
                plan.to_sql().replace(' ', "")
            }
        };

        let indexed = plan(true).await;
        assert!(indexed.contains("KnnScan"), "{indexed}");
        assert!(indexed.contains("fact_embedding_hnsw"), "{indexed}");
        let brute = plan(false).await;
        assert!(brute.contains("KnnTopK"), "{brute}");
        assert!(!brute.contains("KnnScan"), "{brute}");
    }

    #[tokio::test]
    async fn test_cluster_for_fact() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = MemoryRepo::new(db.inner());
        let cluster_repo = crate::repos::ClusterRepo::new(db.inner());

        let fact_id = repo
            .create_fact("clustered content", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let cluster_id = cluster_repo
            .create(Some("test cluster"), &[0.1, 0.2])
            .await
            .unwrap();
        cluster_repo
            .add_member(&cluster_id, &fact_id)
            .await
            .unwrap();

        let cluster_found = repo.cluster_for_fact(&fact_id).await.unwrap();
        assert!(cluster_found.is_some());
        let cluster_found = cluster_found.unwrap();
        assert_eq!(cluster_found.label.as_deref(), Some("test cluster"));

        // Fact with no cluster returns None
        let orphan_id = repo
            .create_fact("orphan content", 0.5, &[0.9, 0.9], &[])
            .await
            .unwrap();
        let none = repo.cluster_for_fact(&orphan_id).await.unwrap();
        assert!(none.is_none());
    }

    #[tokio::test]
    async fn test_all_ids_and_content_includes_deleted() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = MemoryRepo::new(db.inner());

        let a = repo
            .create_fact("first", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let b = repo
            .create_fact("second", 0.5, &[0.3, 0.4], &[])
            .await
            .unwrap();
        repo.soft_delete_fact(&b).await.unwrap();

        // Sorted: same-tick created_at makes the query order unstable.
        let mut rows = repo.all_ids_and_content().await.unwrap();
        rows.sort();
        let mut expected = vec![
            (a.clone(), "first".to_string()),
            (b.clone(), "second".to_string()),
        ];
        expected.sort();
        assert_eq!(rows, expected);

        // update_fact with only an embedding is the write path reembed uses
        let updated = repo
            .update_fact(&a, None, None, None, Some(&[9.0, 8.0, 7.0]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.embedding, vec![9.0, 8.0, 7.0]);
        assert_eq!(updated.content, "first");
    }

    #[tokio::test]
    async fn test_top_tags_counts_live_facts_and_caps() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = crate::repos::MemoryRepo::new(db.inner());

        assert!(repo.top_tags(10).await.unwrap().is_empty());

        for (content, tags) in [
            ("a", &["rare"][..]),
            ("b", &["common"][..]),
            ("c", &["common"][..]),
            ("d", &["common", "other"][..]),
            ("e", &["zzz", "aaa"][..]),
            ("f", &["zzz", "aaa"][..]),
            ("g", &[][..]),
        ] {
            repo.create_fact(
                content,
                0.5,
                &[0.1],
                &tags.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
            )
            .await
            .unwrap();
        }
        // A soft-deleted fact's tags must not be counted: the tally is of live memory.
        let gone = repo
            .create_fact("gone", 0.5, &[0.1], &["ghost".to_string()])
            .await
            .unwrap();
        repo.soft_delete_fact(&gone).await.unwrap();

        let all = repo.top_tags(100).await.unwrap();
        assert_eq!(all[0], ("common".to_string(), 3));
        assert_eq!(
            all.iter().find(|(tag, _)| tag == "rare").map(|(_, n)| *n),
            Some(1)
        );
        assert_eq!(
            all.iter().find(|(tag, _)| tag == "ghost"),
            None,
            "a soft-deleted fact still contributed its tag: {all:?}"
        );
        // Equal counts break on the name, ascending.
        let aaa = all.iter().position(|(t, _)| t == "aaa").unwrap();
        let zzz = all.iter().position(|(t, _)| t == "zzz").unwrap();
        assert!(aaa < zzz, "tie must resolve by name; got {all:?}");

        // The cap is a cap, and it keeps the highest counts.
        let top3 = repo.top_tags(3).await.unwrap();
        assert_eq!(top3.len(), 3);
        assert_eq!(top3[0].0, "common");
        assert_eq!(top3, all[..3]);
    }

    // ---- column sorting -----------------------------------------------

    /// Seeds a fact and pins `created_at` to a fixed day. The schema defaults it to
    /// `time::now()`, so facts created inside one test share a timestamp and cannot prove a time
    /// ordering at all. `day` is a `u32` formatted into a datetime literal, so the only text that
    /// reaches the query is built from an integer here.
    async fn create_at(
        repo: &MemoryRepo<'_>,
        db: &Surreal<Any>,
        content: &str,
        confidence: f64,
        tags: &[&str],
        day: u32,
    ) -> String {
        let id = repo
            .create_fact(
                content,
                confidence,
                &[0.1],
                &tags.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
            )
            .await
            .unwrap();
        if day > 0 {
            let response = db
                .query(format!(
                    "UPDATE type::record($id) SET created_at = d'2024-01-{day:02}T00:00:00Z'"
                ))
                .bind(("id", id.clone()))
                .await
                .unwrap();
            response.check().unwrap();
        }
        id
    }

    async fn sorted_contents(repo: &MemoryRepo<'_>, sort: FactSort, dir: SortDir) -> Vec<String> {
        repo.list(FactListQuery {
            sort,
            dir,
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_iter()
        .map(|f| f.content)
        .collect()
    }

    async fn sorted_ids(repo: &MemoryRepo<'_>, sort: FactSort, dir: SortDir) -> Vec<String> {
        repo.list(FactListQuery {
            sort,
            dir,
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_iter()
        .map(|f| record_id_to_string(f.id.as_ref().expect("a listed fact has an id")))
        .collect()
    }

    /// Every key orders correctly in both directions, against an expectation written down here
    /// rather than read back from the database.
    #[tokio::test]
    async fn test_list_each_sort_key_orders_in_both_directions() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = MemoryRepo::new(db.inner());

        // (content, confidence, tags, created_at day) picked so that content, confidence and tag
        // count each give a *different* order — no key can pass by borrowing another's result —
        // and so that a direction which ignores `SortDir` cannot pass either.
        let fixture = [
            ("aa", 0.9, &["p", "q", "r"][..], 4),
            ("bb", 0.1, &["p"][..], 2),
            ("cc", 0.7, &[][..], 1),
            ("dd", 0.3, &["p", "q"][..], 3),
        ];
        let mut seeded = Vec::new();
        for (content, confidence, tags, day) in fixture {
            seeded.push(create_at(&repo, db.inner(), content, confidence, tags, day).await);
        }
        assert_eq!(seeded.len(), 4, "fixture ids must be distinct");
        seeded.sort();
        assert_eq!(
            seeded
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            4,
            "the generated ids must be distinct: {seeded:?}"
        );

        assert_eq!(
            sorted_contents(&repo, FactSort::CreatedAt, SortDir::Desc).await,
            vec!["aa", "dd", "bb", "cc"]
        );
        assert_eq!(
            sorted_contents(&repo, FactSort::CreatedAt, SortDir::Asc).await,
            vec!["cc", "bb", "dd", "aa"]
        );
        assert_eq!(
            sorted_contents(&repo, FactSort::Confidence, SortDir::Desc).await,
            vec!["aa", "cc", "dd", "bb"]
        );
        assert_eq!(
            sorted_contents(&repo, FactSort::Confidence, SortDir::Asc).await,
            vec!["bb", "dd", "cc", "aa"]
        );
        assert_eq!(
            sorted_contents(&repo, FactSort::Content, SortDir::Desc).await,
            vec!["dd", "cc", "bb", "aa"]
        );
        assert_eq!(
            sorted_contents(&repo, FactSort::Content, SortDir::Asc).await,
            vec!["aa", "bb", "cc", "dd"]
        );
        // Tag count: 3, 2, 1, 0. The zero-tag row sorts first ascending rather than erroring on
        // `array::len([])` or being dropped from the result set.
        assert_eq!(
            sorted_contents(&repo, FactSort::TagCount, SortDir::Desc).await,
            vec!["aa", "dd", "bb", "cc"]
        );
        assert_eq!(
            sorted_contents(&repo, FactSort::TagCount, SortDir::Asc).await,
            vec!["cc", "bb", "dd", "aa"]
        );

        // The id key is generated, so its expected order is Rust's own ordering of the ids that
        // creation returned. `FactSort::Id` is also the one key that takes no tie-break.
        let asc = sorted_ids(&repo, FactSort::Id, SortDir::Asc).await;
        let desc = sorted_ids(&repo, FactSort::Id, SortDir::Desc).await;
        let expected = seeded.clone();
        assert_eq!(asc.len(), 4, "every fact must be returned, got {asc:?}");
        assert_eq!(asc, expected, "`id ASC` must order by the record key");
        let mut reversed = expected;
        reversed.reverse();
        assert_eq!(desc, reversed, "`id DESC` must be the exact reverse");
    }

    /// The back-compat guarantee: `FactSort::CreatedAt` + `SortDir::Desc` must return exactly what
    /// the query returned before column sorting existed, replayed here as the old SQL string.
    #[tokio::test]
    async fn test_list_default_matches_the_pre_sorting_query_exactly() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = MemoryRepo::new(db.inner());

        for (content, day) in [("one", 1), ("two", 5), ("three", 3)] {
            create_at(&repo, db.inner(), content, 0.5, &[], day).await;
        }
        let gone = create_at(&repo, db.inner(), "deleted", 0.5, &[], 6).await;
        repo.soft_delete_fact(&gone).await.unwrap();

        let mut response = db
            .inner()
            .query(
                "SELECT * FROM fact WHERE deleted = false \
                 ORDER BY created_at DESC LIMIT $limit START $offset",
            )
            .bind(("limit", 100i64))
            .bind(("offset", 0i64))
            .await
            .unwrap();
        let legacy: Vec<Fact> = response.take(0).unwrap();

        let current = repo
            .list(FactListQuery {
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap();

        fn key(rows: &[Fact]) -> Vec<(String, Option<chrono::DateTime<chrono::Utc>>)> {
            rows.iter()
                .map(|f| (f.content.clone(), f.created_at))
                .collect()
        }
        assert_eq!(key(&current), key(&legacy));
        // And both are the order the UI showed: newest first, the soft-deleted row absent.
        assert_eq!(
            current
                .iter()
                .map(|f| f.content.clone())
                .collect::<Vec<_>>(),
            vec!["two", "three", "one"]
        );
    }

    /// Paging through a tie group is the property the tie-break exists for.
    ///
    /// Seven facts all sitting on the schema's `DEFAULT 0.5` confidence is the common case on a
    /// real corpus, not an edge case. The no-duplicates/no-gaps check is what the operator would
    /// notice, but on its own it does NOT catch a missing tie-break — an in-memory store scans a
    /// tie group in one stable order per process. The exact-sequence assertion is what pins the
    /// contract, which is why both are here (same reasoning as sessions'
    /// `test_list_untouched_tail_pages_in_one_stable_total_order`).
    ///
    /// Caveat, established by mutation: on `mem://` the assertions below still pass with the
    /// `, id ASC` tie-break deleted, because SurrealDB's sort is stable over a key-ascending scan,
    /// so an all-tied group happens to come back in id order anyway. The tie-break turns that
    /// accident into a guarantee — an index scan, a reverse scan or a top-K rewrite would each
    /// reshuffle the group and reopen the duplicate/drop bug on the second page — and
    /// `test_fact_sort_order_expr_is_a_closed_allowlist` is the assertion that actually fails when
    /// it is removed.
    #[tokio::test]
    async fn test_list_confidence_tiebreak_pages_without_gaps() {
        const PAGE: usize = 2;
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = MemoryRepo::new(db.inner());

        let mut seeded = Vec::new();
        for index in 0..7 {
            seeded.push(create_at(&repo, db.inner(), &format!("tied {index}"), 0.5, &[], 0).await);
        }
        let mut expected = seeded.clone();
        expected.sort();
        assert_eq!(expected.len(), 7);

        let mut concatenated: Vec<String> = Vec::new();
        let mut offset = 0;
        loop {
            let page = repo
                .list(FactListQuery {
                    sort: FactSort::Confidence,
                    limit: PAGE,
                    offset,
                    ..Default::default()
                })
                .await
                .unwrap();
            if page.is_empty() {
                break;
            }
            assert!(
                page.len() <= PAGE,
                "LIMIT must cap every page, got {} at offset {offset}",
                page.len()
            );
            concatenated.extend(
                page.into_iter()
                    .map(|f| record_id_to_string(f.id.as_ref().expect("listed fact has an id"))),
            );
            offset += PAGE;
            assert!(offset <= 7 * PAGE, "pagination did not terminate");
        }

        let mut dedup = concatenated.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(
            dedup.len(),
            concatenated.len(),
            "a fact appeared on two pages: {concatenated:?}"
        );
        assert_eq!(
            dedup, expected,
            "the pages must cover every fact exactly once"
        );
        assert_eq!(
            concatenated, expected,
            "the tie group must be ordered by id ASC, not by whatever the scan yielded"
        );
    }

    /// The ORDER BY text can only ever be one of ten literals, which is the whole injection
    /// argument: there is no code path from a caller's string to this clause.
    ///
    /// `expected_expr`'s exhaustive match is the guard on the claim — adding a `FactSort` variant
    /// without giving it a literal stops this test compiling, so the closed set cannot drift open.
    #[test]
    fn test_fact_sort_order_expr_is_a_closed_allowlist() {
        fn expected_expr(sort: FactSort) -> &'static str {
            match sort {
                FactSort::CreatedAt => "created_at",
                FactSort::Confidence => "confidence",
                FactSort::Content => "content",
                FactSort::Id => "id",
                FactSort::TagCount => "tag_count",
            }
        }
        let all = [
            FactSort::CreatedAt,
            FactSort::Confidence,
            FactSort::Content,
            FactSort::Id,
            FactSort::TagCount,
        ];
        let allowed = [
            "created_at ASC, id ASC",
            "created_at DESC, id ASC",
            "confidence ASC, id ASC",
            "confidence DESC, id ASC",
            "content ASC, id ASC",
            "content DESC, id ASC",
            "id ASC",
            "id DESC",
            "tag_count ASC, id ASC",
            "tag_count DESC, id ASC",
        ];
        let forbidden = [
            ";", "--", "/*", "$", "'", "\"", "
", "(", ")",
        ];
        for sort in all {
            assert_eq!(sort.order_expr(), expected_expr(sort));
            for dir in [SortDir::Asc, SortDir::Desc] {
                let clause = sort.order_clause(dir);
                assert!(
                    allowed.contains(&clause.as_str()),
                    "built clause {clause:?} is not one of the closed set"
                );
                for bad in forbidden {
                    assert!(
                        !clause.contains(bad),
                        "clause {clause:?} must not contain {bad:?}"
                    );
                }
                // Only `Id` skips the secondary key, because it is already unique. The match is on
                // the comma so an `Id` clause is not credited with a tie-break it does not have.
                assert_eq!(
                    clause.matches(", id ASC").count(),
                    usize::from(sort != FactSort::Id),
                    "the tie-break must be appended to every key but the id itself: {clause:?}"
                );
            }
            // The projection is a literal too, and non-empty only for the computed key.
            assert_eq!(
                sort.projection(),
                if sort == FactSort::TagCount {
                    ", array::len(tags) AS tag_count"
                } else {
                    ""
                }
            );
            for bad in forbidden {
                assert!(!sort.projection().contains(bad) || sort == FactSort::TagCount);
            }
        }
        assert_eq!(SortDir::Asc.sql(), "ASC");
        assert_eq!(SortDir::Desc.sql(), "DESC");

        // A hostile filter value cannot change the query text at all, because values are bound and
        // never interpolated — the built string is byte-identical to the benign one.
        let benign = MemoryRepo::list_query(
            Some("alpha"),
            Some("tag1"),
            false,
            FactSort::Content,
            SortDir::Asc,
        );
        let hostile = MemoryRepo::list_query(
            Some("a'; DROP TABLE fact; --"),
            Some("tag1, id DESC"),
            false,
            FactSort::Content,
            SortDir::Asc,
        );
        assert_eq!(benign, hostile);
        assert!(!benign.contains(';'));
    }

    /// Proves the TagCount path runs with both filters active, and prints the exact SQL the
    /// engine is sent (cargo captures it unless a test fails or `--nocapture` is used).
    #[tokio::test]
    async fn test_list_tag_count_with_search_and_tag_filter() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = MemoryRepo::new(db.inner());

        create_at(
            &repo,
            db.inner(),
            "alpha three",
            0.5,
            &["keep", "a", "b"],
            1,
        )
        .await;
        create_at(&repo, db.inner(), "alpha one", 0.5, &["keep"], 2).await;
        create_at(&repo, db.inner(), "alpha two", 0.5, &["keep", "a"], 3).await;
        // Matches the search only, and the tag only: both must be filtered out.
        create_at(&repo, db.inner(), "alpha untagged", 0.5, &[], 4).await;
        create_at(&repo, db.inner(), "zulu", 0.5, &["keep"], 5).await;

        let sql = MemoryRepo::list_query(
            Some("alp"),
            Some("keep"),
            false,
            FactSort::TagCount,
            SortDir::Desc,
        );
        println!("LIST QUERY sort=TagCount dir=Desc search+tag: {sql}");

        let rows = repo
            .list(FactListQuery {
                search: Some("alp"),
                tag: Some("keep"),
                sort: FactSort::TagCount,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            rows.iter().map(|f| f.content.clone()).collect::<Vec<_>>(),
            vec!["alpha three", "alpha two", "alpha one"]
        );
        assert_eq!(
            repo.count(Some("alp"), Some("keep"), false).await.unwrap(),
            rows.len(),
            "count() must agree with the filtered list"
        );
    }

    /// A null `created_at` must sort without erroring and without losing its row.
    ///
    /// `fact.created_at` ships as `TYPE datetime DEFAULT time::now()`, which cannot hold a null —
    /// but `Fact.created_at` is an `Option` and a row that predates or bypasses the default has to
    /// survive the ORDER BY. So this test overwrites the field on *its own* in-memory database to
    /// reach that state; no migration is involved. Checked on SurrealDB 3.2: `NULLS LAST` and
    /// `type::coalesce` do not parse inside ORDER BY, and nulls sort smaller than any datetime —
    /// first ascending, last descending. That is the placement we accept, and assert.
    #[tokio::test]
    async fn test_list_sorts_null_created_at_without_dropping_the_row() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = MemoryRepo::new(db.inner());

        db.inner()
            .query("DEFINE FIELD OVERWRITE created_at ON fact TYPE option<datetime>")
            .await
            .unwrap()
            .check()
            .unwrap();

        create_at(&repo, db.inner(), "jan", 0.5, &[], 1).await;
        create_at(&repo, db.inner(), "mar", 0.5, &[], 3).await;
        let nulled = create_at(&repo, db.inner(), "null", 0.5, &[], 0).await;

        // The fixture really does produce a null, or the test proves nothing.
        let stored = repo.get_fact(&nulled).await.unwrap().unwrap();
        assert_eq!(stored.created_at, None);

        let asc = sorted_contents(&repo, FactSort::CreatedAt, SortDir::Asc).await;
        assert_eq!(asc, vec!["null", "jan", "mar"]);
        let desc = sorted_contents(&repo, FactSort::CreatedAt, SortDir::Desc).await;
        assert_eq!(desc, vec!["mar", "jan", "null"]);
        for dir in [SortDir::Asc, SortDir::Desc] {
            assert_eq!(
                sorted_contents(&repo, FactSort::Confidence, dir)
                    .await
                    .len(),
                3,
                "a null created_at must not affect other sorts"
            );
        }
    }
}
