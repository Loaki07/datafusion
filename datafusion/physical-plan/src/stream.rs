// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Stream wrappers for physical operators

use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

#[cfg(test)]
use super::metrics::ExecutionPlanMetricsSet;
use super::metrics::{BaselineMetrics, SplitMetrics};
use super::{ExecutionPlan, RecordBatchStream, SendableRecordBatchStream};
use crate::displayable;

use arrow::{datatypes::SchemaRef, record_batch::RecordBatch};
use datafusion_common::{exec_err, Result};
use datafusion_common_runtime::JoinSet;
use datafusion_execution::TaskContext;

use futures::ready;
use futures::stream::BoxStream;
use futures::{Future, Stream, StreamExt};
use log::debug;
use pin_project_lite::pin_project;
use tokio::sync::mpsc::{Receiver, Sender};

/// Creates a stream from a collection of producing tasks, routing panics to the stream.
///
/// Note that this is similar to  [`ReceiverStream` from tokio-stream], with the differences being:
///
/// 1. Methods to bound and "detach"  tasks (`spawn()` and `spawn_blocking()`).
///
/// 2. Propagates panics, whereas the `tokio` version doesn't propagate panics to the receiver.
///
/// 3. Automatically cancels any outstanding tasks when the receiver stream is dropped.
///
/// [`ReceiverStream` from tokio-stream]: https://docs.rs/tokio-stream/latest/tokio_stream/wrappers/struct.ReceiverStream.html
pub(crate) struct ReceiverStreamBuilder<O> {
    tx: Sender<Result<O>>,
    rx: Receiver<Result<O>>,
    join_set: JoinSet<Result<()>>,
}

impl<O: Send + 'static> ReceiverStreamBuilder<O> {
    /// Create new channels with the specified buffer size
    pub fn new(capacity: usize) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(capacity);

        Self {
            tx,
            rx,
            join_set: JoinSet::new(),
        }
    }

    /// Get a handle for sending data to the output
    pub fn tx(&self) -> Sender<Result<O>> {
        self.tx.clone()
    }

    /// Spawn task that will be aborted if this builder (or the stream
    /// built from it) are dropped
    pub fn spawn<F>(&mut self, task: F)
    where
        F: Future<Output = Result<()>>,
        F: Send + 'static,
    {
        self.join_set.spawn(task);
    }

    /// Spawn a blocking task that will be aborted if this builder (or the stream
    /// built from it) are dropped.
    ///
    /// This is often used to spawn tasks that write to the sender
    /// retrieved from `Self::tx`.
    pub fn spawn_blocking<F>(&mut self, f: F)
    where
        F: FnOnce() -> Result<()>,
        F: Send + 'static,
    {
        self.join_set.spawn_blocking(f);
    }

    /// Create a stream of all data written to `tx`
    pub fn build(self) -> BoxStream<'static, Result<O>> {
        let Self {
            tx,
            rx,
            mut join_set,
        } = self;

        // Doesn't need tx
        drop(tx);

        // future that checks the result of the join set, and propagates panic if seen
        let check = async move {
            while let Some(result) = join_set.join_next().await {
                match result {
                    Ok(task_result) => {
                        match task_result {
                            // Nothing to report
                            Ok(_) => continue,
                            // This means a blocking task error
                            Err(error) => return Some(Err(error)),
                        }
                    }
                    // This means a tokio task error, likely a panic
                    Err(e) => {
                        if e.is_panic() {
                            // resume on the main thread
                            std::panic::resume_unwind(e.into_panic());
                        } else {
                            // This should only occur if the task is
                            // cancelled, which would only occur if
                            // the JoinSet were aborted, which in turn
                            // would imply that the receiver has been
                            // dropped and this code is not running
                            return Some(exec_err!("Non Panic Task error: {e}"));
                        }
                    }
                }
            }
            None
        };

        let check_stream = futures::stream::once(check)
            // unwrap Option / only return the error
            .filter_map(|item| async move { item });

        // Convert the receiver into a stream
        let rx_stream = futures::stream::unfold(rx, |mut rx| async move {
            let next_item = rx.recv().await;
            next_item.map(|next_item| (next_item, rx))
        });

        // Merge the streams together so whichever is ready first
        // produces the batch
        futures::stream::select(rx_stream, check_stream).boxed()
    }
}

/// Builder for `RecordBatchReceiverStream` that propagates errors
/// and panic's correctly.
///
/// [`RecordBatchReceiverStreamBuilder`] is used to spawn one or more tasks
/// that produce [`RecordBatch`]es and send them to a single
/// `Receiver` which can improve parallelism.
///
/// This also handles propagating panic`s and canceling the tasks.
///
/// # Example
///
/// The following example spawns 2 tasks that will write [`RecordBatch`]es to
/// the `tx` end of the builder, after building the stream, we can receive
/// those batches with calling `.next()`
///
/// ```
/// # use std::sync::Arc;
/// # use datafusion_common::arrow::datatypes::{Schema, Field, DataType};
/// # use datafusion_common::arrow::array::RecordBatch;
/// # use datafusion_physical_plan::stream::RecordBatchReceiverStreamBuilder;
/// # use futures::stream::StreamExt;
/// # use tokio::runtime::Builder;
/// # let rt = Builder::new_current_thread().build().unwrap();
/// #
/// # rt.block_on(async {
/// let schema = Arc::new(Schema::new(vec![Field::new("foo", DataType::Int8, false)]));
/// let mut builder = RecordBatchReceiverStreamBuilder::new(Arc::clone(&schema), 10);
///
/// // task 1
/// let tx_1 = builder.tx();
/// let schema_1 = Arc::clone(&schema);
/// builder.spawn(async move {
///     // Your task needs to send batches to the tx
///     tx_1.send(Ok(RecordBatch::new_empty(schema_1))).await.unwrap();
///
///     Ok(())
/// });
///
/// // task 2
/// let tx_2 = builder.tx();
/// let schema_2 = Arc::clone(&schema);
/// builder.spawn(async move {
///     // Your task needs to send batches to the tx
///     tx_2.send(Ok(RecordBatch::new_empty(schema_2))).await.unwrap();
///
///     Ok(())
/// });
///
/// let mut stream = builder.build();
/// while let Some(res_batch) = stream.next().await {
///     // `res_batch` can either from task 1 or 2
///
///     // do something with `res_batch`
/// }
/// # });
/// ```
pub struct RecordBatchReceiverStreamBuilder {
    schema: SchemaRef,
    inner: ReceiverStreamBuilder<RecordBatch>,
}

impl RecordBatchReceiverStreamBuilder {
    /// Create new channels with the specified buffer size
    pub fn new(schema: SchemaRef, capacity: usize) -> Self {
        Self {
            schema,
            inner: ReceiverStreamBuilder::new(capacity),
        }
    }

    /// Get a handle for sending [`RecordBatch`] to the output
    ///
    /// If the stream is dropped / canceled, the sender will be closed and
    /// calling `tx().send()` will return an error. Producers should stop
    /// producing in this case and return control.
    pub fn tx(&self) -> Sender<Result<RecordBatch>> {
        self.inner.tx()
    }

    /// Spawn task that will be aborted if this builder (or the stream
    /// built from it) are dropped
    ///
    /// This is often used to spawn tasks that write to the sender
    /// retrieved from [`Self::tx`], for examples, see the document
    /// of this type.
    pub fn spawn<F>(&mut self, task: F)
    where
        F: Future<Output = Result<()>>,
        F: Send + 'static,
    {
        self.inner.spawn(task)
    }

    /// Spawn a blocking task tied to the builder and stream.
    ///
    /// # Drop / Cancel Behavior
    ///
    /// If this builder (or the stream built from it) is dropped **before** the
    /// task starts, the task is also dropped and will never start execute.
    ///
    /// **Note:** Once the blocking task has started, it **will not** be
    /// forcibly stopped on drop as Rust does not allow forcing a running thread
    /// to terminate. The task will continue running until it completes or
    /// encounters an error.
    ///
    /// Users should ensure that their blocking function periodically checks for
    /// errors calling `tx.blocking_send`. An error signals that the stream has
    /// been dropped / cancelled and the blocking task should exit.
    ///
    /// This is often used to spawn tasks that write to the sender
    /// retrieved from [`Self::tx`], for examples, see the document
    /// of this type.
    pub fn spawn_blocking<F>(&mut self, f: F)
    where
        F: FnOnce() -> Result<()>,
        F: Send + 'static,
    {
        self.inner.spawn_blocking(f)
    }

    /// Runs the `partition` of the `input` ExecutionPlan on the
    /// tokio thread pool and writes its outputs to this stream
    ///
    /// If the input partition produces an error, the error will be
    /// sent to the output stream and no further results are sent.
    pub(crate) fn run_input(
        &mut self,
        input: Arc<dyn ExecutionPlan>,
        partition: usize,
        context: Arc<TaskContext>,
    ) {
        let output = self.tx();

        self.inner.spawn(async move {
            let mut stream = match input.execute(partition, context) {
                Err(e) => {
                    // If send fails, the plan being torn down, there
                    // is no place to send the error and no reason to continue.
                    output.send(Err(e)).await.ok();
                    debug!(
                        "Stopping execution: error executing input: {}",
                        displayable(input.as_ref()).one_line()
                    );
                    return Ok(());
                }
                Ok(stream) => stream,
            };

            // Transfer batches from inner stream to the output tx
            // immediately.
            while let Some(item) = stream.next().await {
                let is_err = item.is_err();

                // If send fails, plan being torn down, there is no
                // place to send the error and no reason to continue.
                if output.send(item).await.is_err() {
                    debug!(
                        "Stopping execution: output is gone, plan cancelling: {}",
                        displayable(input.as_ref()).one_line()
                    );
                    return Ok(());
                }

                // Stop after the first error is encountered (Don't
                // drive all streams to completion)
                if is_err {
                    debug!(
                        "Stopping execution: plan returned error: {}",
                        displayable(input.as_ref()).one_line()
                    );
                    return Ok(());
                }
            }

            Ok(())
        });
    }

    /// Create a stream of all [`RecordBatch`] written to `tx`
    pub fn build(self) -> SendableRecordBatchStream {
        Box::pin(RecordBatchStreamAdapter::new(
            self.schema,
            self.inner.build(),
        ))
    }
}

#[doc(hidden)]
pub struct RecordBatchReceiverStream {}

impl RecordBatchReceiverStream {
    /// Create a builder with an internal buffer of capacity batches.
    pub fn builder(
        schema: SchemaRef,
        capacity: usize,
    ) -> RecordBatchReceiverStreamBuilder {
        RecordBatchReceiverStreamBuilder::new(schema, capacity)
    }
}

pin_project! {
    /// Combines a [`Stream`] with a [`SchemaRef`] implementing
    /// [`SendableRecordBatchStream`] for the combination
    ///
    /// See [`Self::new`] for an example
    pub struct RecordBatchStreamAdapter<S> {
        schema: SchemaRef,

        #[pin]
        stream: S,
    }
}

impl<S> RecordBatchStreamAdapter<S> {
    /// Creates a new [`RecordBatchStreamAdapter`] from the provided schema and stream.
    ///
    /// Note to create a [`SendableRecordBatchStream`] you pin the result
    ///
    /// # Example
    /// ```
    /// # use arrow::array::record_batch;
    /// # use datafusion_execution::SendableRecordBatchStream;
    /// # use datafusion_physical_plan::stream::RecordBatchStreamAdapter;
    /// // Create stream of Result<RecordBatch>
    /// let batch = record_batch!(
    ///   ("a", Int32, [1, 2, 3]),
    ///   ("b", Float64, [Some(4.0), None, Some(5.0)])
    /// ).expect("created batch");
    /// let schema = batch.schema();
    /// let stream = futures::stream::iter(vec![Ok(batch)]);
    /// // Convert the stream to a SendableRecordBatchStream
    /// let adapter = RecordBatchStreamAdapter::new(schema, stream);
    /// // Now you can use the adapter as a SendableRecordBatchStream
    /// let batch_stream: SendableRecordBatchStream = Box::pin(adapter);
    /// // ...
    /// ```
    pub fn new(schema: SchemaRef, stream: S) -> Self {
        Self { schema, stream }
    }
}

impl<S> std::fmt::Debug for RecordBatchStreamAdapter<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordBatchStreamAdapter")
            .field("schema", &self.schema)
            .finish()
    }
}

impl<S> Stream for RecordBatchStreamAdapter<S>
where
    S: Stream<Item = Result<RecordBatch>>,
{
    type Item = Result<RecordBatch>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().stream.poll_next(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stream.size_hint()
    }
}

impl<S> RecordBatchStream for RecordBatchStreamAdapter<S>
where
    S: Stream<Item = Result<RecordBatch>>,
{
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }
}

/// `EmptyRecordBatchStream` can be used to create a [`RecordBatchStream`]
/// that will produce no results
pub struct EmptyRecordBatchStream {
    /// Schema wrapped by Arc
    schema: SchemaRef,
}

impl EmptyRecordBatchStream {
    /// Create an empty RecordBatchStream
    pub fn new(schema: SchemaRef) -> Self {
        Self { schema }
    }
}

impl RecordBatchStream for EmptyRecordBatchStream {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }
}

impl Stream for EmptyRecordBatchStream {
    type Item = Result<RecordBatch>;

    fn poll_next(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        Poll::Ready(None)
    }
}

/// Stream wrapper that records `BaselineMetrics` for a particular
/// `[SendableRecordBatchStream]` (likely a partition)
pub(crate) struct ObservedStream {
    inner: SendableRecordBatchStream,
    baseline_metrics: BaselineMetrics,
    fetch: Option<usize>,
    produced: usize,
}

impl ObservedStream {
    pub fn new(
        inner: SendableRecordBatchStream,
        baseline_metrics: BaselineMetrics,
        fetch: Option<usize>,
    ) -> Self {
        Self {
            inner,
            baseline_metrics,
            fetch,
            produced: 0,
        }
    }

    fn limit_reached(
        &mut self,
        poll: Poll<Option<Result<RecordBatch>>>,
    ) -> Poll<Option<Result<RecordBatch>>> {
        let Some(fetch) = self.fetch else { return poll };

        if self.produced >= fetch {
            return Poll::Ready(None);
        }

        if let Poll::Ready(Some(Ok(batch))) = &poll {
            if self.produced + batch.num_rows() > fetch {
                let batch = batch.slice(0, fetch.saturating_sub(self.produced));
                self.produced += batch.num_rows();
                return Poll::Ready(Some(Ok(batch)));
            };
            self.produced += batch.num_rows()
        }
        poll
    }
}

impl RecordBatchStream for ObservedStream {
    fn schema(&self) -> SchemaRef {
        self.inner.schema()
    }
}

impl Stream for ObservedStream {
    type Item = Result<RecordBatch>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let mut poll = self.inner.poll_next_unpin(cx);
        if self.fetch.is_some() {
            poll = self.limit_reached(poll);
        }
        self.baseline_metrics.record_poll(poll)
    }
}

pin_project! {
    /// Stream wrapper that splits large [`RecordBatch`]es into smaller batches.
    ///
    /// This ensures upstream operators receive batches no larger than
    /// `batch_size`, which can improve parallelism when data sources
    /// generate very large batches.
    ///
    /// # Fields
    ///
    /// - `current_batch`: The batch currently being split, if any
    /// - `offset`: Index of the next row to split from `current_batch`.
    ///   This tracks our position within the current batch being split.
    ///
    /// # Invariants
    ///
    /// - `offset` is always ≤ `current_batch.num_rows()` when `current_batch` is `Some`
    /// - When `current_batch` is `None`, `offset` is always 0
    /// - `batch_size` is always > 0
pub struct BatchSplitStream {
        #[pin]
        input: SendableRecordBatchStream,
        schema: SchemaRef,
        batch_size: usize,
        metrics: SplitMetrics,
        current_batch: Option<RecordBatch>,
        offset: usize,
    }
}

impl BatchSplitStream {
    /// Create a new [`BatchSplitStream`]
    pub fn new(
        input: SendableRecordBatchStream,
        batch_size: usize,
        metrics: SplitMetrics,
    ) -> Self {
        let schema = input.schema();
        Self {
            input,
            schema,
            batch_size,
            metrics,
            current_batch: None,
            offset: 0,
        }
    }

    /// Attempt to produce the next sliced batch from the current batch.
    ///
    /// Returns `Some(batch)` if a slice was produced, `None` if the current batch
    /// is exhausted and we need to poll upstream for more data.
    fn next_sliced_batch(&mut self) -> Option<Result<RecordBatch>> {
        let batch = self.current_batch.take()?;

        // Assert slice boundary safety - offset should never exceed batch size
        debug_assert!(
            self.offset <= batch.num_rows(),
            "Offset {} exceeds batch size {}",
            self.offset,
            batch.num_rows()
        );

        let remaining = batch.num_rows() - self.offset;
        let to_take = remaining.min(self.batch_size);
        let out = batch.slice(self.offset, to_take);

        self.metrics.batches_splitted.add(1);
        self.offset += to_take;
        if self.offset < batch.num_rows() {
            // More data remains in this batch, store it back
            self.current_batch = Some(batch);
        } else {
            // Batch is exhausted, reset offset
            // Note: current_batch is already None since we took it at the start
            self.offset = 0;
        }
        Some(Ok(out))
    }

    /// Poll the upstream input for the next batch.
    ///
    /// Returns the appropriate `Poll` result based on upstream state.
    /// Small batches are passed through directly, large batches are stored
    /// for slicing and return the first slice immediately.
    fn poll_upstream(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<RecordBatch>>> {
        match ready!(self.input.as_mut().poll_next(cx)) {
            Some(Ok(batch)) => {
                if batch.num_rows() <= self.batch_size {
                    // Small batch, pass through directly
                    Poll::Ready(Some(Ok(batch)))
                } else {
                    // Large batch, store for slicing and return first slice
                    self.current_batch = Some(batch);
                    // Immediately produce the first slice
                    match self.next_sliced_batch() {
                        Some(result) => Poll::Ready(Some(result)),
                        None => Poll::Ready(None), // Should not happen
                    }
                }
            }
            Some(Err(e)) => Poll::Ready(Some(Err(e))),
            None => Poll::Ready(None),
        }
    }
}

impl Stream for BatchSplitStream {
    type Item = Result<RecordBatch>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        // First, try to produce a slice from the current batch
        if let Some(result) = self.next_sliced_batch() {
            return Poll::Ready(Some(result));
        }

        // No current batch or current batch exhausted, poll upstream
        self.poll_upstream(cx)
    }
}

impl RecordBatchStream for BatchSplitStream {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test::exec::{
        assert_strong_count_converges_to_zero, BlockingExec, MockExec, PanicExec,
    };

    use arrow::datatypes::{DataType, Field, Schema};
    use datafusion_common::exec_err;

    fn schema() -> SchemaRef {
        Arc::new(Schema::new(vec![Field::new("a", DataType::Float32, true)]))
    }

    #[tokio::test]
    #[should_panic(expected = "PanickingStream did panic")]
    async fn record_batch_receiver_stream_propagates_panics() {
        let schema = schema();

        let num_partitions = 10;
        let input = PanicExec::new(Arc::clone(&schema), num_partitions);
        consume(input, 10).await
    }

    #[tokio::test]
    #[should_panic(expected = "PanickingStream did panic: 1")]
    async fn record_batch_receiver_stream_propagates_panics_early_shutdown() {
        let schema = schema();

        // Make 2 partitions, second partition panics before the first
        let num_partitions = 2;
        let input = PanicExec::new(Arc::clone(&schema), num_partitions)
            .with_partition_panic(0, 10)
            .with_partition_panic(1, 3); // partition 1 should panic first (after 3 )

        // Ensure that the panic results in an early shutdown (that
        // everything stops after the first panic).

        // Since the stream reads every other batch: (0,1,0,1,0,panic)
        // so should not exceed 5 batches prior to the panic
        let max_batches = 5;
        consume(input, max_batches).await
    }

    #[tokio::test]
    async fn record_batch_receiver_stream_drop_cancel() {
        let task_ctx = Arc::new(TaskContext::default());
        let schema = schema();

        // Make an input that never proceeds
        let input = BlockingExec::new(Arc::clone(&schema), 1);
        let refs = input.refs();

        // Configure a RecordBatchReceiverStream to consume the input
        let mut builder = RecordBatchReceiverStream::builder(schema, 2);
        builder.run_input(Arc::new(input), 0, Arc::clone(&task_ctx));
        let stream = builder.build();

        // Input should still be present
        assert!(std::sync::Weak::strong_count(&refs) > 0);

        // Drop the stream, ensure the refs go to zero
        drop(stream);
        assert_strong_count_converges_to_zero(refs).await;
    }

    #[tokio::test]
    /// Ensure that if an error is received in one stream, the
    /// `RecordBatchReceiverStream` stops early and does not drive
    /// other streams to completion.
    async fn record_batch_receiver_stream_error_does_not_drive_completion() {
        let task_ctx = Arc::new(TaskContext::default());
        let schema = schema();

        // make an input that will error twice
        let error_stream = MockExec::new(
            vec![exec_err!("Test1"), exec_err!("Test2")],
            Arc::clone(&schema),
        )
        .with_use_task(false);

        let mut builder = RecordBatchReceiverStream::builder(schema, 2);
        builder.run_input(Arc::new(error_stream), 0, Arc::clone(&task_ctx));
        let mut stream = builder.build();

        // Get the first result, which should be an error
        let first_batch = stream.next().await.unwrap();
        let first_err = first_batch.unwrap_err();
        assert_eq!(first_err.strip_backtrace(), "Execution error: Test1");

        // There should be no more batches produced (should not get the second error)
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn batch_split_stream_basic_functionality() {
        use arrow::array::{Int32Array, RecordBatch};
        use futures::stream::{self, StreamExt};

        let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Int32, false)]));

        // Create a large batch that should be split
        let large_batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![Arc::new(Int32Array::from((0..2000).collect::<Vec<_>>()))],
        )
        .unwrap();

        // Create a stream with the large batch
        let input_stream = stream::iter(vec![Ok(large_batch)]);
        let adapter = RecordBatchStreamAdapter::new(Arc::clone(&schema), input_stream);
        let batch_stream = Box::pin(adapter) as SendableRecordBatchStream;

        // Create a BatchSplitStream with batch_size = 500
        let metrics = ExecutionPlanMetricsSet::new();
        let split_metrics = SplitMetrics::new(&metrics, 0);
        let mut split_stream = BatchSplitStream::new(batch_stream, 500, split_metrics);

        let mut total_rows = 0;
        let mut batch_count = 0;

        while let Some(result) = split_stream.next().await {
            let batch = result.unwrap();
            assert!(batch.num_rows() <= 500, "Batch size should not exceed 500");
            total_rows += batch.num_rows();
            batch_count += 1;
        }

        assert_eq!(total_rows, 2000, "All rows should be preserved");
        assert_eq!(batch_count, 4, "Should have 4 batches of 500 rows each");
    }

    /// Consumes all the input's partitions into a
    /// RecordBatchReceiverStream and runs it to completion
    ///
    /// panic's if more than max_batches is seen,
    async fn consume(input: PanicExec, max_batches: usize) {
        let task_ctx = Arc::new(TaskContext::default());

        let input = Arc::new(input);
        let num_partitions = input.properties().output_partitioning().partition_count();

        // Configure a RecordBatchReceiverStream to consume all the input partitions
        let mut builder =
            RecordBatchReceiverStream::builder(input.schema(), num_partitions);
        for partition in 0..num_partitions {
            builder.run_input(
                Arc::clone(&input) as Arc<dyn ExecutionPlan>,
                partition,
                Arc::clone(&task_ctx),
            );
        }
        let mut stream = builder.build();

        // Drain the stream until it is complete, panic'ing on error
        let mut num_batches = 0;
        while let Some(next) = stream.next().await {
            next.unwrap();
            num_batches += 1;
            assert!(
                num_batches < max_batches,
                "Got the limit of {num_batches} batches before seeing panic"
            );
        }
    }
}
