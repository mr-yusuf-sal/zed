use crate::driver::{Driver, ObjectKind, ObjectNode, ResultSet};
use anyhow::{Context as _, Result, bail};
use futures::TryStreamExt as _;
use mongodb::bson::{Bson, Document, doc};
use ui::SharedString;

/// MongoDB stores documents, not rows, and two documents in one collection need not share a
/// single field. Forcing them into a fixed grid would either drop fields or invent nulls, so
/// the driver takes the union of the keys it actually sees across the page it fetched and uses
/// that as the column set. A field missing from a document stays NULL, which is the truth.
///
/// Nested documents and arrays are rendered as compact JSON inside their cell rather than
/// flattened into more columns: flattening turns one wide document into hundreds of mostly
/// empty columns and makes the common case unreadable.
pub struct MongoDriver {
    client: mongodb::Client,
    default_database: Option<String>,
}

impl MongoDriver {
    pub async fn connect(
        host: &str,
        port: u16,
        database: Option<&str>,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<Self> {
        let credentials = match (username, password) {
            (Some(user), Some(password)) => format!("{user}:{password}@"),
            (Some(user), None) => format!("{user}@"),
            _ => String::new(),
        };
        let uri = format!("mongodb://{credentials}{host}:{port}");
        let client = mongodb::Client::with_uri_str(&uri)
            .await
            .with_context(|| format!("connecting to mongodb at {host}:{port}"))?;

        // with_uri_str is lazy, so ping to turn a bad host or password into an error here
        // rather than on the first expansion of the tree.
        client
            .database(database.unwrap_or("admin"))
            .run_command(doc! { "ping": 1 })
            .await
            .context("pinging the server")?;

        Ok(Self {
            client,
            default_database: database.map(str::to_string),
        })
    }
}

/// One cell's worth of a BSON value. Scalars read as themselves; anything structured is JSON so
/// that what is in the cell is unambiguous rather than a lossy summary.
fn render(value: &Bson) -> Option<SharedString> {
    match value {
        Bson::Null | Bson::Undefined => None,
        Bson::String(text) => Some(SharedString::from(text.clone())),
        Bson::Int32(n) => Some(SharedString::from(n.to_string())),
        Bson::Int64(n) => Some(SharedString::from(n.to_string())),
        Bson::Double(n) => Some(SharedString::from(n.to_string())),
        Bson::Boolean(b) => Some(SharedString::from(b.to_string())),
        Bson::ObjectId(id) => Some(SharedString::from(id.to_hex())),
        Bson::DateTime(at) => Some(SharedString::from(
            at.try_to_rfc3339_string()
                .unwrap_or_else(|_| at.timestamp_millis().to_string()),
        )),
        other => Some(SharedString::from(
            serde_json::to_string(&other.clone().into_relaxed_extjson())
                .unwrap_or_else(|_| format!("{other:?}")),
        )),
    }
}

fn to_result_set(documents: Vec<Document>, truncated: bool) -> ResultSet {
    // Column order follows first appearance so the fields of the first document read in the
    // order they were stored, which is usually the order someone expects to see them.
    let mut columns: Vec<SharedString> = Vec::new();
    for document in &documents {
        for key in document.keys() {
            let key = SharedString::from(key.clone());
            if !columns.contains(&key) {
                columns.push(key);
            }
        }
    }

    let rows = documents
        .iter()
        .map(|document| {
            columns
                .iter()
                .map(|column| document.get(column.as_ref()).and_then(render))
                .collect()
        })
        .collect();

    ResultSet {
        columns,
        rows,
        rows_affected: None,
        notices: Vec::new(),
        truncated,
    }
}

#[async_trait::async_trait]
impl Driver for MongoDriver {
    async fn children(&self, path: &[SharedString]) -> Result<Vec<ObjectNode>> {
        match path {
            [] => {
                let names = self
                    .client
                    .list_database_names()
                    .await
                    .context("listing databases")?;
                Ok(names
                    .into_iter()
                    .filter(|name| !matches!(name.as_str(), "admin" | "local" | "config"))
                    .map(|name| {
                        let name = SharedString::from(name);
                        ObjectNode {
                            path: vec![name.clone()],
                            name,
                            kind: ObjectKind::Database,
                            has_children: true,
                            detail: None,
                        }
                    })
                    .collect())
            }
            [database] => {
                let names = self
                    .client
                    .database(database)
                    .list_collection_names()
                    .await
                    .with_context(|| format!("listing collections in {database}"))?;
                Ok(names
                    .into_iter()
                    .map(|name| {
                        let name = SharedString::from(name);
                        ObjectNode {
                            path: vec![database.clone(), name.clone()],
                            name,
                            kind: ObjectKind::Collection,
                            // A collection has no schema to expand into.
                            has_children: false,
                            detail: None,
                        }
                    })
                    .collect())
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Takes a raw command document, the same thing `db.runCommand(...)` takes, because that is
    /// the one form that can express every operation rather than a hand-rolled subset.
    async fn query(&self, text: &str, row_limit: usize) -> Result<ResultSet> {
        let command: Document = serde_json::from_str::<serde_json::Value>(text)
            .context("the query must be a JSON command document, e.g. {\"find\": \"users\"}")
            .and_then(|json| {
                mongodb::bson::to_document(&json).context("converting the command to BSON")
            })?;

        let database = command
            .get_str("$db")
            .ok()
            .map(str::to_string)
            .or_else(|| self.default_database.clone())
            .context("no database: add \"$db\" to the command or put one in the connection URL")?;

        let response = self
            .client
            .database(&database)
            .run_command(command)
            .await
            .context("running the command")?;

        // A find returns its documents inside cursor.firstBatch; everything else is better
        // shown as the single response document it is.
        if let Ok(cursor) = response.get_document("cursor") {
            if let Ok(batch) = cursor.get_array("firstBatch") {
                let truncated = batch.len() > row_limit;
                let documents = batch
                    .iter()
                    .take(row_limit)
                    .filter_map(|value| value.as_document().cloned())
                    .collect();
                return Ok(to_result_set(documents, truncated));
            }
        }
        Ok(to_result_set(vec![response], false))
    }

    async fn preview(&self, path: &[SharedString], row_limit: usize) -> Result<ResultSet> {
        let [database, collection, ..] = path else {
            bail!("expected a database and a collection, got {path:?}");
        };
        let cursor = self
            .client
            .database(database)
            .collection::<Document>(collection)
            .find(doc! {})
            // One more than asked for is how the view learns the page was cut short.
            .limit(row_limit as i64 + 1)
            .await
            .with_context(|| format!("reading {database}.{collection}"))?;

        let mut documents: Vec<Document> = cursor.try_collect().await.context("reading rows")?;
        let truncated = documents.len() > row_limit;
        documents.truncate(row_limit);
        Ok(to_result_set(documents, truncated))
    }
}
