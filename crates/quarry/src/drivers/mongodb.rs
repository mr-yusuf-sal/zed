use crate::driver::{
    CatalogEntry, CellEdit, Driver, EditTarget, ObjectKind, ObjectNode, ResultSet,
};
use anyhow::{Context as _, Result, bail};
use futures::{TryStreamExt as _, future::join_all};
use mongodb::{
    bson::{Bson, DateTime, Document, doc, oid::ObjectId},
    options::{ClientOptions, Credential},
    results::CollectionType,
};
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

/// Databases every server has and nobody browses for their data.
const SYSTEM_DATABASES: [&str; 3] = ["admin", "local", "config"];

/// Commands whose reply is a cursor. These go through `run_cursor_command`, which follows
/// `getMore` in the cursor's own session; a hand-rolled `getMore` from another implicit
/// session is rejected by the server.
const CURSOR_COMMANDS: [&str; 4] = ["find", "aggregate", "listCollections", "listIndexes"];

/// Bounds the catalog on a database with an unreasonable number of collections, since each one
/// costs a sampled document.
const CATALOG_COLLECTION_LIMIT: usize = 200;

impl MongoDriver {
    pub async fn connect(
        host: &str,
        port: u16,
        database: Option<&str>,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<Self> {
        let mut options = ClientOptions::parse(format!("mongodb://{host}:{port}"))
            .await
            .with_context(|| format!("connecting to mongodb at {host}:{port}"))?;
        // Given as a credential rather than spliced into the URI, where a password containing
        // `@`, `:` or `/` would have to be percent-encoded to parse at all.
        if let Some(username) = username {
            options.credential = Some(
                Credential::builder()
                    .username(username.to_string())
                    .password(password.map(str::to_string))
                    .build(),
            );
        }
        let client = mongodb::Client::with_options(options)
            .with_context(|| format!("connecting to mongodb at {host}:{port}"))?;

        let database = database.filter(|database| !database.is_empty());
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

    /// Reads a command typed in the editor and decides which database it runs in.
    fn parse_command(&self, statement: &str, database: Option<&str>) -> Result<(Document, String)> {
        let json = serde_json::from_str::<serde_json::Value>(statement)
            .context("the query must be a JSON command document, e.g. {\"find\": \"users\"}")?;
        let serde_json::Value::Object(json) = json else {
            bail!("the query must be a JSON object, e.g. {{\"find\": \"users\"}}");
        };
        // Extended JSON, so `{"_id": {"$oid": "..."}}` in a filter is an ObjectId rather than a
        // nested document with a `$oid` field.
        let mut command = Document::try_from(json).context("converting the command to BSON")?;

        // A `$db` written into the command is the most specific choice there is, so it wins
        // over the editor's database picker. It is removed because the driver adds its own.
        let database = match command.remove("$db") {
            Some(Bson::String(database)) => Some(database),
            Some(other) => bail!("\"$db\" must be a string, got {other}"),
            None => None,
        }
        .or_else(|| database.map(str::to_string))
        .or_else(|| self.default_database.clone())
        .context(
            "no database: pick one, add \"$db\" to the command or put one in the connection",
        )?;
        Ok((command, database))
    }

    async fn collections(&self, database: &str) -> Result<Vec<(String, CollectionType)>> {
        let mut collections: Vec<(String, CollectionType)> = self
            .client
            .database(database)
            .list_collections()
            .await
            .with_context(|| format!("listing collections in {database}"))?
            .map_ok(|specification| (specification.name, specification.collection_type))
            .try_collect()
            .await
            .with_context(|| format!("listing collections in {database}"))?;
        collections.retain(|(name, _)| !name.starts_with("system."));
        collections.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(collections)
    }

    /// One document's top-level fields and their types. A collection has no schema, so one
    /// sample is a hint about its shape, not a description of it.
    async fn sample_fields(
        &self,
        database: &str,
        collection: &str,
    ) -> Result<Vec<(String, &'static str)>> {
        let document = self
            .client
            .database(database)
            .collection::<Document>(collection)
            .find_one(doc! {})
            .await
            .with_context(|| format!("sampling {database}.{collection}"))?;
        Ok(document
            .into_iter()
            .flat_map(|document| document.into_iter())
            .map(|(name, value)| (name, type_name(&value)))
            .collect())
    }
}

/// The names the shell and `$type` use, which are what someone reading the tree will know.
fn type_name(value: &Bson) -> &'static str {
    match value {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Array(_) => "array",
        Bson::Document(_) => "object",
        Bson::Boolean(_) => "bool",
        Bson::Null => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) => "javascript",
        Bson::JavaScriptCodeWithScope(_) => "javascriptWithScope",
        Bson::Int32(_) => "int",
        Bson::Int64(_) => "long",
        Bson::Timestamp(_) => "timestamp",
        Bson::Binary(_) => "binData",
        Bson::ObjectId(_) => "objectId",
        Bson::DateTime(_) => "date",
        Bson::Symbol(_) => "symbol",
        Bson::Decimal128(_) => "decimal",
        Bson::Undefined => "undefined",
        Bson::MaxKey => "maxKey",
        Bson::MinKey => "minKey",
        Bson::DbPointer(_) => "dbPointer",
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

/// A command as it is typed in the query editor: relaxed extended JSON, so an ObjectId reads
/// `{"$oid": ...}` and runs back as one, pretty-printed without blank lines, which would split it.
fn to_json(document: Document) -> String {
    let json = Bson::Document(document).into_relaxed_extjson();
    serde_json::to_string_pretty(&json).unwrap_or_else(|_| json.to_string())
}

/// Reads text typed into a cell back into BSON: extended JSON when it parses, so `42`, `true`,
/// `{"a": 1}` and `{"$oid": "..."}` keep their types, and a plain string otherwise.
fn parse_value(text: &str) -> Bson {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|json| Bson::try_from(json).ok())
        .unwrap_or_else(|| Bson::String(text.to_string()))
}

/// Every value a displayed `_id` could have been. The grid shows an ObjectId as bare hex and a
/// number as digits, so the text alone does not say which it was; the caller asks the server
/// which of these actually exists.
fn id_candidates(displayed: Option<&str>) -> Vec<Bson> {
    let Some(displayed) = displayed else {
        return vec![Bson::Null];
    };
    let mut candidates = Vec::new();
    if let Ok(id) = ObjectId::parse_str(displayed) {
        candidates.push(Bson::ObjectId(id));
    }
    let parsed = parse_value(displayed);
    if !candidates.contains(&parsed) {
        candidates.push(parsed);
    }
    let text = Bson::String(displayed.to_string());
    if !candidates.contains(&text) {
        candidates.push(text);
    }
    candidates
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
        truncated,
        documents: documents
            .into_iter()
            .map(|document| Some(SharedString::from(to_json(document))))
            .collect(),
        ..ResultSet::default()
    }
}

/// Write commands report success with `ok: 1` even when individual writes failed, so the
/// failures have to be dug out of `writeErrors` to be noticed at all.
fn write_errors(response: &Document) -> Option<String> {
    let errors = response.get_array("writeErrors").ok()?;
    let messages: Vec<String> = errors
        .iter()
        .filter_map(|error| error.as_document()?.get_str("errmsg").ok())
        .map(str::to_string)
        .collect();
    (!messages.is_empty()).then(|| messages.join("; "))
}

fn count(response: &Document, field: &str) -> Option<u64> {
    match response.get(field)? {
        Bson::Int32(n) => u64::try_from(*n).ok(),
        Bson::Int64(n) => u64::try_from(*n).ok(),
        Bson::Double(n) => Some(*n as u64),
        _ => None,
    }
}

#[async_trait::async_trait]
impl Driver for MongoDriver {
    async fn children(&self, path: &[SharedString]) -> Result<Vec<ObjectNode>> {
        match path {
            [] => Ok(self
                .databases()
                .await?
                .into_iter()
                .map(|name| ObjectNode {
                    path: vec![name.clone()],
                    name,
                    kind: ObjectKind::Database,
                    has_children: true,
                    detail: None,
                })
                .collect()),
            [database] => {
                let collections = self.collections(database).await?;
                let database_handle = self.client.database(database);
                // Estimated counts come from collection metadata, not a scan, so asking for all
                // of them at once is cheap. A view has no metadata count and is left blank.
                let counts = join_all(collections.iter().map(|(name, kind)| {
                    let collection = database_handle.collection::<Document>(name);
                    let is_view = matches!(kind, CollectionType::View);
                    async move {
                        if is_view {
                            None
                        } else {
                            collection.estimated_document_count().await.ok()
                        }
                    }
                }))
                .await;
                Ok(collections
                    .into_iter()
                    .zip(counts)
                    .map(|((name, kind), count)| {
                        let name = SharedString::from(name);
                        ObjectNode {
                            path: vec![database.clone(), name.clone()],
                            name,
                            kind: ObjectKind::Collection,
                            has_children: count != Some(0),
                            detail: match (kind, count) {
                                (CollectionType::View, _) => Some(SharedString::from("view")),
                                (_, Some(count)) => {
                                    Some(SharedString::from(format!("~{count} docs")))
                                }
                                _ => None,
                            },
                        }
                    })
                    .collect())
            }
            [database, collection] => Ok(self
                .sample_fields(database, collection)
                .await?
                .into_iter()
                .map(|(field, kind)| {
                    let name = SharedString::from(field);
                    ObjectNode {
                        path: vec![database.clone(), collection.clone(), name.clone()],
                        name,
                        kind: ObjectKind::Column,
                        has_children: false,
                        detail: Some(SharedString::from(kind)),
                    }
                })
                .collect()),
            _ => Ok(Vec::new()),
        }
    }

    /// Takes a raw command document, the same thing `db.runCommand(...)` takes, because that is
    /// the one form that can express every operation rather than a hand-rolled subset.
    async fn query(
        &self,
        statement: &str,
        database: Option<&str>,
        row_limit: usize,
    ) -> Result<Vec<ResultSet>> {
        let (command, database) = self.parse_command(statement, database)?;
        let database = self.client.database(&database);

        let command_name = command.keys().next().cloned().unwrap_or_default();
        if CURSOR_COMMANDS.contains(&command_name.as_str()) && !command.contains_key("explain") {
            let mut cursor = database
                .run_cursor_command(command)
                .await
                .context("running the command")?;
            let mut documents = Vec::new();
            // One more than asked for is how the view learns the page was cut short. Dropping
            // the cursor afterwards makes the driver kill it on the server.
            while documents.len() <= row_limit
                && let Some(document) = cursor.try_next().await.context("reading results")?
            {
                documents.push(document);
            }
            let truncated = documents.len() > row_limit;
            documents.truncate(row_limit);
            return Ok(vec![to_result_set(documents, truncated)]);
        }

        let response = database
            .run_command(command)
            .await
            .context("running the command")?;
        if let Some(errors) = write_errors(&response) {
            bail!("{errors}");
        }
        let rows_affected = match command_name.as_str() {
            "update" => count(&response, "nModified"),
            "insert" | "delete" => count(&response, "n"),
            _ => None,
        };
        if let Some(rows_affected) = rows_affected {
            return Ok(vec![ResultSet {
                rows_affected: Some(rows_affected),
                ..ResultSet::default()
            }]);
        }
        Ok(vec![to_result_set(vec![response], false)])
    }

    /// Wraps the command in `explain`. The plan is a deeply nested document, so it is shown as
    /// indented JSON, one line per row, rather than squeezed into one cell.
    async fn explain(
        &self,
        statement: &str,
        database: Option<&str>,
        analyze: bool,
    ) -> Result<Vec<ResultSet>> {
        let (command, database) = self.parse_command(statement, database)?;
        let verbosity = if analyze {
            "executionStats"
        } else {
            "queryPlanner"
        };
        let plan = self
            .client
            .database(&database)
            .run_command(doc! { "explain": command, "verbosity": verbosity })
            .await
            .context("explaining the command")?;
        Ok(vec![ResultSet {
            columns: vec![SharedString::from("plan")],
            rows: to_json(plan)
                .lines()
                .map(|line| vec![Some(SharedString::from(line.to_string()))])
                .collect(),
            ..ResultSet::default()
        }])
    }

    async fn preview_page(
        &self,
        path: &[SharedString],
        row_limit: usize,
        offset: usize,
    ) -> Result<ResultSet> {
        let [database, collection, ..] = path else {
            bail!("expected a database and a collection, got {path:?}");
        };
        let cursor = self
            .client
            .database(database)
            .collection::<Document>(collection)
            .find(doc! {})
            .skip(offset as u64)
            // One more than asked for is how the view learns the page was cut short.
            .limit(row_limit as i64 + 1)
            .await
            .with_context(|| format!("reading {database}.{collection}"))?;

        let mut documents: Vec<Document> = cursor.try_collect().await.context("reading rows")?;
        let truncated = documents.len() > row_limit;
        documents.truncate(row_limit);
        let mut result = to_result_set(documents, truncated);
        result.edit_target = result
            .columns
            .iter()
            .position(|column| column.as_ref() == "_id")
            .map(|id_column| EditTarget {
                path: vec![database.clone(), collection.clone()],
                key_columns: vec![id_column],
                read_only_columns: vec![id_column],
            });
        Ok(result)
    }

    async fn databases(&self) -> Result<Vec<SharedString>> {
        let mut names: Vec<String> = self
            .client
            .list_database_names()
            .await
            .context("listing databases")?
            .into_iter()
            .filter(|name| !SYSTEM_DATABASES.contains(&name.as_str()))
            .collect();
        // A database that does not exist yet still belongs in the picker if the connection
        // names it: the first insert is what creates it.
        if let Some(default_database) = &self.default_database
            && !names.contains(default_database)
        {
            names.push(default_database.clone());
        }
        names.sort();
        Ok(names.into_iter().map(SharedString::from).collect())
    }

    fn default_database(&self) -> Option<SharedString> {
        self.default_database.clone().map(SharedString::from)
    }

    async fn catalog(&self, database: Option<&str>) -> Result<Vec<CatalogEntry>> {
        let Some(database) = database
            .map(str::to_string)
            .or_else(|| self.default_database.clone())
        else {
            return Ok(Vec::new());
        };
        let mut collections = self.collections(&database).await?;
        collections.truncate(CATALOG_COLLECTION_LIMIT);
        let database_name = SharedString::from(database.clone());

        let fields = join_all(
            collections
                .iter()
                .map(|(name, _)| self.sample_fields(&database, name)),
        )
        .await;

        let mut entries = Vec::new();
        for ((name, _), fields) in collections.into_iter().zip(fields) {
            let collection = SharedString::from(name);
            entries.push(CatalogEntry {
                label: collection.clone(),
                kind: ObjectKind::Collection,
                parent: Some(database_name.clone()),
                detail: None,
            });
            // A collection that cannot be sampled, a view over a missing source for instance,
            // still has a name worth completing.
            for (field, kind) in fields.unwrap_or_default() {
                entries.push(CatalogEntry {
                    label: SharedString::from(field),
                    kind: ObjectKind::Column,
                    parent: Some(collection.clone()),
                    detail: Some(SharedString::from(kind)),
                });
            }
        }
        Ok(entries)
    }

    /// The commands that recreate the collection: `create` with whatever options it has (a
    /// validator, capping, a view's pipeline), then `createIndexes` for every index but `_id_`,
    /// which `create` makes itself.
    async fn create_script(&self, path: &[SharedString]) -> Result<String> {
        let [database, collection, ..] = path else {
            bail!("expected a database and a collection, got {path:?}");
        };
        let database_handle = self.client.database(database);
        let specification = database_handle
            .list_collections()
            .filter(doc! { "name": collection.as_ref() })
            .await
            .with_context(|| format!("reading {database}.{collection}"))?
            .try_next()
            .await
            .with_context(|| format!("reading {database}.{collection}"))?
            .with_context(|| format!("{database}.{collection} was not found"))?;

        let mut create = doc! { "create": collection.as_ref() };
        let options = mongodb::bson::to_document(&specification.options)
            .context("reading the collection's options")?;
        for (name, value) in options {
            if !matches!(value, Bson::Null) {
                create.insert(name, value);
            }
        }
        let mut script = vec![to_json(create)];

        if !matches!(specification.collection_type, CollectionType::View) {
            let indexes: Vec<Document> = database_handle
                .run_cursor_command(doc! { "listIndexes": collection.as_ref() })
                .await
                .with_context(|| format!("listing the indexes of {collection}"))?
                .try_collect()
                .await
                .with_context(|| format!("listing the indexes of {collection}"))?;
            let indexes: Vec<Bson> = indexes
                .into_iter()
                .filter(|index| index.get_str("name").ok() != Some("_id_"))
                .map(|mut index| {
                    // The index format version and namespace are the server's to choose.
                    index.remove("v");
                    index.remove("ns");
                    Bson::Document(index)
                })
                .collect();
            if !indexes.is_empty() {
                script.push(to_json(doc! {
                    "createIndexes": collection.as_ref(),
                    "indexes": indexes,
                }));
            }
        }
        // A blank line between commands is what the editor splits MongoDB commands on.
        Ok(script.join("\n\n") + "\n")
    }

    fn qualified_name(&self, path: &[SharedString]) -> Option<String> {
        match path {
            [database, collection, ..] => Some(format!("{database}.{collection}")),
            [database] => Some(database.to_string()),
            [] => None,
        }
    }

    fn select_statement(&self, path: &[SharedString], limit: usize) -> Option<String> {
        let [_, collection] = path else {
            return None;
        };
        Some(to_json(
            doc! { "find": collection.as_ref(), "filter": {}, "limit": limit as i64 },
        ))
    }

    fn count_statement(&self, path: &[SharedString]) -> Option<String> {
        let [_, collection] = path else {
            return None;
        };
        Some(to_json(doc! { "count": collection.as_ref(), "query": {} }))
    }

    async fn update_cell(&self, edit: &CellEdit) -> Result<u64> {
        let [database, collection, ..] = edit.path.as_slice() else {
            bail!("expected a database and a collection, got {:?}", edit.path);
        };
        let field = edit.column.as_ref();
        if field == "_id" {
            bail!("_id cannot be changed; a document's identity is fixed");
        }
        // `$set` reads a dot as a path into a nested document and a leading `$` as an operator,
        // so such a column name would update something other than the cell that was edited.
        if field.contains('.') || field.starts_with('$') {
            bail!("the field {field:?} cannot be addressed by an update");
        }
        let displayed_id = edit
            .key
            .iter()
            .find(|(name, _)| name.as_ref() == "_id")
            .map(|(_, value)| value.as_ref().map(|value| value.as_ref()))
            .context("the edit does not say which document to change")?;

        let collection = self
            .client
            .database(database)
            .collection::<Document>(collection);
        let filter = doc! { "_id": { "$in": id_candidates(displayed_id) } };
        let matches: Vec<Document> = collection
            .find(filter)
            .limit(2)
            .await
            .context("finding the document")?
            .try_collect()
            .await
            .context("finding the document")?;
        let [document] = matches.as_slice() else {
            bail!(
                "{} documents match _id {}; refusing to guess which one to change",
                matches.len(),
                displayed_id.unwrap_or("null")
            );
        };
        let id = document.get("_id").cloned().unwrap_or(Bson::Null);

        let value = match edit.value.as_deref() {
            None => Bson::Null,
            // Dates and ObjectIds are displayed as plain text, so typing one back would store a
            // string. Keep the field's type when the text still reads as that type.
            Some(text) => match document.get(field) {
                Some(Bson::DateTime(_)) => DateTime::parse_rfc3339_str(text.trim())
                    .map(Bson::DateTime)
                    .unwrap_or_else(|_| parse_value(text)),
                Some(Bson::ObjectId(_)) => ObjectId::parse_str(text.trim())
                    .map(Bson::ObjectId)
                    .unwrap_or_else(|_| parse_value(text)),
                _ => parse_value(text),
            },
        };

        let mut assignment = Document::new();
        assignment.insert(field, value);
        let result = collection
            .update_one(doc! { "_id": id }, doc! { "$set": assignment })
            .await
            .context("updating the document")?;
        if result.matched_count != 1 {
            bail!(
                "the update matched {} documents instead of one",
                result.matched_count
            );
        }
        Ok(result.matched_count)
    }
}
