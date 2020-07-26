// Copyright 2020 Andy Grove
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The scheduler is responsible for breaking a physical query plan down into stages and tasks
//! and co-ordinating execution of these stages and tasks across the cluster.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;
use uuid::Uuid;

use crate::dataframe::{avg, count, max, min, sum};
use crate::datafusion::execution::physical_plan::csv::CsvReadOptions;
use crate::datafusion::logicalplan::LogicalPlan;
use crate::datafusion::logicalplan::{col_index, Expr};
use crate::distributed::executor::DefaultContext;
use crate::error::{ballista_error, BallistaError, Result};
use crate::execution::operators::ProjectionExec;
use crate::execution::operators::ShuffleExchangeExec;
use crate::execution::operators::ShuffleReaderExec;
use crate::execution::operators::{CsvScanExec, HashAggregateExec};
use crate::execution::operators::{FilterExec, ParquetScanExec};
use crate::execution::physical_plan::{
    AggregateMode, ColumnarBatch, Distribution, ExecutionContext, ExecutionPlan, ExecutorMeta,
    Partitioning, PhysicalPlan, ShuffleId,
};

use smol::Task;

/// A Job typically represents a single query and the query is executed in stages. Stages are
/// separated by map operations (shuffles) to re-partition data before the next stage starts.
#[derive(Debug)]
pub struct Job {
    /// Job UUID
    pub id: Uuid,
    /// A list of stages within this job. There can be dependencies between stages to form
    /// a directed acyclic graph (DAG).
    pub stages: Vec<Rc<RefCell<Stage>>>,
    /// The root stage id that produces the final results
    pub root_stage_id: usize,
}

impl Job {
    pub fn explain(&self) {
        println!("Job {} has {} stages:\n", self.id, self.stages.len());
        self.stages.iter().for_each(|stage| {
            let stage = stage.as_ref().borrow();
            println!("Stage {}:\n", stage.id);
            if stage.prior_stages.is_empty() {
                println!("Stage {} has no dependencies.", stage.id);
            } else {
                println!(
                    "Stage {} depends on stages {:?}.",
                    stage.id, stage.prior_stages
                );
            }
            println!(
                "\n{:?}\n",
                stage
                    .plan
                    .as_ref()
                    .expect("Stages should always have a plan")
            );
        })
    }
}

/// A query stage consists of tasks. Typically, tasks map to partitions.
#[derive(Debug)]
pub struct Stage {
    /// Stage id which is unique within a job.
    pub id: usize,
    /// A list of stages that must complete before this stage can execute.
    pub prior_stages: Vec<usize>,
    /// The physical plan to execute for this stage
    pub plan: Option<Arc<PhysicalPlan>>,
}

impl Stage {
    /// Create a new empty stage with the specified id.
    fn new(id: usize) -> Self {
        Self {
            id,
            prior_stages: vec![],
            plan: None,
        }
    }
}

/// Task that can be sent to an executor for execution
#[derive(Debug, Clone)]
pub struct ExecutionTask {
    pub(crate) job_uuid: Uuid,
    pub(crate) stage_id: usize,
    pub(crate) partition_id: usize,
    pub(crate) plan: PhysicalPlan,
    pub(crate) shuffle_locations: HashMap<ShuffleId, ExecutorMeta>,
}

impl ExecutionTask {
    pub fn new(
        job_uuid: Uuid,
        stage_id: usize,
        partition_id: usize,
        plan: PhysicalPlan,
        shuffle_locations: HashMap<ShuffleId, ExecutorMeta>,
    ) -> Self {
        Self {
            job_uuid,
            stage_id,
            partition_id,
            plan,
            shuffle_locations,
        }
    }
}

/// Create a Job (DAG of stages) from a physical execution plan.
pub fn create_job(plan: Arc<PhysicalPlan>) -> Result<Job> {
    let mut scheduler = Scheduler::new();
    scheduler.create_job(plan)?;
    Ok(scheduler.job)
}

pub struct Scheduler {
    job: Job,
    next_stage_id: usize,
}

impl Scheduler {
    fn new() -> Self {
        let job = Job {
            id: Uuid::new_v4(),
            stages: vec![],
            root_stage_id: 0,
        };
        Self {
            job,
            next_stage_id: 0,
        }
    }

    fn create_job(&mut self, plan: Arc<PhysicalPlan>) -> Result<()> {
        let new_stage_id = self.next_stage_id;
        self.next_stage_id += 1;
        let new_stage = Rc::new(RefCell::new(Stage::new(new_stage_id)));
        self.job.stages.push(new_stage.clone());
        let plan = self.visit_plan(plan, new_stage.clone())?;
        new_stage.as_ref().borrow_mut().plan = Some(plan);
        Ok(())
    }

    fn visit_plan(
        &mut self,
        plan: Arc<PhysicalPlan>,
        current_stage: Rc<RefCell<Stage>>,
    ) -> Result<Arc<PhysicalPlan>> {
        //
        match plan.as_ref() {
            PhysicalPlan::ShuffleExchange(exec) => {
                // shuffle indicates that we need a new stage
                let new_stage_id = self.next_stage_id;
                self.next_stage_id += 1;
                let new_stage = Rc::new(RefCell::new(Stage::new(new_stage_id)));
                self.job.stages.push(new_stage.clone());

                // the children need to be part of this new stage
                let shuffle_input = self.visit_plan(exec.child.clone(), new_stage.clone())?;

                new_stage.as_ref().borrow_mut().plan = Some(shuffle_input);

                // the current stage depends on this new stage
                current_stage
                    .as_ref()
                    .borrow_mut()
                    .prior_stages
                    .push(new_stage_id);

                // return a shuffle reader to read the results from the stage
                let n = exec
                    .child
                    .as_execution_plan()
                    .output_partitioning()
                    .partition_count();

                let shuffle_id = (0..n)
                    .map(|n| ShuffleId {
                        job_uuid: self.job.id,
                        stage_id: new_stage_id,
                        partition_id: n,
                    })
                    .collect();
                Ok(Arc::new(PhysicalPlan::ShuffleReader(Arc::new(
                    ShuffleReaderExec::new(exec.schema(), shuffle_id),
                ))))
            }
            PhysicalPlan::HashAggregate(exec) => {
                let child = self.visit_plan(exec.child.clone(), current_stage)?;
                Ok(Arc::new(PhysicalPlan::HashAggregate(Arc::new(
                    exec.with_new_children(vec![child]),
                ))))
            }
            PhysicalPlan::Filter(exec) => {
                let child = self.visit_plan(exec.child.clone(), current_stage)?;
                Ok(Arc::new(PhysicalPlan::Filter(Arc::new(
                    exec.with_new_children(vec![child]),
                ))))
            }
            PhysicalPlan::CsvScan(_) => Ok(plan.clone()),
            PhysicalPlan::ParquetScan(_) => Ok(plan.clone()),
            _ => Err(ballista_error("visit_plan unsupported operator")),
        }
    }
}

enum StageStatus {
    Pending,
    Completed,
}

/// Execute a job directly against executors as starting point
pub async fn execute_job(job: &Job, ctx: Arc<dyn ExecutionContext>) -> Result<Vec<ColumnarBatch>> {
    let executors = ctx.get_executor_ids().await?;

    println!("Executors: {:?}", executors);

    if executors.is_empty() {
        println!("no executors found");
        return Err(ballista_error("no executors available"));
    }

    let mut shuffle_location_map: HashMap<ShuffleId, ExecutorMeta> = HashMap::new();

    let mut stage_status_map = HashMap::new();

    for stage in &job.stages {
        let stage = stage.borrow_mut();
        stage_status_map.insert(stage.id, StageStatus::Pending);
    }

    // loop until all stages are complete
    let mut num_completed = 0;
    while num_completed < job.stages.len() {
        num_completed = 0;

        //TODO do stages in parallel when possible
        for stage in &job.stages {
            let stage = stage.borrow_mut();
            let status = stage_status_map.get(&stage.id).unwrap();
            match status {
                StageStatus::Pending => {
                    // have prior stages already completed ?
                    if stage
                        .prior_stages
                        .iter()
                        .all(|id| match stage_status_map.get(id) {
                            Some(StageStatus::Completed) => true,
                            _ => false,
                        })
                    {
                        println!("Running stage {}", stage.id);
                        let plan = stage
                            .plan
                            .as_ref()
                            .expect("all stages should have plans at execution time");

                        let exec = plan.as_execution_plan();
                        let parts = exec.output_partitioning().partition_count();

                        let mut threads = vec![];
                        let mut executors_ids = vec![];
                        let mut executor_index = 0;
                        for partition in 0..parts {
                            println!("Running stage {} partition {}", stage.id, partition);
                            let task = ExecutionTask::new(
                                job.id,
                                stage.id,
                                partition,
                                plan.as_ref().clone(),
                                shuffle_location_map.clone(),
                            );

                            // load balance across the executors
                            let executor_id = &executors[executor_index];
                            executor_index += 1;
                            if executor_index == executors.len() {
                                executor_index = 0;
                            }

                            let executor_id = executor_id.clone();
                            executors_ids.push(executor_id.clone());

                            let ctx = ctx.clone();
                            let handle = thread::spawn(move || {
                                println!("in thread");
                                smol::run(async {
                                    println!("in smol::run");
                                    Task::spawn(async move {
                                        println!("in Task::blocking");
                                        ctx.execute_task(executor_id.clone(), task).await
                                    })
                                    .await
                                })
                            });
                            threads.push(handle);
                        }

                        let mut shuffle_ids = vec![];
                        for thread in threads {
                            println!("Waiting to join thread");
                            let shuffle_id = thread.join().unwrap()?;
                            shuffle_ids.push(shuffle_id);
                        }
                        println!("Joined all threads");

                        for (shuffle_id, executor_id) in shuffle_ids.iter().zip(executors_ids) {
                            shuffle_location_map.insert(*shuffle_id, executor_id);
                        }
                        stage_status_map.insert(stage.id, StageStatus::Completed);

                        if stage.id == job.root_stage_id {
                            println!("reading final results from query!");
                            let ctx = Arc::new(DefaultContext::new(
                                &ctx.config(),
                                shuffle_location_map.clone(),
                            ));
                            let data = ctx.read_shuffle(&shuffle_ids[0]).await?;
                            return Ok(data);
                        }
                    } else {
                        println!("Cannot run stage {} yet", stage.id);
                    }
                }
                StageStatus::Completed => {
                    num_completed += 1;
                }
            }
        }
    }

    Err(ballista_error("oops"))
}

/// Convert a logical plan into a physical plan
pub fn create_physical_plan(plan: &LogicalPlan) -> Result<Arc<PhysicalPlan>> {
    match plan {
        LogicalPlan::Projection { input, expr, .. } => {
            let exec = ProjectionExec::try_new(expr, create_physical_plan(input)?)?;
            Ok(Arc::new(PhysicalPlan::Projection(Arc::new(exec))))
        }
        LogicalPlan::Selection { input, expr, .. } => {
            let input = create_physical_plan(input)?;
            let exec = FilterExec::new(&input, expr);
            Ok(Arc::new(PhysicalPlan::Filter(Arc::new(exec))))
        }
        LogicalPlan::Aggregate {
            input,
            group_expr,
            aggr_expr,
            ..
        } => {
            let input = create_physical_plan(input)?;
            if input
                .as_execution_plan()
                .output_partitioning()
                .partition_count()
                == 1
            {
                let exec = HashAggregateExec::try_new(
                    AggregateMode::Complete,
                    group_expr.clone(),
                    aggr_expr.clone(),
                    input,
                )?;
                Ok(Arc::new(PhysicalPlan::HashAggregate(Arc::new(exec))))
            } else {
                // Create partial hash aggregate to run against partitions in parallel
                let partial_hash_exec = HashAggregateExec::try_new(
                    AggregateMode::Partial,
                    group_expr.clone(),
                    aggr_expr.clone(),
                    input,
                )?;
                let partial = Arc::new(PhysicalPlan::HashAggregate(Arc::new(partial_hash_exec)));

                // Create final hash aggregate to run on the coalesced partition of the results
                // from the partial hash aggregate

                let mut final_group = vec![];
                for i in 0..group_expr.len() {
                    final_group.push(col_index(i as usize));
                }

                //TODO this is ugly and shows that the design needs revisiting here

                let mut final_aggr = vec![];
                for (i, expr) in aggr_expr.iter().enumerate() {
                    let j = group_expr.len() + i;
                    match expr {
                        Expr::AggregateFunction { name, .. } => {
                            let expr = match name.as_str() {
                                "SUM" => sum(col_index(j)),
                                "MIN" => min(col_index(j)),
                                "MAX" => max(col_index(j)),
                                "AVG" => avg(col_index(j)),
                                "COUNT" => count(col_index(j)),
                                _ => panic!(),
                            };
                            final_aggr.push(expr);
                        }
                        Expr::Alias(expr, alias) => match expr.as_ref() {
                            Expr::AggregateFunction { name, .. } => {
                                let expr = match name.as_str() {
                                    "SUM" => sum(col_index(j)).alias(alias),
                                    "MIN" => min(col_index(j)).alias(alias),
                                    "MAX" => max(col_index(j)).alias(alias),
                                    "AVG" => avg(col_index(j)).alias(alias),
                                    "COUNT" => count(col_index(j)).alias(alias),
                                    _ => panic!(),
                                };
                                final_aggr.push(expr);
                            }
                            _ => panic!(),
                        },
                        _ => panic!(),
                    }
                }

                let final_hash_exec = HashAggregateExec::try_new(
                    AggregateMode::Final,
                    final_group,
                    final_aggr,
                    partial,
                )?;
                Ok(Arc::new(PhysicalPlan::HashAggregate(Arc::new(
                    final_hash_exec,
                ))))
            }
        }
        LogicalPlan::CsvScan {
            path, projection, ..
        } => {
            //TODO make batch size and other csv options configurable from the context
            let batch_size = 64 * 1024;
            let options = CsvReadOptions::new();
            let exec = CsvScanExec::try_new(&path, options, projection.clone(), batch_size)?;
            Ok(Arc::new(PhysicalPlan::CsvScan(Arc::new(exec))))
        }
        LogicalPlan::ParquetScan {
            path, projection, ..
        } => {
            //TODO make batch size configurable from the context
            let batch_size = 1024 * 1024;
            let exec = ParquetScanExec::try_new(&path, projection.clone(), batch_size)?;
            Ok(Arc::new(PhysicalPlan::ParquetScan(Arc::new(exec))))
        }
        other => Err(BallistaError::General(format!(
            "create_physical_plan unsupported operator {:?}",
            other
        ))),
    }
}

/// Optimizer rule to insert shuffles as needed
pub fn ensure_requirements(plan: &PhysicalPlan) -> Result<Arc<PhysicalPlan>> {
    let execution_plan = plan.as_execution_plan();

    // recurse down and replace children
    if execution_plan.children().is_empty() {
        return Ok(Arc::new(plan.clone()));
    }
    let children: Vec<Arc<PhysicalPlan>> = execution_plan
        .children()
        .iter()
        .map(|c| ensure_requirements(c.as_ref()))
        .collect::<Result<Vec<_>>>()?;

    match execution_plan.required_child_distribution() {
        Distribution::SinglePartition => {
            let new_children: Vec<Arc<PhysicalPlan>> = children
                .iter()
                .map(|c| {
                    if c.as_execution_plan()
                        .output_partitioning()
                        .partition_count()
                        > 1
                    {
                        Arc::new(PhysicalPlan::ShuffleExchange(Arc::new(
                            ShuffleExchangeExec::new(
                                c.clone(),
                                Partitioning::UnknownPartitioning(1),
                            ),
                        )))
                    } else {
                        c.clone()
                    }
                })
                .collect();

            Ok(Arc::new(plan.with_new_children(new_children)))
        }
        _ => Ok(Arc::new(plan.clone())),
    }
}

// #[cfg(test)]
// mod tests {
//     use super::*;
//     use crate::dataframe::{col, sum, Context};
//     use std::collections::HashMap;
//
//     #[test]
//     fn create_plan() -> Result<()> {
//         let ctx = Context::local(HashMap::new());
//
//         let df = ctx
//             .read_parquet("/mnt/nyctaxi/parquet/year=2019", None)?
//             .aggregate(vec![col("passenger_count")], vec![sum(col("fare_amount"))])?;
//
//         let plan = df.logical_plan();
//
//         let plan = create_physical_plan(&plan)?;
//         let plan = ensure_requirements(&plan)?;
//         println!("Optimized physical plan:\n{:?}", plan);
//
//         let mut scheduler = Scheduler::new();
//         scheduler.create_job(plan)?;
//
//         scheduler.job.explain();
//         Ok(())
//     }
// }
