use core::fmt;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::SystemTime,
};

use anyhow::Result;
use indexify_internal_api as internal_api;
use internal_api::{ExtractorDescription, StateChange};
use rocksdb::OptimisticTransactionDB;

use super::{
    requests::{RequestPayload, StateChangeProcessed, StateMachineUpdateRequest},
    serializer::JsonEncode,
    store_utils::{decrement_running_task_count, increment_running_task_count},
    ContentId,
    ExecutorId,
    ExtractorName,
    JsonEncoder,
    NamespaceName,
    SchemaId,
    StateChangeId,
    StateMachineColumns,
    StateMachineError,
    TaskId,
};

#[derive(thiserror::Error, Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct IndexifyState {
    //  TODO: Check whether only id's can be stored in reverse indexes
    // Reverse Indexes
    /// The tasks that are currently unassigned
    pub unassigned_tasks: HashSet<TaskId>,

    /// State changes that have not been processed yet
    pub unprocessed_state_changes: HashSet<StateChangeId>,

    /// Namespace -> Content ID
    pub content_namespace_table: HashMap<NamespaceName, HashSet<ContentId>>,

    /// Namespace -> Extraction policy id
    pub extraction_policies_table: HashMap<NamespaceName, HashSet<String>>,

    /// Extractor -> Executors table
    pub extractor_executors_table: HashMap<ExtractorName, HashSet<ExecutorId>>,

    /// Namespace -> Index id
    pub namespace_index_table: HashMap<NamespaceName, HashSet<String>>,

    /// Tasks that are currently unfinished, by extractor. Once they are
    /// finished, they are removed from this set.
    pub unfinished_tasks_by_extractor: HashMap<ExtractorName, HashSet<TaskId>>,

    /// Number of tasks currently running on each executor
    pub executor_running_task_count: HashMap<ExecutorId, usize>,

    /// Namespace -> Schemas
    pub schemas_by_namespace: HashMap<NamespaceName, HashSet<SchemaId>>,
}

impl fmt::Display for IndexifyState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "IndexifyState {{ unassigned_tasks: {:?}, unprocessed_state_changes: {:?}, content_namespace_table: {:?}, extraction_policies_table: {:?}, extractor_executors_table: {:?}, namespace_index_table: {:?}, unfinished_tasks_by_extractor: {:?}, executor_running_task_count: {:?}, schemas_by_namespace: {:?} }}",
            self.unassigned_tasks,
            self.unprocessed_state_changes,
            self.content_namespace_table,
            self.extraction_policies_table,
            self.extractor_executors_table,
            self.namespace_index_table,
            self.unfinished_tasks_by_extractor,
            self.executor_running_task_count,
            self.schemas_by_namespace
        )
    }
}

impl IndexifyState {
    fn set_new_state_changes(
        &self,
        db: &Arc<OptimisticTransactionDB>,
        txn: &rocksdb::Transaction<OptimisticTransactionDB>,
        state_changes: &Vec<StateChange>,
    ) -> Result<(), StateMachineError> {
        for change in state_changes {
            let serialized_change = JsonEncoder::encode(change)?;
            txn.put_cf(
                StateMachineColumns::StateChanges.cf(db),
                &change.id,
                &serialized_change,
            )
            .map_err(|e| StateMachineError::DatabaseError(e.to_string()))?;
        }
        Ok(())
    }

    fn set_processed_state_changes(
        &self,
        db: &Arc<OptimisticTransactionDB>,
        txn: &rocksdb::Transaction<OptimisticTransactionDB>,
        state_changes: &Vec<StateChangeProcessed>,
    ) -> Result<(), StateMachineError> {
        let state_changes_cf = StateMachineColumns::StateChanges.cf(db);

        for change in state_changes {
            let result = txn
                .get_cf(state_changes_cf, &change.state_change_id)
                .map_err(|e| StateMachineError::DatabaseError(e.to_string()))?;
            let result = result
                .ok_or_else(|| StateMachineError::DatabaseError("State change not found".into()))?;

            let mut state_change = JsonEncoder::decode::<StateChange>(&result)?;
            state_change.processed_at = Some(change.processed_at);
            let serialized_change = JsonEncoder::encode(&state_change)?;
            txn.put_cf(
                state_changes_cf,
                &change.state_change_id,
                &serialized_change,
            )
            .map_err(|e| StateMachineError::DatabaseError(e.to_string()))?;
        }
        Ok(())
    }

    fn set_index(
        &self,
        db: &Arc<OptimisticTransactionDB>,
        txn: &rocksdb::Transaction<OptimisticTransactionDB>,
        index: &internal_api::Index,
        id: &String,
    ) -> Result<(), StateMachineError> {
        let serialized_index = JsonEncoder::encode(index)?;
        txn.put_cf(StateMachineColumns::IndexTable.cf(db), id, serialized_index)
            .map_err(|e| StateMachineError::DatabaseError(e.to_string()))?;
        Ok(())
    }

    fn _get_task(
        &self,
        db: &Arc<OptimisticTransactionDB>,
        txn: &rocksdb::Transaction<OptimisticTransactionDB>,
        task_id: &TaskId,
    ) -> Result<internal_api::Task, StateMachineError> {
        let serialized_task = txn
            .get_cf(StateMachineColumns::Tasks.cf(db), task_id)
            .map_err(|e| StateMachineError::DatabaseError(e.to_string()))?
            .ok_or_else(|| {
                StateMachineError::DatabaseError(format!("Task {} not found", task_id))
            })?;
        let task = JsonEncoder::decode(&serialized_task)?;
        Ok(task)
    }

    fn set_tasks(
        &self,
        db: &Arc<OptimisticTransactionDB>,
        txn: &rocksdb::Transaction<OptimisticTransactionDB>,
        tasks: &Vec<internal_api::Task>,
    ) -> Result<(), StateMachineError> {
        for task in tasks {
            let serialized_task = JsonEncoder::encode(task)?;
            txn.put_cf(
                StateMachineColumns::Tasks.cf(db),
                task.id.clone(),
                &serialized_task,
            )
            .map_err(|e| StateMachineError::DatabaseError(e.to_string()))?;
        }
        Ok(())
    }

    fn update_tasks(
        &self,
        db: &Arc<OptimisticTransactionDB>,
        txn: &rocksdb::Transaction<OptimisticTransactionDB>,
        tasks: Vec<&internal_api::Task>,
    ) -> Result<(), StateMachineError> {
        for task in tasks {
            let serialized_task = JsonEncoder::encode(task)?;
            txn.put_cf(
                StateMachineColumns::Tasks.cf(db),
                task.id.clone(),
                &serialized_task,
            )
            .map_err(|e| StateMachineError::DatabaseError(e.to_string()))?;
        }
        Ok(())
    }

    fn get_task_assignments_for_executor(
        &self,
        db: &Arc<OptimisticTransactionDB>,
        txn: &rocksdb::Transaction<OptimisticTransactionDB>,
        executor_id: &str,
    ) -> Result<HashSet<TaskId>, StateMachineError> {
        let value = txn
            .get_cf(StateMachineColumns::TaskAssignments.cf(db), executor_id)
            .map_err(|e| {
                StateMachineError::DatabaseError(format!("Error reading task assignments: {}", e))
            })?;
        match value {
            Some(existing_value) => {
                let existing_value: HashSet<TaskId> = JsonEncoder::decode(&existing_value)
                    .map_err(|e| {
                        StateMachineError::DatabaseError(format!(
                            "Error deserializing task assignments: {}",
                            e
                        ))
                    })?;
                Ok(existing_value)
            }
            None => Ok(HashSet::new()),
        }
    }

    /// Set the list of tasks that have been assigned to some executor
    fn set_task_assignments(
        &self,
        db: &Arc<OptimisticTransactionDB>,
        txn: &rocksdb::Transaction<OptimisticTransactionDB>,
        task_assignments: &HashMap<String, HashSet<TaskId>>,
    ) -> Result<(), StateMachineError> {
        let task_assignment_cf = StateMachineColumns::TaskAssignments.cf(db);
        for (executor_id, task_ids) in task_assignments {
            txn.put_cf(
                task_assignment_cf,
                executor_id,
                JsonEncoder::encode(&task_ids)?,
            )
            .map_err(|e| {
                StateMachineError::DatabaseError(format!("Error writing task assignments: {}", e))
            })?;
        }
        Ok(())
    }

    // FIXME USE MULTI-GET HERE
    fn delete_task_assignments_for_executor(
        &self,
        db: &Arc<OptimisticTransactionDB>,
        txn: &rocksdb::Transaction<OptimisticTransactionDB>,
        executor_id: &str,
    ) -> Result<Vec<TaskId>, StateMachineError> {
        let task_assignment_cf = StateMachineColumns::TaskAssignments.cf(db);
        let task_ids: Vec<TaskId> = txn
            .get_cf(task_assignment_cf, executor_id)
            .map_err(|e| {
                StateMachineError::DatabaseError(format!(
                    "Error reading task assignments for executor: {}",
                    e
                ))
            })?
            .map(|db_vec| {
                JsonEncoder::decode(&db_vec).map_err(|e| {
                    StateMachineError::DatabaseError(format!(
                        "Error deserializing task assignments for executor: {}",
                        e
                    ))
                })
            })
            .unwrap_or_else(|| Ok(Vec::new()))?;

        txn.delete_cf(task_assignment_cf, executor_id)
            .map_err(|e| {
                StateMachineError::DatabaseError(format!(
                    "Error deleting task assignments for executor: {}",
                    e
                ))
            })?;

        Ok(task_ids)
    }

    fn set_content(
        &self,
        db: &Arc<OptimisticTransactionDB>,
        txn: &rocksdb::Transaction<OptimisticTransactionDB>,
        contents_vec: &Vec<internal_api::ContentMetadata>,
    ) -> Result<(), StateMachineError> {
        for content in contents_vec {
            let serialized_content = JsonEncoder::encode(content)?;
            txn.put_cf(
                StateMachineColumns::ContentTable.cf(db),
                content.id.clone(),
                &serialized_content,
            )
            .map_err(|e| {
                StateMachineError::DatabaseError(format!("Error writing content: {}", e))
            })?;
        }
        Ok(())
    }

    fn set_executor(
        &self,
        db: &Arc<OptimisticTransactionDB>,
        txn: &rocksdb::Transaction<OptimisticTransactionDB>,
        addr: String,
        executor_id: &str,
        extractor: &ExtractorDescription,
        ts_secs: &u64,
    ) -> Result<(), StateMachineError> {
        let serialized_executor = JsonEncoder::encode(&internal_api::ExecutorMetadata {
            id: executor_id.into(),
            last_seen: *ts_secs,
            addr: addr.clone(),
            extractor: extractor.clone(),
        })?;
        txn.put_cf(
            StateMachineColumns::Executors.cf(db),
            executor_id,
            serialized_executor,
        )
        .map_err(|e| StateMachineError::DatabaseError(format!("Error writing executor: {}", e)))?;
        Ok(())
    }

    fn delete_executor(
        &self,
        db: &Arc<OptimisticTransactionDB>,
        txn: &rocksdb::Transaction<OptimisticTransactionDB>,
        executor_id: &str,
    ) -> Result<internal_api::ExecutorMetadata, StateMachineError> {
        //  Get a handle on the executor before deleting it from the DB
        let executors_cf = StateMachineColumns::Executors.cf(db);
        let serialized_executor = txn
            .get_cf(executors_cf, executor_id)
            .map_err(|e| {
                StateMachineError::DatabaseError(format!("Error reading executor: {}", e))
            })?
            .ok_or_else(|| {
                StateMachineError::DatabaseError(format!("Executor {} not found", executor_id))
            })?;
        let executor_meta =
            JsonEncoder::decode::<internal_api::ExecutorMetadata>(&serialized_executor)?;
        txn.delete_cf(executors_cf, executor_id).map_err(|e| {
            StateMachineError::DatabaseError(format!("Error deleting executor: {}", e))
        })?;
        Ok(executor_meta)
    }

    fn set_extractor(
        &self,
        db: &Arc<OptimisticTransactionDB>,
        txn: &rocksdb::Transaction<OptimisticTransactionDB>,
        extractor: &ExtractorDescription,
    ) -> Result<(), StateMachineError> {
        let serialized_extractor = JsonEncoder::encode(extractor)?;
        txn.put_cf(
            StateMachineColumns::Extractors.cf(db),
            &extractor.name,
            serialized_extractor,
        )
        .map_err(|e| StateMachineError::DatabaseError(format!("Error writing extractor: {}", e)))?;
        Ok(())
    }

    fn set_extraction_policy(
        &self,
        db: &Arc<OptimisticTransactionDB>,
        txn: &rocksdb::Transaction<OptimisticTransactionDB>,
        extraction_policy: &internal_api::ExtractionPolicy,
        updated_structured_data_schema: &Option<internal_api::StructuredDataSchema>,
        new_structured_data_schema: &internal_api::StructuredDataSchema,
    ) -> Result<(), StateMachineError> {
        let serialized_extraction_policy = JsonEncoder::encode(extraction_policy)?;
        txn.put_cf(
            &StateMachineColumns::ExtractionPolicies.cf(db),
            extraction_policy.id.clone(),
            serialized_extraction_policy,
        )
        .map_err(|e| {
            StateMachineError::DatabaseError(format!("Error writing extraction policy: {}", e))
        })?;
        if let Some(schema) = updated_structured_data_schema {
            self.set_schema(db, txn, schema)?
        }
        self.set_schema(db, txn, new_structured_data_schema)?;
        Ok(())
    }

    fn set_namespace(
        &self,
        db: &Arc<OptimisticTransactionDB>,
        txn: &rocksdb::Transaction<OptimisticTransactionDB>,
        namespace: &NamespaceName,
        structured_data_schema: &internal_api::StructuredDataSchema,
    ) -> Result<(), StateMachineError> {
        let serialized_name = JsonEncoder::encode(namespace)?;
        txn.put_cf(
            &StateMachineColumns::Namespaces.cf(db),
            namespace,
            serialized_name,
        )
        .map_err(|e| StateMachineError::DatabaseError(format!("Error writing namespace: {}", e)))?;
        self.set_schema(db, txn, structured_data_schema)?;
        Ok(())
    }

    fn set_schema(
        &self,
        db: &Arc<OptimisticTransactionDB>,
        txn: &rocksdb::Transaction<OptimisticTransactionDB>,
        schema: &internal_api::StructuredDataSchema,
    ) -> Result<(), StateMachineError> {
        let serialized_schema = JsonEncoder::encode(schema)?;
        txn.put_cf(
            &StateMachineColumns::StructuredDataSchemas.cf(db),
            schema.id.clone(),
            serialized_schema,
        )
        .map_err(|e| StateMachineError::DatabaseError(format!("Error writing schema: {}", e)))?;
        Ok(())
    }

    fn set_content_policies_applied_on_content(
        &self,
        db: &Arc<OptimisticTransactionDB>,
        txn: &rocksdb::Transaction<OptimisticTransactionDB>,
        mappings: &[internal_api::ContentExtractionPolicyMapping],
    ) -> Result<(), StateMachineError> {
        //  Fetch all values at once
        let mapping_cf = StateMachineColumns::ExtractionPoliciesAppliedOnContent.cf(db);
        let keys_with_cf: Vec<(_, _)> = mappings
            .iter()
            .map(|m| (mapping_cf, m.content_id.as_str()))
            .collect();
        let values = txn.multi_get_cf(keys_with_cf.clone());

        //  Iterate in memory and update the data
        let mut updated_mappings = Vec::new();
        for (index, value) in values.into_iter().enumerate() {
            let mut existing_mapping: internal_api::ContentExtractionPolicyMapping = match value {
                Ok(Some(data)) => JsonEncoder::decode(&data)?,
                Ok(None) => internal_api::ContentExtractionPolicyMapping {
                    content_id: keys_with_cf[index].1.to_string(),
                    extraction_policy_names: HashSet::new(),
                    time_of_policy_completion: HashMap::new(),
                },
                Err(e) => {
                    return Err(StateMachineError::DatabaseError(format!(
                        "Error getting the content policies applied on content id {}: {}",
                        keys_with_cf[index].1, e
                    )))
                }
            };

            let new_mapping = mappings[index].clone();
            existing_mapping
                .extraction_policy_names
                .extend(new_mapping.extraction_policy_names);
            existing_mapping
                .time_of_policy_completion
                .extend(new_mapping.time_of_policy_completion);

            updated_mappings.push(existing_mapping);
        }

        //  Write the data back
        for updated_mapping in updated_mappings {
            let data = JsonEncoder::encode(&updated_mapping)?;
            let key = updated_mapping.content_id;
            txn.put_cf(mapping_cf, key.clone(), data).map_err(|e| {
                StateMachineError::DatabaseError(format!(
                    "Error writing content policies applied on content for id {}: {}",
                    key, e
                ))
            })?;
        }

        Ok(())
    }

    pub fn mark_extraction_policy_applied_on_content(
        &self,
        db: &Arc<OptimisticTransactionDB>,
        txn: &rocksdb::Transaction<OptimisticTransactionDB>,
        content_id: &str,
        extraction_policy_name: &str,
        policy_completion_time: &SystemTime,
    ) -> Result<(), StateMachineError> {
        let mapping_cf = StateMachineColumns::ExtractionPoliciesAppliedOnContent.cf(db);
        let value = txn
            .get_cf(mapping_cf, content_id)
            .map_err(|e| {
                StateMachineError::DatabaseError(format!(
                    "Error getting the content policies applied on content id {}: {}",
                    content_id, e
                ))
            })?
            .ok_or_else(|| {
                StateMachineError::DatabaseError(format!(
                    "No content policies applied on content found for id {}",
                    content_id
                ))
            })?;
        let content_policy_mappings =
            JsonEncoder::decode::<internal_api::ContentExtractionPolicyMapping>(&value)?;

        //  First ensure that this content has the extraction policy registered against
        // it
        if !content_policy_mappings
            .extraction_policy_names
            .contains(extraction_policy_name)
        {
            return Err(StateMachineError::DatabaseError(format!(
                "Extraction policy {} not applied on content {} because extraction policy was not registered against the content",
                extraction_policy_name, content_id
            )));
        }

        //  Mark the time the content was processed against the extraction policy and
        // store it back
        let mut time_of_policy_completion = content_policy_mappings.time_of_policy_completion;
        time_of_policy_completion.insert(extraction_policy_name.into(), *policy_completion_time);
        let updated_mapping = internal_api::ContentExtractionPolicyMapping {
            content_id: content_id.into(),
            extraction_policy_names: content_policy_mappings.extraction_policy_names,
            time_of_policy_completion,
        };
        let data = JsonEncoder::encode(&updated_mapping)?;
        txn.put_cf(mapping_cf, content_id, data).map_err(|e| {
            StateMachineError::DatabaseError(format!(
                "Error writing content policies applied on content for id {}: {}",
                content_id, e
            ))
        })?;

        Ok(())
    }

    /// This method will make all state machine forward index writes to RocksDB
    pub fn apply_state_machine_updates(
        &mut self,
        request: StateMachineUpdateRequest,
        db: &Arc<OptimisticTransactionDB>,
    ) -> Result<(), StateMachineError> {
        let txn = db.transaction();

        self.set_new_state_changes(db, &txn, &request.new_state_changes)?;
        self.set_processed_state_changes(db, &txn, &request.state_changes_processed)?;

        match &request.payload {
            RequestPayload::CreateIndex {
                index,
                namespace: _,
                id,
            } => {
                self.set_index(db, &txn, index, id)?;
            }
            RequestPayload::CreateTasks { tasks } => {
                self.set_tasks(db, &txn, tasks)?;
            }
            RequestPayload::AssignTask { assignments } => {
                let assignments: HashMap<&String, HashSet<TaskId>> =
                    assignments
                        .iter()
                        .fold(HashMap::new(), |mut acc, (task_id, executor_id)| {
                            acc.entry(executor_id).or_default().insert(task_id.clone());
                            acc
                        });

                // FIXME - Write a test which assigns tasks mutliple times to the same executor
                // and make sure it's additive.

                for (executor_id, tasks) in assignments.iter() {
                    let mut existing_tasks =
                        self.get_task_assignments_for_executor(db, &txn, executor_id)?;
                    existing_tasks.extend(tasks.clone());
                    let task_assignment =
                        HashMap::from([(executor_id.to_string(), existing_tasks)]);
                    self.set_task_assignments(db, &txn, &task_assignment)?;
                }
            }
            RequestPayload::UpdateTask {
                task,
                mark_finished,
                executor_id,
                content_metadata,
            } => {
                self.update_tasks(db, &txn, vec![task])?;

                if *mark_finished {
                    //  If the task is meant to be marked finished and has an executor id, remove it
                    // from the list of tasks assigned to an executor
                    if let Some(executor_id) = executor_id {
                        let mut existing_tasks =
                            self.get_task_assignments_for_executor(db, &txn, executor_id)?;
                        existing_tasks.remove(&task.id);
                        let mut new_task_assignment = HashMap::new();
                        new_task_assignment.insert(executor_id.to_string(), existing_tasks);
                        self.set_task_assignments(db, &txn, &new_task_assignment)?;
                        decrement_running_task_count(
                            &mut self.executor_running_task_count,
                            executor_id,
                        );
                    }
                }

                //  Insert the content metadata into the db
                self.set_content(db, &txn, content_metadata)?;
            }
            RequestPayload::RegisterExecutor {
                addr,
                executor_id,
                extractor,
                ts_secs,
            } => {
                //  Insert the executor
                self.set_executor(db, &txn, addr.into(), executor_id, extractor, ts_secs)?;

                //  Insert the associated extractor
                self.set_extractor(db, &txn, extractor)?;
            }
            RequestPayload::RemoveExecutor { executor_id } => {
                //  NOTE: Special case of a handler that also remove its own reverse indexes
                // here and returns from this function  Doing this because
                // altering the reverse indexes requires references to the removed items

                //  Get a handle on the executor before deleting it from the DB
                let executor_meta = self.delete_executor(db, &txn, executor_id)?;

                // Remove all tasks assigned to this executor and get a handle on the task ids
                let task_ids = self.delete_task_assignments_for_executor(db, &txn, executor_id)?;

                txn.commit()
                    .map_err(|e| StateMachineError::TransactionError(e.to_string()))?;

                //  Remove the the extractor from the executor -> extractor mapping table
                let executors = self
                    .extractor_executors_table
                    .entry(executor_meta.extractor.name.clone())
                    .or_default();
                executors.remove(&executor_meta.id);

                //  Put the tasks of the deleted executor into the unassigned tasks list
                for task_id in task_ids {
                    self.unassigned_tasks.insert(task_id);
                }

                // Remove from the executor load table
                self.executor_running_task_count.remove(executor_id);

                return Ok(());
            }
            RequestPayload::CreateContent { content_metadata } => {
                self.set_content(db, &txn, content_metadata)?;
            }
            RequestPayload::CreateExtractionPolicy {
                extraction_policy,
                updated_structured_data_schema,
                new_structured_data_schema,
            } => {
                self.set_extraction_policy(
                    db,
                    &txn,
                    extraction_policy,
                    updated_structured_data_schema,
                    new_structured_data_schema,
                )?;
            }
            RequestPayload::SetContentExtractionPolicyMappings {
                content_extraction_policy_mappings,
            } => {
                self.set_content_policies_applied_on_content(
                    db,
                    &txn,
                    content_extraction_policy_mappings,
                )?;
            }
            RequestPayload::MarkExtractionPolicyAppliedOnContent {
                content_id,
                extraction_policy_name,
                policy_completion_time,
            } => {
                self.mark_extraction_policy_applied_on_content(
                    db,
                    &txn,
                    content_id,
                    extraction_policy_name,
                    policy_completion_time,
                )?;
            }
            RequestPayload::CreateNamespace {
                name,
                structured_data_schema,
            } => {
                self.set_namespace(db, &txn, name, structured_data_schema)?;
            }
            RequestPayload::MarkStateChangesProcessed { state_changes } => {
                self.set_processed_state_changes(db, &txn, state_changes)?;
            }
            _ => (),
        };

        txn.commit()
            .map_err(|e| StateMachineError::TransactionError(e.to_string()))?;

        self.apply(request);

        Ok(())
    }

    /// This method handles all reverse index writes which are in memory
    /// This will only run after the RocksDB transaction to commit the forward
    /// index writes is done
    pub fn apply(&mut self, request: StateMachineUpdateRequest) {
        for change in request.new_state_changes {
            self.unprocessed_state_changes.insert(change.id.clone());
        }
        for change in request.state_changes_processed {
            self.mark_state_changes_processed(&change, change.processed_at);
        }
        match request.payload {
            RequestPayload::RegisterExecutor {
                addr,
                executor_id,
                extractor,
                ts_secs,
            } => {
                self.extractor_executors_table
                    .entry(extractor.name.clone())
                    .or_default()
                    .insert(executor_id.clone());
                let _executor_info = internal_api::ExecutorMetadata {
                    id: executor_id.clone(),
                    last_seen: ts_secs,
                    addr: addr.clone(),
                    extractor: extractor.clone(),
                };
                // initialize executor load at 0
                self.executor_running_task_count
                    .insert(executor_id.clone(), 0);
            }
            RequestPayload::RemoveExecutor { executor_id: _ } => (),
            RequestPayload::CreateTasks { tasks } => {
                for task in tasks {
                    self.unassigned_tasks.insert(task.id.clone());
                    self.unfinished_tasks_by_extractor
                        .entry(task.extractor.clone())
                        .or_default()
                        .insert(task.id.clone());
                }
            }
            RequestPayload::AssignTask { assignments } => {
                for (task_id, executor_id) in assignments {
                    self.unassigned_tasks.remove(&task_id);

                    increment_running_task_count(
                        &mut self.executor_running_task_count,
                        &executor_id,
                    );
                }
            }
            RequestPayload::CreateContent { content_metadata } => {
                for content in content_metadata {
                    //  The below write is handled in apply_state_machine_updates
                    self.content_namespace_table
                        .entry(content.namespace.clone())
                        .or_default()
                        .insert(content.id.clone());
                }
            }
            RequestPayload::CreateExtractionPolicy {
                extraction_policy,
                updated_structured_data_schema,
                new_structured_data_schema,
            } => {
                self.extraction_policies_table
                    .entry(extraction_policy.namespace.clone())
                    .or_default()
                    .insert(extraction_policy.id);
                if let Some(schema) = updated_structured_data_schema {
                    self.update_schema_reverse_idx(schema);
                }
                self.update_schema_reverse_idx(new_structured_data_schema);
            }
            RequestPayload::CreateNamespace {
                name: _,
                structured_data_schema,
            } => {
                self.update_schema_reverse_idx(structured_data_schema);
            }
            RequestPayload::CreateIndex {
                index: _,
                namespace,
                id,
            } => {
                self.namespace_index_table
                    .entry(namespace.clone())
                    .or_default()
                    .insert(id);
            }
            RequestPayload::UpdateTask {
                task,
                mark_finished,
                executor_id,
                content_metadata,
            } => {
                if mark_finished {
                    self.unassigned_tasks.remove(&task.id);
                    self.unfinished_tasks_by_extractor
                        .entry(task.extractor.clone())
                        .or_default()
                        .remove(&task.id);
                    if let Some(executor_id) = executor_id {
                        decrement_running_task_count(
                            &mut self.executor_running_task_count,
                            &executor_id,
                        );
                    }
                }
                for content in content_metadata {
                    self.content_namespace_table
                        .entry(content.namespace.clone())
                        .or_default()
                        .insert(content.id.clone());
                }
            }
            RequestPayload::MarkStateChangesProcessed { state_changes } => {
                for state_change in state_changes {
                    self.mark_state_changes_processed(&state_change, state_change.processed_at);
                }
            }
            _ => (),
        }
    }

    pub fn mark_state_changes_processed(
        &mut self,
        state_change: &StateChangeProcessed,
        _processed_at: u64,
    ) {
        self.unprocessed_state_changes
            .remove(&state_change.state_change_id);
    }

    fn update_schema_reverse_idx(&mut self, schema: internal_api::StructuredDataSchema) {
        self.schemas_by_namespace
            .entry(schema.namespace.clone())
            .or_default()
            .insert(schema.id.clone());
    }
}
