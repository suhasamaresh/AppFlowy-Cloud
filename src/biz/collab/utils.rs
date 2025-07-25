use app_error::AppError;
use appflowy_collaborate::ws2::WorkspaceCollabInstanceCache;
use collab::core::collab::{default_client_id, CollabOptions, DataSource};
use collab::preclude::Collab;
use collab_database::database::DatabaseBody;
use collab_database::database_trait::NoPersistenceDatabaseCollabService;
use collab_database::entity::FieldType;
use collab_database::fields::type_option_cell_reader;
use collab_database::fields::type_option_cell_writer;
use collab_database::fields::Field;
use collab_database::fields::TypeOptionCellReader;
use collab_database::fields::TypeOptionCellWriter;
use collab_database::fields::TypeOptionData;
use collab_database::fields::TypeOptions;
use collab_database::rows::meta_id_from_row_id;
use collab_database::rows::Cell;
use collab_database::rows::DatabaseRowBody;
use collab_database::rows::RowDetail;
use collab_database::rows::RowId;
use collab_database::rows::RowMetaKey;
use collab_database::template::timestamp_parse::TimestampCellData;
use collab_database::workspace_database::WorkspaceDatabaseBody;
use collab_document::document::Document;
use collab_document::importer::md_importer::MDImporter;
use collab_entity::CollabType;
use collab_entity::EncodedCollab;
use collab_folder::CollabOrigin;
use database::collab::select_workspace_database_oid;
use database::collab::CollabStore;
use database::collab::GetCollabOrigin;
use database_entity::dto::QueryCollab;
use database_entity::dto::QueryCollabResult;
use rayon::iter::IntoParallelIterator;
use rayon::iter::ParallelIterator;
use sqlx::PgPool;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{instrument, trace};
use uuid::Uuid;
use yrs::block::ClientID;
use yrs::Map;

pub const DEFAULT_SPACE_ICON: &str = "interface_essential/home-3";
pub const DEFAULT_SPACE_ICON_COLOR: &str = "0xFFA34AFD";

#[instrument(level = "debug", skip_all)]
pub fn get_row_details_serde(
  row_detail: RowDetail,
  field_by_id_name_uniq: &HashMap<String, Field>,
  type_option_reader_by_id: &HashMap<String, Box<dyn TypeOptionCellReader>>,
) -> HashMap<String, serde_json::Value> {
  let mut cells = row_detail.row.cells;
  let mut row_details_serde: HashMap<String, serde_json::Value> =
    HashMap::with_capacity(cells.len());

  trace!(
    "get_row_details_serde: row_id: {}, cells: {:#?}, field_by_id_name_uniq: {:#?}",
    row_detail.row.id,
    cells,
    field_by_id_name_uniq
  );
  for (field_id, field) in field_by_id_name_uniq {
    let cell: Cell = match cells.remove(field_id) {
      Some(cell) => cell.clone(),
      None => {
        let field_type = FieldType::from(field.field_type);
        match field_type {
          FieldType::CreatedTime => {
            TimestampCellData::new(Some(row_detail.row.created_at)).to_cell(field_type)
          },
          FieldType::LastEditedTime => {
            TimestampCellData::new(Some(row_detail.row.modified_at)).to_cell(field_type)
          },
          _ => Cell::new(),
        }
      },
    };
    let cell_value = match type_option_reader_by_id.get(&field.id) {
      Some(tor) => tor.json_cell(&cell),
      None => {
        tracing::error!("Failed to get type option reader by id: {}", field.id);
        serde_json::Value::Null
      },
    };
    row_details_serde.insert(field.name.clone(), cell_value);
  }

  row_details_serde
}

/// create a map of field name to field
/// if the field name is repeated, it will be appended with the field id,
pub fn field_by_name_uniq(mut fields: Vec<Field>) -> HashMap<String, Field> {
  fields.sort_by_key(|a| a.id.clone());
  let mut uniq_name_set: HashSet<String> = HashSet::with_capacity(fields.len());
  let mut field_by_name: HashMap<String, Field> = HashMap::with_capacity(fields.len());

  for field in fields {
    // if the name already exists, append the field id to the name
    let name = if uniq_name_set.contains(&field.name) {
      format!("{}-{}", field.name, field.id)
    } else {
      field.name.clone()
    };
    uniq_name_set.insert(name.clone());
    field_by_name.insert(name, field);
  }
  field_by_name
}

/// create a map of field id to field name, and ensure that the field name is unique.
/// if the field name is repeated, it will be appended with the field id,
/// under practical usage circumstances, no other collision should occur
pub fn field_by_id_name_uniq(mut fields: Vec<Field>) -> HashMap<String, Field> {
  fields.sort_by_key(|a| a.id.clone());
  let mut uniq_name_set: HashSet<String> = HashSet::with_capacity(fields.len());
  let mut field_by_id: HashMap<String, Field> = HashMap::with_capacity(fields.len());

  for mut field in fields {
    // if the name already exists, append the field id to the name
    if uniq_name_set.contains(&field.name) {
      let new_name = format!("{}-{}", field.name, field.id);
      field.name.clone_from(&new_name);
    }
    uniq_name_set.insert(field.name.clone());
    field_by_id.insert(field.id.clone(), field);
  }
  field_by_id
}

/// create a map type option writer by field id
pub fn type_option_writer_by_id(
  fields: &[Field],
) -> HashMap<String, Box<dyn TypeOptionCellWriter>> {
  let mut type_option_reader_by_id: HashMap<String, Box<dyn TypeOptionCellWriter>> =
    HashMap::with_capacity(fields.len());
  for field in fields {
    let field_id: String = field.id.clone();
    let type_option_reader: Box<dyn TypeOptionCellWriter> = {
      let field_type: &FieldType = &FieldType::from(field.field_type);
      let type_option_data: TypeOptionData = match field.get_any_type_option(field_type.type_id()) {
        Some(tod) => tod.clone(),
        None => HashMap::new(),
      };
      type_option_cell_writer(type_option_data, field_type)
    };
    type_option_reader_by_id.insert(field_id, type_option_reader);
  }
  type_option_reader_by_id
}

/// create a map type option reader by field id
pub fn type_option_reader_by_id(
  fields: &[Field],
) -> HashMap<String, Box<dyn TypeOptionCellReader>> {
  let mut type_option_reader_by_id: HashMap<String, Box<dyn TypeOptionCellReader>> =
    HashMap::with_capacity(fields.len());
  for field in fields {
    let field_id: String = field.id.clone();
    let type_option_reader: Box<dyn TypeOptionCellReader> = {
      let field_type: &FieldType = &FieldType::from(field.field_type);
      let type_option_data: TypeOptionData = match field.get_any_type_option(field_type.type_id()) {
        Some(tod) => tod.clone(),
        None => HashMap::new(),
      };
      type_option_cell_reader(type_option_data, field_type)
    };
    type_option_reader_by_id.insert(field_id, type_option_reader);
  }
  type_option_reader_by_id
}

pub fn type_options_serde(
  type_options: &TypeOptions,
  field_type: &FieldType,
) -> HashMap<String, serde_json::Value> {
  let type_option = match type_options.get(&field_type.type_id()) {
    Some(type_option) => type_option,
    None => return HashMap::new(),
  };

  let mut result = HashMap::with_capacity(type_option.len());
  for (key, value) in type_option {
    match field_type {
      FieldType::SingleSelect | FieldType::MultiSelect | FieldType::Media => {
        // Certain type option are stored as stringified JSON
        // We need to parse them back to JSON
        // e.g. "{ \"key\": \"value\" }" -> { "key": "value" }
        if let yrs::Any::String(arc_str) = value {
          if let Ok(serde_value) = serde_json::from_str::<serde_json::Value>(arc_str) {
            result.insert(key.clone(), serde_value);
          }
        }
      },
      _ => {
        result.insert(key.clone(), serde_json::to_value(value).unwrap_or_default());
      },
    }
  }

  result
}

pub async fn get_latest_collab_database_row_body(
  collab_storage: &Arc<dyn CollabStore>,
  workspace_id: Uuid,
  db_row_id: Uuid,
) -> Result<(Collab, DatabaseRowBody), AppError> {
  let mut db_row_collab = get_latest_collab(
    collab_storage,
    GetCollabOrigin::Server,
    workspace_id,
    db_row_id,
    CollabType::DatabaseRow,
    default_client_id(),
  )
  .await?;

  let row_id: RowId = db_row_id.to_string().into();
  let db_row_body = DatabaseRowBody::open(row_id, &mut db_row_collab).map_err(|err| {
    AppError::Internal(anyhow::anyhow!(
      "Failed to create database row body from collab, db_row_id: {}, err: {}",
      db_row_id,
      err
    ))
  })?;

  Ok((db_row_collab, db_row_body))
}

pub async fn get_latest_collab_database_body(
  collab_storage: &Arc<dyn CollabStore>,
  workspace_id: Uuid,
  database_id: Uuid,
) -> Result<(Collab, DatabaseBody), AppError> {
  let db_collab = get_latest_collab(
    collab_storage,
    GetCollabOrigin::Server,
    workspace_id,
    database_id,
    CollabType::Database,
    default_client_id(),
  )
  .await?;

  tokio::task::spawn_blocking(move || {
    let db_body = DatabaseBody::from_collab(
      &db_collab,
      Arc::new(NoPersistenceDatabaseCollabService::new(default_client_id())),
      None,
    )
    .ok_or_else(|| {
      AppError::Internal(anyhow::anyhow!(
        "Failed to create database body from collab, db_collab_id: {}",
        database_id,
      ))
    })?;
    Ok((db_collab, db_body))
  })
  .await?
}

#[instrument(level = "trace", skip_all)]
pub async fn get_latest_collab(
  collab_storage: &Arc<dyn CollabStore>,
  collab_origin: GetCollabOrigin,
  workspace_id: Uuid,
  object_id: Uuid,
  collab_type: CollabType,
  client_id: ClientID,
) -> Result<Collab, AppError> {
  let encode_collab = collab_storage
    .get_full_encode_collab(collab_origin, &workspace_id, &object_id, collab_type)
    .await
    .map(|v| v.encoded_collab)?;
  let options =
    CollabOptions::new(object_id.to_string(), client_id).with_data_source(encode_collab.into());
  let collab = Collab::new_with_options(CollabOrigin::Server, options)
    .map_err(|e| AppError::Unhandled(e.to_string()))?;
  Ok(collab)
}

pub async fn batch_get_latest_collab_encoded(
  collab_storage: &Arc<dyn CollabStore>,
  collab_origin: GetCollabOrigin,
  workspace_id: Uuid,
  oid_list: &[Uuid],
  collab_type: CollabType,
) -> Result<HashMap<Uuid, EncodedCollab>, AppError> {
  let uid = match collab_origin {
    GetCollabOrigin::User { uid } => uid,
    _ => 0,
  };
  let queries: Vec<QueryCollab> = oid_list
    .iter()
    .map(|row_id| QueryCollab {
      object_id: *row_id,
      collab_type,
    })
    .collect();
  let query_collab_results = collab_storage
    .batch_get_collab(&uid, workspace_id, queries)
    .await;
  let encoded_collabs = tokio::task::spawn_blocking(move || {
    let collabs: HashMap<_, EncodedCollab> = query_collab_results
      .into_par_iter()
      .filter_map(|(oid, query_collab_result)| match query_collab_result {
        QueryCollabResult::Success { encode_collab_v1 } => {
          let decoded_result = EncodedCollab::decode_from_bytes(&encode_collab_v1);
          match decoded_result {
            Ok(decoded) => Some((oid, decoded)),
            Err(err) => {
              tracing::error!("Failed to decode collab for row {}: {}", oid, err);
              None
            },
          }
        },
        QueryCollabResult::Failed { error } => {
          tracing::error!("Failed to get collab: {:?}", error);
          None
        },
      })
      .collect();
    collabs
  })
  .await?;
  Ok(encoded_collabs)
}

pub async fn get_latest_collab_workspace_database_body(
  pg_pool: &PgPool,
  storage: &Arc<dyn CollabStore>,
  origin: GetCollabOrigin,
  workspace_id: Uuid,
) -> Result<WorkspaceDatabaseBody, AppError> {
  let ws_db_oid = select_workspace_database_oid(pg_pool, &workspace_id).await?;
  let mut collab = get_latest_collab(
    storage,
    origin,
    workspace_id,
    ws_db_oid,
    CollabType::WorkspaceDatabase,
    default_client_id(),
  )
  .await?;
  let ws_db = WorkspaceDatabaseBody::open(&mut collab).map_err(|err| {
    AppError::Internal(anyhow::anyhow!(
      "Failed to open workspace database body: {}",
      err
    ))
  })?;
  Ok(ws_db)
}

pub const DUMMY_UID: i64 = 0;
pub async fn get_latest_collab_document(
  collab_storage: &Arc<dyn CollabStore>,
  collab_origin: GetCollabOrigin,
  workspace_id: Uuid,
  doc_oid: Uuid,
) -> Result<Document, AppError> {
  let doc_collab = get_latest_collab(
    collab_storage,
    collab_origin,
    workspace_id,
    doc_oid,
    CollabType::Document,
    default_client_id(),
  )
  .await?;
  Document::open(doc_collab).map_err(|e| {
    AppError::Internal(anyhow::anyhow!(
      "Failed to create document body from collab, doc_oid: {}, {}",
      doc_oid,
      e
    ))
  })
}

pub async fn collab_to_bin(collab: Collab, collab_type: CollabType) -> Result<Vec<u8>, AppError> {
  tokio::task::spawn_blocking(move || {
    let bin = collab
      .encode_collab_v1(|collab| collab_type.validate_require_data(collab))
      .map_err(|e| AppError::Unhandled(e.to_string()))?
      .encode_to_bytes()?;
    Ok(bin)
  })
  .await?
}

pub async fn collab_to_doc_state(
  collab: Collab,
  collab_type: CollabType,
) -> Result<Vec<u8>, AppError> {
  tokio::task::spawn_blocking(move || {
    let bin = collab
      .encode_collab_v1(|collab| collab_type.validate_require_data(collab))
      .map_err(|e| AppError::Unhandled(e.to_string()))?
      .doc_state
      .to_vec();
    Ok(bin)
  })
  .await?
}

pub fn collab_from_doc_state(
  doc_state: Vec<u8>,
  object_id: &Uuid,
  client_id: ClientID,
) -> Result<Collab, AppError> {
  let options = CollabOptions::new(object_id.to_string(), client_id)
    .with_data_source(DataSource::DocStateV1(doc_state));
  let collab = Collab::new_with_options(CollabOrigin::Server, options)
    .map_err(|e| AppError::Unhandled(e.to_string()))?;
  Ok(collab)
}

/// Base on values given by [cell_value_by_id], write to fields of DatabaseRowBody.
/// Returns encoded collab updates to the database row
#[instrument(level = "debug", skip_all)]
pub async fn write_to_database_row(
  db_body: &DatabaseBody,
  db_row_txn: &mut yrs::TransactionMut<'_>,
  db_row_body: &DatabaseRowBody,
  cell_value_by_id: HashMap<String, serde_json::Value>,
  modified_ts: i64,
) -> Result<(), AppError> {
  let all_fields = db_body.fields.get_all_fields(db_row_txn);
  let field_by_id = all_fields.iter().fold(HashMap::new(), |mut acc, field| {
    acc.insert(field.id.clone(), field.clone());
    acc
  });
  let type_option_reader_by_id = type_option_writer_by_id(&all_fields);
  let field_by_name = field_by_name_uniq(all_fields);

  // set last_modified
  db_row_body.update(db_row_txn, |row_update| {
    row_update.set_last_modified(modified_ts);
  });

  trace!(
    "insert {} cells, {} fields. values: {:#?}",
    cell_value_by_id.len(),
    field_by_id.len(),
    cell_value_by_id
  );
  // for each field given by user input, overwrite existing data
  for (id, serde_val) in cell_value_by_id {
    let field = match field_by_id.get(&id) {
      Some(f) => f,
      // try use field name if id not found
      None => match field_by_name.get(&id) {
        Some(f) => f,
        None => {
          tracing::warn!("Failed to get field by id or name for field: {}", id);
          continue;
        },
      },
    };
    let cell_writer = match type_option_reader_by_id.get(&field.id) {
      Some(cell_writer) => cell_writer,
      None => {
        tracing::error!("Failed to get type option writer for field: {}", field.id);
        continue;
      },
    };
    let new_cell: Cell = cell_writer.convert_json_to_cell(serde_val);
    trace!(
      "Writing cell for field: {}, value: {:?}",
      field.id,
      new_cell,
    );
    db_row_body.update(db_row_txn, |row_update| {
      row_update.update_cells(|cells_update| {
        cells_update.insert_cell(&field.id, new_cell);
      });
    });
  }
  Ok(())
}

pub async fn create_row_document(
  workspace_id: Uuid,
  uid: i64,
  new_doc_id: Uuid,
  collab_instance_cache: &impl WorkspaceCollabInstanceCache,
  row_doc_content: String,
) -> Result<CreatedRowDocument, AppError> {
  let md_importer = MDImporter::new(None);
  let client_id = default_client_id();
  let new_doc_id_str = new_doc_id.to_string();
  let doc_data = md_importer
    .import(&new_doc_id_str, row_doc_content)
    .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to import markdown: {:?}", e)))?;
  let doc = Document::create(&new_doc_id_str, doc_data, client_id)
    .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to create document: {:?}", e)))?;
  let doc_ec = doc.encode_collab().map_err(|e| {
    AppError::Internal(anyhow::anyhow!("Failed to encode document collab: {:?}", e))
  })?;

  let mut folder = collab_instance_cache.get_folder(workspace_id).await?;
  let folder_updates = {
    let mut folder_txn = folder.collab.transact_mut();
    folder.body.views.insert(
      &mut folder_txn,
      collab_folder::View::orphan_view(
        &new_doc_id_str,
        collab_folder::ViewLayout::Document,
        Some(uid),
      ),
      None,
      uid,
    );
    folder_txn.encode_update_v1()
  };

  let doc_ec_bytes = doc_ec
    .encode_to_bytes()
    .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to encode db doc: {:?}", e)))?;

  Ok(CreatedRowDocument {
    folder_updates,
    doc_ec_bytes,
  })
}

pub enum DocChanges {
  Update(Vec<u8>, Vec<u8>), // (updated_doc, doc_update)
  Insert(CreatedRowDocument),
}

#[allow(clippy::too_many_arguments)]
pub async fn get_database_row_doc_changes(
  collab_storage: &Arc<dyn CollabStore>,
  collab_instance_cache: &impl WorkspaceCollabInstanceCache,
  workspace_id: Uuid,
  row_doc_content: Option<String>,
  db_row_body: &DatabaseRowBody,
  db_row_txn: &mut yrs::TransactionMut<'_>,
  row_id: &Uuid,
  uid: i64,
) -> Result<Option<(Uuid, DocChanges)>, AppError> {
  let row_doc_content = match row_doc_content {
    Some(row_doc_content) if !row_doc_content.is_empty() => row_doc_content,
    _ => return Ok(None),
  };

  let doc_id = db_row_body
    .document_id(db_row_txn)
    .map_err(|err| AppError::Internal(anyhow::anyhow!("Failed to get document id: {:?}", err)))?;

  match doc_id {
    Some(doc_id) => {
      let doc_uuid = Uuid::parse_str(&doc_id)?;
      let cur_doc = get_latest_collab_document(
        collab_storage,
        GetCollabOrigin::Server,
        workspace_id,
        doc_uuid,
      )
      .await?;

      let md_importer = MDImporter::new(None);
      let new_doc_data = md_importer
        .import(&doc_id, row_doc_content)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to import markdown: {:?}", e)))?;
      let new_doc = Document::create(&doc_id, new_doc_data, default_client_id())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to create document: {:?}", e)))?;

      // if the document content is the same, there is no need to update
      if cur_doc.paragraphs() == new_doc.paragraphs() {
        return Ok(None);
      };

      let (mut cur_doc_collab, mut cur_doc_body) = cur_doc.split();

      let doc_update = {
        let mut txn = cur_doc_collab.context.transact_mut();
        let new_doc_data = new_doc.get_document_data().map_err(|e| {
          AppError::Internal(anyhow::anyhow!("Failed to get document data: {:?}", e))
        })?;
        cur_doc_body
          .reset_with_data(&mut txn, Some(new_doc_data))
          .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to reset document: {:?}", e)))?;
        txn.encode_update_v1()
      };

      // Clear undo manager state to save space
      if let Ok(undo_mgr) = cur_doc_collab.undo_manager_mut() {
        undo_mgr.clear();
      }

      let updated_doc = collab_to_bin(cur_doc_collab, CollabType::Document).await?;
      Ok(Some((
        doc_uuid,
        DocChanges::Update(updated_doc, doc_update),
      )))
    },
    None => {
      // update row to indicate that the document is not empty
      let is_document_empty_id = meta_id_from_row_id(row_id, RowMetaKey::IsDocumentEmpty);
      db_row_body
        .get_meta()
        .insert(db_row_txn, is_document_empty_id, false);

      // get document id
      let new_doc_id = db_row_body
        .document_id(db_row_txn)
        .map_err(|err| AppError::Internal(anyhow::anyhow!("Failed to get document id: {:?}", err)))?
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("Failed to get document id")))?;

      let new_doc_id = Uuid::parse_str(&new_doc_id)?;
      let created_row_doc: CreatedRowDocument = create_row_document(
        workspace_id,
        uid,
        new_doc_id,
        collab_instance_cache,
        row_doc_content,
      )
      .await?;
      Ok(Some((new_doc_id, DocChanges::Insert(created_row_doc))))
    },
  }
}

pub struct CreatedRowDocument {
  // pub updated_folder: Vec<u8>,
  pub folder_updates: Vec<u8>,
  pub doc_ec_bytes: Vec<u8>,
}
